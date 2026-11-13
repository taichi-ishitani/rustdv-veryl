//! Building a problem from objects: the API the generated code calls.
//!
//! Everything here is `#[doc(hidden)]` through `random::__rt`: it is used
//! by the code that `#[randomize]`, `#[constraints]`, `#[constraint_fn]`
//! and `randomize_with!` generate, not by hand.

use std::collections::HashMap;

use num_bigint::BigInt;
use num_traits::{ToPrimitive, Zero};

use super::ir::{BinOp, DistRange, Expr, Item, Origin, UnOp, VarId};
use super::solve::{Problem, VarInfo};
use super::value::{RandValue, Ty};
use super::{ConstraintName, RandError};

/// A value in a constraint: a scalar expression or an aggregate.
#[derive(Clone, Debug)]
pub enum Operand {
    Scalar(Expr),
    /// A fixed-size array.
    Array(Vec<Operand>),
    /// A `Vec`: its length, and its elements once the length is known.
    List {
        len: Expr,
        elems: Vec<Operand>,
        sized: bool,
    },
    /// A `#[randomize]` struct: its fields.
    Struct(Vec<(&'static str, Operand)>),
    /// A field whose type cannot be used in constraints.
    Opaque(String),
}

impl Operand {
    fn skip() -> Operand {
        Operand::Scalar(Expr::Skip)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Only the lengths of `Vec`s are solved.
    Size,
    /// Everything, with the lengths fixed.
    Elements,
}

struct Frame {
    items: Vec<Item>,
    cond: Option<Expr>,
    then: Vec<Item>,
}

struct ObjCtx {
    off: Vec<ConstraintName>,
    replaced: Vec<ConstraintName>,
}

/// Collects variables and constraint items.
pub struct Builder {
    phase: Phase,
    pub(crate) vars: Vec<VarInfo>,
    pub(crate) by_path: HashMap<String, VarId>,
    /// Length variables of `Vec`s (size phase).
    pub(crate) lens: Vec<(String, VarId)>,
    sizes: HashMap<String, usize>,
    frames: Vec<Frame>,
    origins: Vec<Origin>,
    before: Vec<Vec<VarId>>,
    this: Vec<Operand>,
    block: Vec<String>,
    ctx: Vec<ObjCtx>,
    priority: usize,
    depth: usize,
    error: Option<String>,
    constraints: Vec<String>,
    constraints_off: Vec<String>,
    rand_off: Vec<String>,
}

/// The deepest nesting of `#[constraint_fn]` calls.
const MAX_FN_DEPTH: usize = 64;

impl Builder {
    pub(crate) fn new(phase: Phase, sizes: HashMap<String, usize>) -> Builder {
        Builder {
            phase,
            vars: Vec::new(),
            by_path: HashMap::new(),
            lens: Vec::new(),
            sizes,
            frames: vec![Frame {
                items: Vec::new(),
                cond: None,
                then: Vec::new(),
            }],
            origins: Vec::new(),
            before: Vec::new(),
            this: Vec::new(),
            block: Vec::new(),
            ctx: Vec::new(),
            priority: 0,
            depth: 0,
            error: None,
            constraints: Vec::new(),
            constraints_off: Vec::new(),
            rand_off: Vec::new(),
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Records the first error; building goes on with dummy values.
    pub fn fail(&mut self, msg: impl Into<String>) {
        if self.error.is_none() {
            self.error = Some(msg.into());
        }
    }

    pub(crate) fn finish(mut self, owner: &str) -> Result<Problem, RandError> {
        if let Some(e) = self.error.take() {
            return Err(RandError::build(owner, e));
        }
        let frame = self.frames.pop().expect("root frame");
        debug_assert!(self.frames.is_empty(), "balanced if frames");
        Ok(Problem {
            owner: owner.to_string(),
            vars: self.vars,
            items: frame.items,
            origins: self.origins,
            before: self.before,
            rand_off: self.rand_off,
            constraints: self.constraints,
            constraints_off: self.constraints_off,
            bounded: Vec::new(),
        })
    }

    // ---- declaring ----

    pub fn new_var(&mut self, path: &str, ty: Ty, domain: Option<Vec<BigInt>>) -> Expr {
        let id = self.vars.len();
        self.vars.push(VarInfo {
            name: path.to_string(),
            ty,
            domain,
        });
        self.by_path.insert(path.to_string(), id);
        Expr::Var(id, ty)
    }

    /// A scalar field: a variable when `rand`, a constant otherwise.
    pub fn scalar<T: RandValue>(
        &mut self,
        v: &T,
        path: &str,
        rand: bool,
        width: Option<u32>,
    ) -> Operand {
        let Some(ty) = T::TY.or(width.map(Ty::unsigned)) else {
            self.fail(format!("`{path}` needs `#[rand(width = N)]`"));
            return Operand::Scalar(Expr::bool(false));
        };
        if rand {
            Operand::Scalar(self.new_var(path, ty, T::domain()))
        } else {
            Operand::Scalar(Expr::konst_ty(v.to_bigint(), ty))
        }
    }

    pub fn note_rand_off(&mut self, path: &str) {
        self.rand_off.push(path.to_string());
    }

    /// The length of a `Vec` field: a variable in the size phase.
    pub fn list_len(&mut self, path: &str, current: usize, rand: bool) -> (Expr, Option<usize>) {
        if !rand {
            return (Expr::konst(BigInt::from(current)), Some(current));
        }
        match self.phase {
            Phase::Size => {
                let p = format!("{path}.len()");
                let e = self.new_var(&p, Ty::unsigned(32), None);
                if let Expr::Var(id, _) = e {
                    self.lens.push((path.to_string(), id));
                }
                (e, None)
            }
            Phase::Elements => {
                let n = self.sizes.get(path).copied().unwrap_or(current);
                (Expr::konst(BigInt::from(n)), Some(n))
            }
        }
    }

    // ---- objects and blocks ----

    pub fn with_this<R>(&mut self, this: Operand, f: impl FnOnce(&mut Builder) -> R) -> R {
        self.this.push(this);
        let r = f(self);
        self.this.pop();
        r
    }

    pub fn object<R>(
        &mut self,
        off: Vec<ConstraintName>,
        replaced: Vec<ConstraintName>,
        f: impl FnOnce(&mut Builder) -> R,
    ) -> R {
        self.ctx.push(ObjCtx { off, replaced });
        let r = f(self);
        self.ctx.pop();
        r
    }

    /// Runs a named constraint block unless it is off or replaced.
    pub fn block(&mut self, name: ConstraintName, f: impl FnOnce(&mut Builder)) {
        let label = name.to_string();
        if let Some(c) = self.ctx.last() {
            if c.off.contains(&name) {
                self.constraints_off.push(label);
                return;
            }
            if c.replaced.contains(&name) {
                return;
            }
        }
        self.constraints.push(label.clone());
        self.priority += 1;
        self.block.push(label);
        f(self);
        self.block.pop();
    }

    /// An anonymous block (`randomize_with!`).
    pub fn inline_block(&mut self, f: impl FnOnce(&mut Builder)) {
        self.priority += 1;
        self.block.push("with".to_string());
        f(self);
        self.block.pop();
    }

    // ---- names ----

    pub fn this(&self) -> Operand {
        self.this.last().cloned().expect("inside an object")
    }

    pub fn field(&mut self, name: &str) -> Operand {
        let this = self.this();
        self.member(this, name)
    }

    pub fn member(&mut self, op: Operand, name: &str) -> Operand {
        match op {
            Operand::Struct(fields) => match fields.into_iter().find(|(n, _)| *n == name) {
                Some((_, Operand::Opaque(path))) => {
                    self.fail(format!(
                        "`{path}` cannot be used in constraints: its type is not a random value"
                    ));
                    Operand::Scalar(Expr::bool(false))
                }
                Some((_, o)) => o,
                None => {
                    self.fail(format!("no field `{name}`"));
                    Operand::Scalar(Expr::bool(false))
                }
            },
            _ => {
                self.fail(format!("`.{name}` on a value that is not a struct"));
                Operand::Scalar(Expr::bool(false))
            }
        }
    }

    pub fn konst<T: RandValue>(&mut self, v: &T) -> Operand {
        let b = v.to_bigint();
        Operand::Scalar(match T::TY {
            Some(ty) => Expr::konst_ty(b, ty),
            None => Expr::konst(b),
        })
    }

    pub fn lit(&mut self, digits: &str) -> Operand {
        let clean: String = digits.chars().filter(|c| *c != '_').collect();
        let v = if let Some(h) = clean.strip_prefix("0x") {
            BigInt::parse_bytes(h.as_bytes(), 16)
        } else if let Some(b) = clean.strip_prefix("0b") {
            BigInt::parse_bytes(b.as_bytes(), 2)
        } else if let Some(o) = clean.strip_prefix("0o") {
            BigInt::parse_bytes(o.as_bytes(), 8)
        } else {
            BigInt::parse_bytes(clean.as_bytes(), 10)
        };
        match v {
            Some(v) => Operand::Scalar(Expr::konst(v)),
            None => {
                self.fail(format!("bad integer literal `{digits}`"));
                Operand::Scalar(Expr::bool(false))
            }
        }
    }

    pub fn bool_lit(&mut self, b: bool) -> Operand {
        Operand::Scalar(Expr::bool(b))
    }

    pub fn index_var(&mut self, i: usize) -> Operand {
        Operand::Scalar(Expr::konst(BigInt::from(i)))
    }

    // ---- expressions ----

    pub fn scalar_of(&mut self, op: Operand) -> Expr {
        match op {
            Operand::Scalar(e) => e,
            _ => {
                self.fail("an array or struct where a value is expected");
                Expr::bool(false)
            }
        }
    }

    pub fn un(&mut self, op: UnOp, a: Operand) -> Operand {
        let a = self.scalar_of(a);
        if a.has_skip() {
            return Operand::skip();
        }
        Operand::Scalar(Expr::un(op, a))
    }

    pub fn bin(&mut self, op: BinOp, a: Operand, b: Operand) -> Operand {
        let a = self.scalar_of(a);
        let b = self.scalar_of(b);
        if a.has_skip() || b.has_skip() {
            return Operand::skip();
        }
        Operand::Scalar(Expr::bin(op, a, b))
    }

    pub fn cast<T: RandValue>(&mut self, a: Operand) -> Operand {
        let a = self.scalar_of(a);
        if a.has_skip() {
            return Operand::skip();
        }
        match T::TY {
            Some(ty) => Operand::Scalar(Expr::cast(a, ty)),
            None => {
                self.fail("cannot cast to a type without a fixed width");
                Operand::Scalar(a)
            }
        }
    }

    pub fn index(&mut self, op: Operand, idx: Operand) -> Operand {
        let idx = self.scalar_of(idx);
        if idx.has_skip() {
            return Operand::skip();
        }
        let elems = match op {
            Operand::Array(e) => e,
            Operand::List {
                elems, sized: true, ..
            } => elems,
            Operand::List { sized: false, .. } => return Operand::skip(),
            _ => {
                self.fail("indexing a value that is not an array");
                return Operand::Scalar(Expr::bool(false));
            }
        };
        if let Some(k) = idx.as_const() {
            return match k.to_usize().filter(|k| *k < elems.len()) {
                Some(k) => elems[k].clone(),
                None => {
                    let msg = format!(
                        "index {k} is outside an array of length {} in {}",
                        elems.len(),
                        self.block
                            .last()
                            .map(String::as_str)
                            .unwrap_or("a constraint")
                    );
                    self.fail(msg);
                    Operand::Scalar(Expr::bool(false))
                }
            };
        }
        // A random index: one case per element, and the index in range.
        let mut scalars = Vec::new();
        for e in &elems {
            match e {
                Operand::Scalar(x) => scalars.push(x.clone()),
                _ => {
                    self.fail("a random index into an array of arrays or structs");
                    return Operand::Scalar(Expr::bool(false));
                }
            }
        }
        if scalars.is_empty() {
            self.fail("a random index into an empty array");
            return Operand::Scalar(Expr::bool(false));
        }
        let n = Expr::konst(BigInt::from(scalars.len()));
        let in_range = Expr::bin(BinOp::Lt, idx.clone(), n);
        self.push_item_expr(in_range, "index in range".to_string());
        let mut acc = scalars.last().cloned().expect("non-empty");
        for (k, x) in scalars.iter().enumerate().rev().skip(1) {
            let c = Expr::bin(BinOp::Eq, idx.clone(), Expr::konst(BigInt::from(k)));
            acc = Expr::ite(c, x.clone(), acc);
        }
        Operand::Scalar(acc)
    }

    pub fn len(&mut self, op: Operand) -> Operand {
        match op {
            Operand::Array(e) => Operand::Scalar(Expr::konst(BigInt::from(e.len()))),
            Operand::List { len, .. } => Operand::Scalar(len),
            _ => {
                self.fail("`len()` of a value that is not an array");
                Operand::Scalar(Expr::bool(false))
            }
        }
    }

    fn elements(&mut self, op: Operand) -> Option<Vec<Expr>> {
        let elems = match op {
            Operand::Array(e) => e,
            Operand::List {
                elems, sized: true, ..
            } => elems,
            Operand::List { sized: false, .. } => return None,
            _ => {
                self.fail("an array is expected");
                return Some(Vec::new());
            }
        };
        Some(elems.into_iter().map(|e| self.scalar_of(e)).collect())
    }

    /// `sum`, `product`, `and`, `or`, `xor` of an array.
    pub fn reduce(&mut self, op: Operand, kind: &str) -> Operand {
        let Some(elems) = self.elements(op) else {
            return Operand::skip();
        };
        let (bop, unit) = match kind {
            "sum" => (BinOp::Add, 0),
            "product" => (BinOp::Mul, 1),
            "and" => (BinOp::And, -1),
            "or" => (BinOp::Or, 0),
            "xor" => (BinOp::Xor, 0),
            _ => unreachable!("checked by the macro"),
        };
        let mut it = elems.into_iter();
        let Some(first) = it.next() else {
            return Operand::Scalar(Expr::konst(BigInt::from(unit)));
        };
        let acc = it.fold(first, |a, x| Expr::bin(bop, a, x));
        Operand::Scalar(acc)
    }

    /// `x.inside([..])`: values and ranges.
    pub fn inside(&mut self, x: Operand, items: Vec<InsideItem>) -> Operand {
        let x = self.scalar_of(x);
        let mut acc = Expr::bool(false);
        for it in items {
            let c = match it {
                InsideItem::Value(v) => {
                    let v = self.scalar_of(v);
                    Expr::bin(BinOp::Eq, x.clone(), v)
                }
                InsideItem::Range(lo, hi, inclusive) => {
                    let (lo, hi) = (self.scalar_of(lo), self.scalar_of(hi));
                    let ge = Expr::bin(BinOp::Ge, x.clone(), lo);
                    let op = if inclusive { BinOp::Le } else { BinOp::Lt };
                    let le = Expr::bin(op, x.clone(), hi);
                    Expr::bin(BinOp::LAnd, ge, le)
                }
                InsideItem::Array(a) => match self.elements(a) {
                    None => return Operand::skip(),
                    Some(es) => es.into_iter().fold(Expr::bool(false), |acc, e| {
                        let eq = Expr::bin(BinOp::Eq, x.clone(), e);
                        Expr::bin(BinOp::LOr, acc, eq)
                    }),
                },
            };
            if c.has_skip() {
                return Operand::skip();
            }
            acc = Expr::bin(BinOp::LOr, acc, c);
        }
        Operand::Scalar(acc)
    }

    /// `x.inside({ collection })`.
    pub fn inside_set<C: InsideSource>(&mut self, x: Operand, c: &C) -> Operand {
        let items = c.items(self);
        self.inside(x, items)
    }

    // ---- statements ----

    fn origin(&mut self, loc: String, text: String) -> usize {
        let block = self
            .block
            .last()
            .cloned()
            .unwrap_or_else(|| "constraint".to_string());
        self.origins.push(Origin { block, loc, text });
        self.origins.len() - 1
    }

    fn push(&mut self, it: Item) {
        if it.has_skip() {
            return;
        }
        self.frames.last_mut().expect("frame").items.push(it);
    }

    fn push_item_expr(&mut self, e: Expr, text: String) {
        let o = self.origin(String::new(), text);
        self.push(Item::Hard(Expr::truth(e), o));
    }

    pub fn hard(&mut self, c: Operand, file: &str, line: u32, text: &str) {
        let e = self.scalar_of(c);
        let o = self.origin(format!("{file}:{line}"), text.to_string());
        if let Some(v) = e.as_const() {
            if v.is_zero() {
                // Kept: a constant false makes the problem unsatisfiable,
                // and the report names it.
                self.push(Item::Hard(Expr::bool(false), o));
            }
            return;
        }
        self.push(Item::Hard(Expr::truth(e), o));
    }

    pub fn soft(&mut self, c: Operand, file: &str, line: u32, text: &str) {
        let e = self.scalar_of(c);
        let _ = (file, line, text);
        let prio = self.priority;
        self.push(Item::Soft(Expr::truth(e), prio));
    }

    /// `unique([..])` or `unique(arr)`: every pair differs.
    pub fn unique(&mut self, ops: Vec<Operand>, file: &str, line: u32, text: &str) {
        let mut es = Vec::new();
        for op in ops {
            match op {
                Operand::Scalar(e) => es.push(e),
                other => match self.elements(other) {
                    Some(v) => es.extend(v),
                    None => return,
                },
            }
        }
        let o = self.origin(format!("{file}:{line}"), text.to_string());
        for i in 0..es.len() {
            for j in i + 1..es.len() {
                let ne = Expr::bin(BinOp::Ne, es[i].clone(), es[j].clone());
                self.push(Item::Hard(ne, o));
            }
        }
    }

    /// `dist!(x, ..)`: `each` gives the weight per value.
    pub fn dist(
        &mut self,
        x: Operand,
        ranges: Vec<(Operand, Operand, bool, Operand, bool)>,
        file: &str,
        line: u32,
        text: &str,
    ) {
        let x = self.scalar_of(x);
        let mut out = Vec::new();
        for (lo, hi, inclusive, w, each) in ranges {
            let (lo, hi, w) = (self.scalar_of(lo), self.scalar_of(hi), self.scalar_of(w));
            let (Some(lo), Some(hi), Some(w)) = (lo.as_const(), hi.as_const(), w.as_const()) else {
                self.fail("`dist` ranges and weights must be constants");
                return;
            };
            let hi = if inclusive { hi.clone() } else { hi - 1 };
            if hi < *lo || w.sign() == num_bigint::Sign::Minus {
                continue;
            }
            let weight = if each { w * (&hi - lo + 1) } else { w.clone() };
            out.push(DistRange {
                lo: lo.clone(),
                hi,
                weight,
            });
        }
        let o = self.origin(format!("{file}:{line}"), text.to_string());
        self.push(Item::Dist(x, out, o));
    }

    pub fn solve_before(&mut self, a: Vec<Operand>, _b: Vec<Operand>) {
        let mut ids = Vec::new();
        for op in a {
            let e = self.scalar_of(op);
            match e {
                Expr::Var(id, _) => ids.push(id),
                Expr::Skip | Expr::Const(..) => {}
                _ => self.fail("`solve_before` takes random variables"),
            }
        }
        self.before.push(ids);
    }

    /// Starts an `if`; `Some(b)` when the condition is a constant, in which
    /// case only that branch is built.
    pub fn if_begin(&mut self, c: Operand) -> Option<bool> {
        let e = self.scalar_of(c);
        if let Some(v) = e.as_const() {
            return Some(!v.is_zero());
        }
        self.frames.push(Frame {
            items: Vec::new(),
            cond: Some(e),
            then: Vec::new(),
        });
        None
    }

    pub fn if_else(&mut self, k: Option<bool>) {
        if k.is_none() {
            let f = self.frames.last_mut().expect("if frame");
            f.then = std::mem::take(&mut f.items);
        }
    }

    pub fn if_end(&mut self, k: Option<bool>) {
        if k.is_none() {
            let f = self.frames.pop().expect("if frame");
            let c = f.cond.expect("if condition");
            self.push(Item::If(Expr::truth(c), f.then, f.items));
        }
    }

    /// The bounds of `for i in lo..hi`; empty when they are not known yet.
    pub fn range(&mut self, lo: Operand, hi: Operand, inclusive: bool) -> std::ops::Range<usize> {
        let (lo, hi) = (self.scalar_of(lo), self.scalar_of(hi));
        let (Some(lo), Some(hi)) = (lo.as_const(), hi.as_const()) else {
            if self.phase == Phase::Elements {
                self.fail("loop bounds must be constants or array lengths");
            }
            return 0..0;
        };
        let lo = lo.to_usize().unwrap_or(0);
        let hi = hi.to_usize().unwrap_or(0) + inclusive as usize;
        lo..hi.max(lo)
    }

    // ---- constraint functions ----

    pub fn fn_enter(&mut self, name: &str) -> bool {
        self.depth += 1;
        if self.depth > MAX_FN_DEPTH {
            self.fail(format!(
                "constraint functions nest deeper than {MAX_FN_DEPTH} (recursion through `{name}`?)"
            ));
            return false;
        }
        self.frames.push(Frame {
            items: Vec::new(),
            cond: None,
            then: Vec::new(),
        });
        true
    }

    /// Ends a `bool` constraint function: its items, and its tail
    /// expression if any, as one value.
    pub fn fn_exit_bool(&mut self, tail: Option<Operand>) -> Operand {
        self.depth -= 1;
        let f = self.frames.pop().expect("fn frame");
        let mut acc = Expr::bool(true);
        let mut skip = false;
        for it in f.items {
            match item_expr(&it) {
                Some(e) => acc = Expr::bin(BinOp::LAnd, acc, e),
                None => {
                    if it.has_skip() {
                        skip = true;
                    } else {
                        self.fail("`soft` and `dist` are not allowed in a constraint function");
                    }
                }
            }
        }
        if let Some(t) = tail {
            let t = self.scalar_of(t);
            if t.has_skip() {
                skip = true;
            } else {
                acc = Expr::bin(BinOp::LAnd, acc, Expr::truth(t));
            }
        }
        if skip {
            Operand::skip()
        } else {
            Operand::Scalar(acc)
        }
    }

    pub fn fn_exit_value<T: RandValue>(&mut self, tail: Operand) -> Operand {
        self.depth -= 1;
        let f = self.frames.pop().expect("fn frame");
        if !f.items.is_empty() {
            self.fail("a constraint function returning a value has only an expression");
        }
        self.cast::<T>(tail)
    }
}

/// A constraint item as an expression (inside constraint functions).
fn item_expr(it: &Item) -> Option<Expr> {
    match it {
        Item::Hard(e, _) => Some(e.clone()),
        Item::If(c, a, b) => {
            let all = |v: &[Item]| -> Option<Expr> {
                let mut acc = Expr::bool(true);
                for i in v {
                    acc = Expr::bin(BinOp::LAnd, acc, item_expr(i)?);
                }
                Some(acc)
            };
            let (ta, tb) = (all(a)?, all(b)?);
            let t = Expr::bin(BinOp::LAnd, c.clone(), ta);
            let f = Expr::bin(BinOp::LAnd, Expr::un(UnOp::LNot, c.clone()), tb);
            Some(Expr::bin(BinOp::LOr, t, f))
        }
        Item::Soft(..) | Item::Dist(..) => None,
    }
}

/// An element of `inside([..])`.
pub enum InsideItem {
    Value(Operand),
    Range(Operand, Operand, bool),
    /// All the elements of an array.
    Array(Operand),
}

/// A Rust collection usable in `x.inside({ .. })`.
pub trait InsideSource {
    fn items(&self, b: &mut Builder) -> Vec<InsideItem>;
}

impl<T: RandValue> InsideSource for [T] {
    fn items(&self, b: &mut Builder) -> Vec<InsideItem> {
        self.iter().map(|v| InsideItem::Value(b.konst(v))).collect()
    }
}

impl<T: RandValue> InsideSource for Vec<T> {
    fn items(&self, b: &mut Builder) -> Vec<InsideItem> {
        self.as_slice().items(b)
    }
}

impl<T: RandValue, const N: usize> InsideSource for [T; N] {
    fn items(&self, b: &mut Builder) -> Vec<InsideItem> {
        self.as_slice().items(b)
    }
}

impl<T: RandValue> InsideSource for [std::ops::RangeInclusive<T>] {
    fn items(&self, b: &mut Builder) -> Vec<InsideItem> {
        self.iter()
            .map(|r| InsideItem::Range(b.konst(r.start()), b.konst(r.end()), true))
            .collect()
    }
}

impl<T: RandValue> InsideSource for Vec<std::ops::RangeInclusive<T>> {
    fn items(&self, b: &mut Builder) -> Vec<InsideItem> {
        self.as_slice().items(b)
    }
}

impl<C: InsideSource + ?Sized> InsideSource for &C {
    fn items(&self, b: &mut Builder) -> Vec<InsideItem> {
        (**self).items(b)
    }
}

/// A solved problem: values by path.
pub struct Solution {
    pub(crate) values: Vec<BigInt>,
    pub(crate) by_path: HashMap<String, VarId>,
    pub(crate) sizes: HashMap<String, usize>,
}

impl Solution {
    pub fn value<T: RandValue>(&self, path: &str) -> Option<T> {
        let id = *self.by_path.get(path)?;
        Some(T::from_bigint(&self.values[id]))
    }

    pub fn size(&self, path: &str) -> Option<usize> {
        self.sizes.get(path).copied()
    }
}

/// `a.b` paths of fields; `[i]` for elements.
pub fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}.{name}")
    }
}

pub fn elem(path: &str, i: usize) -> String {
    format!("{path}[{i}]")
}

// ---- detecting field types (autoref specialization) ----

/// Wraps a state field to find out whether its type can be used in
/// constraints: `(&Probe(&x)).declare_state(..)` picks [`ViaField`] when
/// the type is a [`RandField`](super::RandField), and [`ViaOther`] when
/// not.
pub struct Probe<'a, T>(pub &'a T);

pub trait ViaField {
    fn declare_state(&self, b: &mut Builder, path: &str) -> Operand;
}

impl<T: super::RandField> ViaField for Probe<'_, T> {
    fn declare_state(&self, b: &mut Builder, path: &str) -> Operand {
        self.0.declare(b, path, false, None)
    }
}

