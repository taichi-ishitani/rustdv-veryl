//! Solving a problem in stages (see `random-design.md`).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use num_bigint::BigInt;
use num_traits::Zero;

use super::RandError;
use super::blast::Blaster;
use super::ir::{BinOp, DistRange, Expr, Item, Origin, VarId, eval};
use super::rng::SplitMix64;
use super::sat::{Lit, SatSolver, SolveResult};
use super::value::Ty;

#[derive(Clone, Debug)]
pub struct VarInfo {
    pub name: String,
    pub ty: Ty,
    /// The values the variable may take (an enum's variants).
    pub domain: Option<Vec<BigInt>>,
}

#[derive(Default)]
pub struct Problem {
    pub owner: String,
    pub vars: Vec<VarInfo>,
    pub items: Vec<Item>,
    pub origins: Vec<Origin>,
    /// `solve_before(a, b)`: the variables of `a` are fixed before those
    /// of `b`; only the order of the `a` side matters.
    pub before: Vec<Vec<VarId>>,
    /// Constants and variables turned off, for the report.
    pub rand_off: Vec<String>,
    pub constraints: Vec<String>,
    pub constraints_off: Vec<String>,
    /// Length variables that must not exceed a limit unless the
    /// constraints bound them: (variable, limit, path).
    pub bounded: Vec<(VarId, BigInt, String)>,
}

/// Retries for picking a value of a `solve_before` or `dist` variable.
const PICK_TRIES: usize = 8;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

