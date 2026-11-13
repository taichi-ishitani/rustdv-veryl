//! Constrained randomization: SystemVerilog's `rand`, `constraint` and
//! `randomize()` on Rust structs.
//!
//! See `docs/random.md` for the user guide and `docs/random-design.md`
//! for the implementation.
//!
//! ```ignore
//! use rustdv_veryl::random::{RandEnum, constraints, randomize, randomize_with};
//!
//! #[derive(Clone, Copy, PartialEq, Eq, Default, RandEnum)]
//! enum Op { #[default] Add, Sub, Mul }
//!
//! #[randomize]
//! #[derive(Clone, Default)]
//! struct AluCmd {
//!     #[rand] op: Op,
//!     #[rand] a: u8,
//!     limit: u8,
//! }
//!
//! #[constraints]
//! impl AluCmd {
//!     fn small_mul() {
//!         if op == Op::Mul { a < limit; }
//!     }
//! }
//!
//! let mut cmd = AluCmd { limit: 8, ..Default::default() };
//! cmd.randomize()?;
//! ```

mod blast;
mod builder;
mod ir;
mod rng;
mod sat;
mod solve;
mod value;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use num_bigint::BigInt;

pub use rustdv_veryl_macros::{
    RandEnum, constraint_fn, constraints, randcase, randomize, randomize_with, std_randomize,
};
pub use sat::{Lit, SatSolver, SolveResult, Var};
pub use value::RandValue;

use builder::{Builder, Operand, Phase, Solution};
use rng::SplitMix64;

/// The name of a constraint block: `AluCmd::SMALL_MUL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConstraintName {
    owner: &'static str,
    name: &'static str,
}

impl ConstraintName {
    #[doc(hidden)]
    pub const fn new(owner: &'static str, name: &'static str) -> ConstraintName {
        ConstraintName { owner, name }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }
}

impl fmt::Display for ConstraintName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}::{}", self.owner, self.name)
    }
}

/// The name of a random field: `AluCmd::A`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FieldName(&'static str);

impl FieldName {
    #[doc(hidden)]
    pub const fn new(name: &'static str) -> FieldName {
        FieldName(name)
    }

    pub fn name(&self) -> &'static str {
        self.0
    }
}

// ===========================================================================
// Errors
// ===========================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RandErrorKind {
    /// The constraints cannot be satisfied together.
    Unsatisfiable,
    /// The solver ran out of time.
    Timeout,
    /// The constraints could not be built (an index outside an array, a
    /// `Vec` without a bound on its length, a field that cannot be used).
    Invalid,
}

/// Why `randomize()` failed. Its `Display` is the report.
#[derive(Clone, Debug)]
pub struct RandError {
    kind: RandErrorKind,
    report: String,
}

impl RandError {
    pub fn kind(&self) -> RandErrorKind {
        self.kind
    }

    pub fn report(&self) -> &str {
        &self.report
    }

    fn header(p: &solve::Problem) -> String {
        let mut s = format!("randomize() failed: {}\n", p.owner);
        let vars: Vec<&str> = p
            .vars
            .iter()
            .map(|v| v.name.as_str())
            .filter(|n| !n.is_empty())
            .collect();
        s += &format!("  rand variables: {}\n", vars.join(", "));
        if !p.rand_off.is_empty() {
            s += &format!("  rand off: {}\n", p.rand_off.join(", "));
        }
        s += &format!("  constraints: {}\n", p.constraints.join(", "));
        if !p.constraints_off.is_empty() {
            s += &format!("  constraint off: {}\n", p.constraints_off.join(", "));
        }
        s
    }

    pub(crate) fn unsat(p: &solve::Problem, core: &[usize]) -> RandError {
        let mut s = Self::header(p);
        s += "  unsatisfied:\n";
        let mut seen = Vec::new();
        for o in core {
            if seen.contains(o) {
                continue;
            }
            seen.push(*o);
            let o = &p.origins[*o];
            if o.loc.is_empty() {
                s += &format!("    {}: {}\n", o.block, o.text);
            } else {
                s += &format!("    {} ({}): {}\n", o.block, o.loc, o.text);
            }
        }
        RandError {
            kind: RandErrorKind::Unsatisfiable,
            report: s.trim_end().to_string(),
        }
    }