pub trait ViaOther {
    fn declare_state(&self, _b: &mut Builder, path: &str) -> Operand {
        Operand::Opaque(path.to_string())
    }
}

impl<T> ViaOther for &Probe<'_, T> {}

/// Finds out whether a type has `#[constraints]`.
pub struct TcProbe<T>(std::marker::PhantomData<T>);

impl<T> TcProbe<T> {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        TcProbe(std::marker::PhantomData)
    }
}

pub trait TcReal<T> {
    fn type_blocks(&self, o: &T, b: &mut Builder);
    fn pre_randomize(&self, o: &mut T);
    fn post_randomize(&self, o: &mut T);
}

impl<T: super::TypeConstraints> TcReal<T> for TcProbe<T> {
    fn type_blocks(&self, o: &T, b: &mut Builder) {
        o.__type_blocks(b)
    }
    fn pre_randomize(&self, o: &mut T) {
        o.__pre_randomize()
    }
    fn post_randomize(&self, o: &mut T) {
        o.__post_randomize()
    }
}

pub trait TcNone<T> {
    fn type_blocks(&self, _o: &T, _b: &mut Builder) {}
    fn pre_randomize(&self, _o: &mut T) {}
    fn post_randomize(&self, _o: &mut T) {}
}

impl<T> TcNone<T> for &TcProbe<T> {}

/// `const` check that a field exists, for compile errors on typos.
pub const fn has_field(fields: &[&str], name: &str) -> bool {
    let mut i = 0;
    while i < fields.len() {
        if str_eq(fields[i], name) {
            return true;
        }
        i += 1;
    }
    false
}

const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}