pub fn solve(
    p: &Problem,
    rng: &mut SplitMix64,
    sat: &mut dyn SatSolver,
    timeout: Option<Duration>,
) -> Result<Vec<BigInt>, RandError> {
    let deadline = timeout.map(|t| Instant::now() + t);
    sat.set_deadline(deadline);

    // Variables used by the constraints; the others are picked directly.
    let mut used = Vec::new();
    for it in &p.items {
        item_vars(it, &mut used);
    }
    for b in &p.before {
        used.extend(b);
    }
    used.extend(p.bounded.iter().map(|b| b.0));
    let used: HashSet<VarId> = used.into_iter().collect();

    let mut values: Vec<BigInt> = vec![BigInt::zero(); p.vars.len()];
    let mut bl = Blaster::new(sat);
    for (id, v) in p.vars.iter().enumerate() {
        if used.contains(&id) {
            let pol: Vec<bool> = (0..v.ty.width).map(|_| rng.next_u64() & 1 == 1).collect();
            bl.declare_var(id, v.ty, &pol);
        } else {
            values[id] = random_value(v, rng);
        }
    }

    // Hard items get a selector each, so that the unsat core names them.
    let mut hard: Vec<(Lit, usize)> = Vec::new();
    let mut soft: Vec<(Lit, usize)> = Vec::new();
    let mut dists: Vec<(Expr, Vec<DistRange>)> = Vec::new();
    let t = bl.tru();
    for (id, v) in p.vars.iter().enumerate() {
        if let (true, Some(dom)) = (used.contains(&id), &v.domain) {
            let e = Expr::Var(id, v.ty);
            let lits: Vec<Lit> = dom
                .iter()
                .map(|d| {
                    let eq = Expr::bin(BinOp::Eq, e.clone(), Expr::konst_ty(d.clone(), v.ty));
                    let b = bl.expr(&eq);
                    b.bits[0]
                })
                .collect();
            let l = bl.or_n(&lits);
            bl.sat.add_clause(&[l]);
        }
    }
    for it in &p.items {
        encode_item(&mut bl, it, t, &mut hard, &mut soft, &mut dists);
    }

    let mut assume: Vec<Lit> = hard.iter().map(|(l, _)| *l).collect();
    match bl.sat.solve(&assume) {
        SolveResult::Sat => {}
        SolveResult::Unsat => {
            let core: Vec<usize> = hard
                .iter()
                .filter(|(l, _)| bl.sat.in_unsat_core(l.var))
                .map(|(_, o)| *o)
                .collect();
            return Err(RandError::unsat(p, &core));
        }
        SolveResult::Interrupted => return Err(RandError::timeout(p)),
    }

    // A `Vec` length the constraints do not bound.
    for (id, limit, path) in &p.bounded {
        let ty = p.vars[*id].ty;
        let gt = Expr::bin(BinOp::Gt, Expr::Var(*id, ty), Expr::konst(limit.clone()));
        let l = cond_lit(&mut bl, &gt);
        let mut a = assume.clone();
        a.push(l);
        match bl.sat.solve(&a) {
            SolveResult::Sat => {
                return Err(RandError::build(
                    &p.owner,
                    format!(
                        "the length of `{path}` is not bounded: add a constraint like `{path}.len() <= N` (N <= {limit})"
                    ),
                ));
            }
            SolveResult::Unsat => {}
            SolveResult::Interrupted => return Err(RandError::timeout(p)),
        }
    }

    // Soft constraints, highest priority first, each kept if it fits.
    soft.sort_by_key(|(_, prio)| std::cmp::Reverse(*prio));
    for (l, _) in &soft {
        assume.push(*l);
        match bl.sat.solve(&assume) {
            SolveResult::Sat => {}
            SolveResult::Unsat => {
                assume.pop();
            }
            SolveResult::Interrupted => return Err(RandError::timeout(p)),
        }
    }

    // `solve_before`: fix the variables in order, each by a random value
    // where it fits.
    for group in &p.before {
        for &id in group {
            let v = &p.vars[id];
            let mut fixed = false;
            for _ in 0..PICK_TRIES {
                let cand = random_value(v, rng);
                let lits = eq_lits(&bl, id, v.ty, &cand);
                let n = assume.len();
                assume.extend(lits);
                match bl.sat.solve(&assume) {
                    SolveResult::Sat => {
                        fixed = true;
                        break;
                    }
                    SolveResult::Unsat => assume.truncate(n),
                    SolveResult::Interrupted => return Err(RandError::timeout(p)),
                }
            }
            if !fixed {
                fix_to_model(&mut bl, &mut assume, id, v.ty, p)?;
            }
        }
    }

    // `dist`: pick a range by its weight and a value in it.
    for (e, ranges) in &dists {
        let total: BigInt = ranges.iter().map(|r| r.weight.clone()).sum();
        if total.is_zero() {
            continue;
        }
        let ty = e.ty();
        for _ in 0..PICK_TRIES {
            let mut pick = rng.below_big(&total);
            let r = ranges
                .iter()
                .find(|r| {
                    if pick < r.weight {
                        true
                    } else {
                        pick -= &r.weight;
                        false
                    }
                })
                .expect("pick below the total weight");
            let span: BigInt = &r.hi - &r.lo + 1;
            let val = &r.lo + rng.below_big(&span);
            let eq = Expr::bin(BinOp::Eq, e.clone(), Expr::konst_ty(val, ty));
            let b = bl.expr(&eq);
            bl.take_side();
            assume.push(b.bits[0]);
            match bl.sat.solve(&assume) {
                SolveResult::Sat => break,
                SolveResult::Unsat => {
                    assume.pop();
                }
                SolveResult::Interrupted => return Err(RandError::timeout(p)),
            }
        }
    }

    match bl.sat.solve(&assume) {
        SolveResult::Sat => {}
        SolveResult::Unsat => unreachable!("the assumptions were checked"),
        SolveResult::Interrupted => return Err(RandError::timeout(p)),
    }
    for (id, v) in p.vars.iter().enumerate() {
        if used.contains(&id) {
            values[id] = bl.model_value(id, v.ty);
        }
    }
    debug_assert!(check(p, &values), "the solution satisfies the constraints");
    Ok(values)
}

fn fix_to_model(
    bl: &mut Blaster,
    assume: &mut Vec<Lit>,
    id: VarId,
    ty: Ty,
    p: &Problem,
) -> Result<(), RandError> {
    match bl.sat.solve(assume) {
        SolveResult::Sat => {
            let v = bl.model_value(id, ty);
            let lits = eq_lits(bl, id, ty, &v);
            assume.extend(lits);
            Ok(())
        }
        SolveResult::Unsat => unreachable!("the assumptions were checked"),
        SolveResult::Interrupted => Err(RandError::timeout(p)),
    }
}

/// Literals fixing a variable's bits to a value.
fn eq_lits(bl: &Blaster, id: VarId, ty: Ty, v: &BigInt) -> Vec<Lit> {
    let p = ty.pattern(v);
    bl.var_bits(id)
        .iter()
        .enumerate()
        .map(|(i, l)| if p.bit(i as u64) { *l } else { !*l })
        .collect()
}