    pub(crate) fn timeout(p: &solve::Problem) -> RandError {
        let s = Self::header(p) + "  the solver ran out of time";
        RandError {
            kind: RandErrorKind::Timeout,
            report: s,
        }
    }

    pub(crate) fn build(owner: &str, msg: String) -> RandError {
        RandError {
            kind: RandErrorKind::Invalid,
            report: format!("randomize() failed: {owner}\n  {msg}"),
        }
    }
}

impl fmt::Display for RandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.report)
    }
}

impl std::error::Error for RandError {}

impl From<RandError> for rustdv::prelude::TestError {
    fn from(e: RandError) -> Self {
        rustdv::prelude::TestError::new(e.to_string())
    }
}

impl From<RandError> for rustdv::prelude::SeqError {
    fn from(e: RandError) -> Self {
        rustdv::prelude::SeqError(e.to_string())
    }
}

// ===========================================================================
// Seeds, solver, timeout
// ===========================================================================

thread_local! {
    static SEED_SOURCE: RefCell<Option<SplitMix64>> = const { RefCell::new(None) };
    static TIMEOUT: Cell<Option<Duration>> = const { Cell::new(Some(solve::DEFAULT_TIMEOUT)) };
    static FACTORY: Cell<Option<SolverFactory>> = const { Cell::new(None) };
}

/// Makes a SAT solver from a seed.
pub type SolverFactory = fn(u64) -> Box<dyn SatSolver>;

/// Seeds this thread's seed source, from which objects take their seeds
/// on their first `randomize()`. Call it at the start of a test with the
/// test's seed (`ctx.seed()`) to make the test reproducible on its own;
/// otherwise the source starts from `RUSTDV_RANDOM_SEED` (or 1).
pub fn set_seed(seed: u64) {
    SEED_SOURCE.with(|s| *s.borrow_mut() = Some(SplitMix64::new(seed)));
}

fn take_seed() -> u64 {
    SEED_SOURCE.with(|s| {
        let mut s = s.borrow_mut();
        let src = s.get_or_insert_with(|| {
            let seed = std::env::var("RUSTDV_RANDOM_SEED")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            SplitMix64::new(seed)
        });
        src.next_u64()
    })
}

fn with_thread_rng<R>(f: impl FnOnce(&mut SplitMix64) -> R) -> R {
    let mut rng = SplitMix64::new(take_seed());
    f(&mut rng)
}

/// The time limit of one `randomize()` on this thread (`None`: no limit).
pub fn set_timeout(t: Option<Duration>) {
    TIMEOUT.with(|c| c.set(t));
}

/// Replaces the SAT solver on this thread; the function gets a seed.
pub fn set_solver_factory(f: SolverFactory) {
    FACTORY.with(|c| c.set(Some(f)));
}

fn new_solver(seed: u64) -> Box<dyn SatSolver> {
    if let Some(f) = FACTORY.with(Cell::get) {
        return f(seed);
    }
    #[cfg(feature = "batsat")]
    {
        Box::new(sat::batsat_impl::Batsat::new(seed))
    }
    #[cfg(not(feature = "batsat"))]
    {
        panic!("no SAT solver: enable the `batsat` feature or call set_solver_factory")
    }
}

/// SV `$urandom`.
pub fn urandom() -> u32 {
    with_thread_rng(|r| r.next_u64() as u32)
}

/// SV `$urandom_range`: a value in `lo..=hi` (the bounds may be in
/// either order).
pub fn urandom_range(lo: u32, hi: u32) -> u32 {
    let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
    with_thread_rng(|r| lo + r.below(hi as u64 - lo as u64 + 1) as u32)
}

/// The bitwise complement at the width of the value (`~a` of SV), also
/// usable in constraints.
pub fn bit_not<T: std::ops::Not<Output = T>>(a: T) -> T {
    !a
}

// ===========================================================================
// Objects
// ===========================================================================

/// The hidden per-object state that `#[randomize]` adds.
pub struct RandState<T: ?Sized> {
    rng: Option<SplitMix64>,
    rand_off: Vec<&'static str>,
    all_off: bool,
    off: Vec<ConstraintName>,
    sets: Vec<ConstraintSet<T>>,
}

impl<T: ?Sized> Default for RandState<T> {
    fn default() -> Self {
        RandState {
            rng: None,
            rand_off: Vec::new(),
            all_off: false,
            off: Vec::new(),
            sets: Vec::new(),
        }
    }
}

impl<T: ?Sized> Clone for RandState<T> {
    fn clone(&self) -> Self {
        RandState {
            rng: self.rng.clone(),
            rand_off: self.rand_off.clone(),
            all_off: self.all_off,
            off: self.off.clone(),
            sets: self.sets.clone(),
        }
    }
}

impl<T: ?Sized> fmt::Debug for RandState<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RandState")
    }
}

/// Random state does not take part in comparisons.
impl<T: ?Sized> PartialEq for RandState<T> {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl<T: ?Sized> Eq for RandState<T> {}

impl<T: ?Sized> std::hash::Hash for RandState<T> {
    fn hash<H: std::hash::Hasher>(&self, _: &mut H) {}
}

impl<T: ?Sized> RandState<T> {
    #[doc(hidden)]
    pub fn is_rand_on(&self, name: &str) -> bool {
        !self.all_off && !self.rand_off.contains(&name)
    }
}

/// A group of constraints for `T`, defined with `#[constraints(for T)]`.
pub trait Constraints<T: ?Sized>: Send + Sync + 'static {
    #[doc(hidden)]
    fn __blocks(&self, b: &mut Builder);
    #[doc(hidden)]
    fn __replaces(&self, _out: &mut Vec<ConstraintName>) {}
}

/// A shared constraint set, as kept by an object.
pub struct ConstraintSet<T: ?Sized>(Arc<dyn Constraints<T>>);

impl<T: ?Sized> ConstraintSet<T> {
    pub fn new(c: impl Constraints<T>) -> Self {
        ConstraintSet(Arc::new(c))
    }
}

impl<T: ?Sized> Clone for ConstraintSet<T> {
    fn clone(&self) -> Self {
        ConstraintSet(self.0.clone())
    }
}

impl<T: ?Sized + 'static> Constraints<T> for ConstraintSet<T> {
    fn __blocks(&self, b: &mut Builder) {
        self.0.__blocks(b)
    }
    fn __replaces(&self, out: &mut Vec<ConstraintName>) {
        self.0.__replaces(out)
    }
}

impl<T: ?Sized> Constraints<T> for () {
    fn __blocks(&self, _b: &mut Builder) {}
}

macro_rules! tuple_constraints {
    ($($n:ident),*) => {
        impl<T: ?Sized, $($n: Constraints<T>),*> Constraints<T> for ($($n,)*) {
            #[allow(non_snake_case)]
            fn __blocks(&self, b: &mut Builder) {
                let ($($n,)*) = self;
                $($n.__blocks(b);)*
            }
            #[allow(non_snake_case)]
            fn __replaces(&self, out: &mut Vec<ConstraintName>) {
                let ($($n,)*) = self;
                $($n.__replaces(out);)*
            }
        }
    };
}

tuple_constraints!(A);
tuple_constraints!(A, B);
tuple_constraints!(A, B, C);
tuple_constraints!(A, B, C, D);
tuple_constraints!(A, B, C, D, E);
tuple_constraints!(A, B, C, D, E, F);

/// A value usable as a field of a `#[randomize]` struct: random values,
/// arrays and `Vec`s of them, and `#[randomize]` structs.
pub trait RandField {
    #[doc(hidden)]
    fn declare(&self, b: &mut Builder, path: &str, rand: bool, width: Option<u32>) -> Operand;
    #[doc(hidden)]
    fn write_back(&mut self, sol: &Solution, path: &str, rand: bool);
    #[doc(hidden)]
    fn pre(&mut self) {}
    #[doc(hidden)]
    fn post(&mut self) {}
}