fn random_value(v: &VarInfo, rng: &mut SplitMix64) -> BigInt {
    match &v.domain {
        Some(d) if !d.is_empty() => d[(rng.next_u64() % d.len() as u64) as usize].clone(),
        _ => {
            let span = BigInt::from(1u8) << v.ty.width;
            v.ty.wrap(&rng.below_big(&span))
        }
    }
}

fn encode_item(
    bl: &mut Blaster,
    it: &Item,
    guard: Lit,
    hard: &mut Vec<(Lit, usize)>,
    soft: &mut Vec<(Lit, usize)>,
    dists: &mut Vec<(Expr, Vec<DistRange>)>,
) {
    match it {
        Item::Hard(e, o) => {
            let l = cond_lit(bl, e);
            let sel = bl.new_lit();
            bl.sat.add_clause(&[!sel, !guard, l]);
            hard.push((sel, *o));
        }
        Item::Soft(e, prio) => {
            let l = cond_lit(bl, e);
            let sel = bl.new_lit();
            bl.sat.add_clause(&[!sel, !guard, l]);
            soft.push((sel, *prio));
        }
        Item::Dist(e, ranges, o) => {
            let mut any = Vec::new();
            for r in ranges.iter().filter(|r| !r.weight.is_zero()) {
                let ge = Expr::bin(BinOp::Ge, e.clone(), Expr::konst(r.lo.clone()));
                let le = Expr::bin(BinOp::Le, e.clone(), Expr::konst(r.hi.clone()));
                let inr = Expr::bin(BinOp::LAnd, ge, le);
                any.push(cond_lit(bl, &inr));
            }
            let l = bl.or_n(&any);
            let sel = bl.new_lit();
            bl.sat.add_clause(&[!sel, !guard, l]);
            hard.push((sel, *o));
            dists.push((e.clone(), ranges.clone()));
        }
        Item::If(c, a, b) => {
            let cl = cond_lit(bl, c);
            let ga = bl.and2(guard, cl);
            let gb = bl.and2(guard, !cl);
            for i in a {
                encode_item(bl, i, ga, hard, soft, dists);
            }
            for i in b {
                encode_item(bl, i, gb, hard, soft, dists);
            }
        }
    }
}

/// The literal of a condition, including its side conditions.
fn cond_lit(bl: &mut Blaster, e: &Expr) -> Lit {
    let b = bl.expr(e);
    let l = bl.truth(&b);
    let mut side = bl.take_side();
    if side.is_empty() {
        l
    } else {
        side.push(l);
        bl.and_n(&side)
    }
}

fn item_vars(it: &Item, out: &mut Vec<VarId>) {
    match it {
        Item::Hard(e, _) | Item::Soft(e, _) | Item::Dist(e, _, _) => e.vars(out),
        Item::If(c, a, b) => {
            c.vars(out);
            for i in a.iter().chain(b) {
                item_vars(i, out);
            }
        }
    }
}

/// Checks a solution against the hard constraints.
pub fn check(p: &Problem, values: &[BigInt]) -> bool {
    let var = |id: VarId| values[id].clone();
    fn holds(it: &Item, var: &dyn Fn(VarId) -> BigInt) -> bool {
        let t = |e: &Expr| eval(e, var).is_some_and(|v| !v.is_zero());
        match it {
            Item::Hard(e, _) => t(e),
            Item::Soft(..) => true,
            Item::Dist(e, ranges, _) => eval(e, var).is_some_and(|v| {
                ranges
                    .iter()
                    .any(|r| !r.weight.is_zero() && r.lo <= v && v <= r.hi)
            }),
            Item::If(c, a, b) => {
                if t(c) {
                    a.iter().all(|i| holds(i, var))
                } else {
                    b.iter().all(|i| holds(i, var))
                }
            }
        }
    }
    p.items.iter().all(|i| holds(i, &var))
        && p.vars.iter().enumerate().all(|(id, v)| {
            v.domain.as_ref().is_none_or(|d| d.contains(&values[id]))
                && v.ty.wrap(&values[id]) == values[id]
        })
}