impl<T: RandValue> RandField for T {
    fn declare(&self, b: &mut Builder, path: &str, rand: bool, width: Option<u32>) -> Operand {
        b.scalar(self, path, rand, width)
    }

    fn write_back(&mut self, sol: &Solution, path: &str, rand: bool) {
        if rand && let Some(v) = sol.value::<T>(path) {
            *self = v;
        }
    }
}

impl<T: RandField, const N: usize> RandField for [T; N] {
    fn declare(&self, b: &mut Builder, path: &str, rand: bool, width: Option<u32>) -> Operand {
        Operand::Array(
            self.iter()
                .enumerate()
                .map(|(i, e)| e.declare(b, &builder::elem(path, i), rand, width))
                .collect(),
        )
    }

    fn write_back(&mut self, sol: &Solution, path: &str, rand: bool) {
        for (i, e) in self.iter_mut().enumerate() {
            e.write_back(sol, &builder::elem(path, i), rand);
        }
    }

    fn pre(&mut self) {
        self.iter_mut().for_each(RandField::pre)
    }

    fn post(&mut self) {
        self.iter_mut().for_each(RandField::post)
    }
}

impl<T: RandField + Default> RandField for Vec<T> {
    fn declare(&self, b: &mut Builder, path: &str, rand: bool, width: Option<u32>) -> Operand {
        let (len, n) = b.list_len(path, self.len(), rand);
        let Some(n) = n else {
            return Operand::List {
                len,
                elems: Vec::new(),
                sized: false,
            };
        };
        let mut elems = Vec::with_capacity(n);
        for i in 0..n {
            let p = builder::elem(path, i);
            let op = match self.get(i) {
                Some(e) => e.declare(b, &p, rand, width),
                None => T::default().declare(b, &p, rand, width),
            };
            elems.push(op);
        }
        Operand::List {
            len,
            elems,
            sized: true,
        }
    }

    fn write_back(&mut self, sol: &Solution, path: &str, rand: bool) {
        if !rand {
            return;
        }
        if let Some(n) = sol.size(path) {
            self.resize_with(n, T::default);
        }
        for (i, e) in self.iter_mut().enumerate() {
            e.write_back(sol, &builder::elem(path, i), rand);
        }
    }

    fn pre(&mut self) {
        self.iter_mut().for_each(RandField::pre)
    }

    fn post(&mut self) {
        self.iter_mut().for_each(RandField::post)
    }
}

/// Implemented by `#[randomize]`.
#[doc(hidden)]
pub trait RandStruct: Sized + 'static {
    const FIELDS: &'static [&'static str];
    const NAME: &'static str;
    fn __state(&self) -> &RandState<Self>;
    fn __state_mut(&mut self) -> &mut RandState<Self>;
    /// The fields as an operand: variables for random fields.
    fn __fields(&self, b: &mut Builder, path: &str, rand: bool) -> Operand;
    /// The type's constraints (by `#[constraints]`), if any.
    fn __type_blocks(&self, b: &mut Builder);
    fn __write_back(&mut self, sol: &Solution, path: &str);
    /// `pre_randomize` of this object and of its random nested objects.
    fn __pre(&mut self);
    fn __post(&mut self);
}

/// Implemented by `#[constraints]` for the type's own constraints.
#[doc(hidden)]
pub trait TypeConstraints {
    fn __type_blocks(&self, b: &mut Builder);
    fn __pre_randomize(&mut self) {}
    fn __post_randomize(&mut self) {}
}

/// Runs an object's constraints: its type's, its attached sets' and
/// `extra`, minus those turned off or replaced.
#[doc(hidden)]
pub fn __constraints<T: RandStruct>(o: &T, b: &mut Builder, extra: Option<&dyn Constraints<T>>) {
    let st = o.__state();
    let mut replaced = Vec::new();
    for s in &st.sets {
        s.__replaces(&mut replaced);
    }
    if let Some(e) = extra {
        e.__replaces(&mut replaced);
    }
    b.object(st.off.clone(), replaced, |b| {
        o.__type_blocks(b);
        for s in &st.sets {
            s.__blocks(b);
        }
        if let Some(e) = extra {
            e.__blocks(b);
        }
    });
}

/// The operand of a nested `#[randomize]` field, with its constraints.
#[doc(hidden)]
pub fn __declare_struct<T: RandStruct>(o: &T, b: &mut Builder, path: &str, rand: bool) -> Operand {
    let op = o.__fields(b, path, rand);
    if rand {
        b.with_this(op.clone(), |b| __constraints(o, b, None));
    }
    op
}

/// The largest length a `Vec` field may take; a `Vec` whose constraints
/// allow more is reported as unbounded.
const MAX_LEN: u32 = 1 << 16;

fn short_name(full: &str) -> &str {
    full.rsplit("::").next().unwrap_or(full)
}

fn run<T: RandStruct>(
    obj: &mut T,
    extra: Option<&dyn Constraints<T>>,
    inline: Option<&dyn Fn(&mut Builder)>,
) -> Result<(), RandError> {
    obj.__pre();
    let mut rng = obj
        .__state()
        .rng
        .clone()
        .unwrap_or_else(|| SplitMix64::new(take_seed()));
    let owner = short_name(T::NAME);
    let timeout = TIMEOUT.with(Cell::get);

    let build = |obj: &T, phase: Phase, sizes: HashMap<String, usize>| {
        let mut b = Builder::new(phase, sizes);
        let root = obj.__fields(&mut b, "", true);
        b.with_this(root, |b| {
            __constraints(obj, b, extra);
            if let Some(f) = inline {
                b.inline_block(|b| f(b));
            }
        });
        let lens = b.lens.clone();
        let by_path = b.by_path.clone();
        b.finish(owner).map(|p| (p, lens, by_path))
    };

    // Sizes first, if there are random `Vec`s.
    let (p, lens, _) = build(obj, Phase::Size, HashMap::new())?;
    let mut sizes = HashMap::new();
    if !lens.is_empty() {
        let mut p = p;
        let limit = BigInt::from(MAX_LEN);
        for (path, id) in &lens {
            p.bounded.push((*id, limit.clone(), path.clone()));
        }
        let mut sat = new_solver(rng.next_u64());
        let values = solve::solve(&p, &mut rng, sat.as_mut(), timeout)?;
        for (path, id) in &lens {
            let n = values[*id].clone();
            sizes.insert(path.clone(), usize::try_from(n).unwrap_or(0));
        }
    }

    let (p, _, by_path) = build(obj, Phase::Elements, sizes.clone())?;
    let mut sat = new_solver(rng.next_u64());
    let values = solve::solve(&p, &mut rng, sat.as_mut(), timeout)?;
    let sol = Solution {
        values,
        by_path,
        sizes,
    };
    obj.__write_back(&sol, "");
    obj.__state_mut().rng = Some(rng);
    obj.__post();
    Ok(())
}

/// Randomization methods. `#[randomize]` also adds them as inherent
/// methods, so calling them needs no import.
pub trait Randomize: RandStruct {
    /// Solves the constraints and writes the solution, or returns the
    /// error and leaves the object unchanged.
    fn randomize(&mut self) -> Result<(), RandError> {
        run(self, None, None)
    }

    /// Like `randomize`, with a constraint set for this call only.
    fn randomize_with<C: Constraints<Self>>(&mut self, set: C) -> Result<(), RandError> {
        run(self, Some(&set), None)
    }

    #[doc(hidden)]
    fn __randomize_inline(&mut self, f: &dyn Fn(&mut Builder)) -> Result<(), RandError> {
        run(self, None, Some(f))
    }

    /// Attaches a constraint set, kept by the object.
    fn with_constraints<C: Constraints<Self>>(mut self, set: C) -> Self {
        self.__state_mut().sets.push(ConstraintSet::new(set));
        self
    }

    /// Replaces all attached constraint sets.
    fn set_constraints<C: Constraints<Self>>(&mut self, set: C) {
        let st = self.__state_mut();
        st.sets.clear();
        st.sets.push(ConstraintSet::new(set));
    }

    fn clear_constraints(&mut self) {
        self.__state_mut().sets.clear();
    }

    fn rand_on(&mut self, f: FieldName) {
        let st = self.__state_mut();
        st.rand_off.retain(|n| *n != f.0);
    }

    fn rand_off(&mut self, f: FieldName) {
        let st = self.__state_mut();
        if !st.rand_off.contains(&f.0) {
            st.rand_off.push(f.0);
        }
    }

    /// Turns every field off (SV `obj.rand_mode(0)`); `rand_on_all`
    /// turns them back on.
    fn rand_off_all(&mut self) {
        self.__state_mut().all_off = true;
    }

    fn rand_on_all(&mut self) {
        let st = self.__state_mut();
        st.all_off = false;
        st.rand_off.clear();
    }

    fn is_rand_on(&self, f: FieldName) -> bool {
        self.__state().is_rand_on(f.0)
    }

    fn constraint_on(&mut self, c: ConstraintName) {
        self.__state_mut().off.retain(|n| *n != c);
    }

    fn constraint_off(&mut self, c: ConstraintName) {
        let st = self.__state_mut();
        if !st.off.contains(&c) {
            st.off.push(c);
        }
    }

    fn is_constraint_on(&self, c: ConstraintName) -> bool {
        !self.__state().off.contains(&c)
    }

    /// Sets the object's seed (SV `srandom`).
    fn srandom(&mut self, seed: u64) {
        self.__state_mut().rng = Some(SplitMix64::new(seed));
    }

    /// The state of the object's generator (SV `get_randstate`).
    fn get_randstate(&self) -> Option<u64> {
        self.__state().rng.as_ref().map(SplitMix64::state)
    }

    fn set_randstate(&mut self, state: u64) {
        self.__state_mut().rng = Some(SplitMix64::new(state));
    }
}

impl<T: RandStruct> Randomize for T {}

/// `std_randomize!`: randomizes local variables.
#[doc(hidden)]
pub fn __std_randomize(
    declare: &dyn Fn(&mut Builder) -> Operand,
    constraints: &dyn Fn(&mut Builder),
) -> Result<Solution, RandError> {
    let mut rng = SplitMix64::new(take_seed());
    let mut b = Builder::new(Phase::Elements, HashMap::new());
    let root = declare(&mut b);
    b.with_this(root, |b| b.inline_block(|b| constraints(b)));
    let by_path = b.by_path.clone();
    let p = b.finish("std_randomize")?;
    let mut sat = new_solver(rng.next_u64());
    let values = solve::solve(&p, &mut rng, sat.as_mut(), TIMEOUT.with(Cell::get))?;
    Ok(Solution {
        values,
        by_path,
        sizes: HashMap::new(),
    })
}

/// `randcase!`: the index of the branch picked by the weights; `None`
/// when all weights are zero.
#[doc(hidden)]
pub fn __randcase_pick(weights: &[u64]) -> Option<usize> {
    let total: u128 = weights.iter().map(|w| *w as u128).sum();
    if total == 0 {
        return None;
    }
    let mut pick = with_thread_rng(|r| {
        let hi = r.next_u64() as u128;
        let lo = r.next_u64() as u128;
        ((hi << 64) | lo) % total
    });
    for (i, w) in weights.iter().enumerate() {
        let w = *w as u128;
        if pick < w {
            return Some(i);
        }
        pick -= w;
    }
    unreachable!("pick below the total")
}

/// Items used by the generated code.
#[doc(hidden)]
pub mod __rt {
    pub use super::builder::{
        Builder, InsideItem, InsideSource, Operand, Probe, Solution, TcNone, TcProbe, TcReal,
        ViaField, ViaOther, elem, has_field, join,
    };
    pub use super::ir::{BinOp, UnOp};
    pub use super::value::{RandValue, Ty};
    pub use super::{
        __constraints, __declare_struct, __randcase_pick, __std_randomize, ConstraintName,
        Constraints, FieldName, RandError, RandField, RandState, RandStruct, Randomize,
        TypeConstraints,
    };
    pub use num_bigint::BigInt;
}

#[cfg(test)]
mod tests;
