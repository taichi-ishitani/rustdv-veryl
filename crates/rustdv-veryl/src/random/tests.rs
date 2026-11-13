//! Differential tests: the bit-blasted circuit against the evaluator.

use num_bigint::BigInt;

use super::ir::{BinOp, Expr, Item, Origin, UnOp, eval};
use super::rng::SplitMix64;
use super::sat::batsat_impl::Batsat;
use super::solve::{Problem, VarInfo, solve};
use super::value::Ty;

const OPS: [BinOp; 18] = [
    BinOp::Add,
    BinOp::Sub,
    BinOp::Mul,
    BinOp::Div,
    BinOp::Rem,
    BinOp::And,
    BinOp::Or,
    BinOp::Xor,
    BinOp::Shl,
    BinOp::Shr,
    BinOp::Eq,
    BinOp::Ne,
    BinOp::Lt,
    BinOp::Le,
    BinOp::Gt,
    BinOp::Ge,
    BinOp::LAnd,
    BinOp::LOr,
];

fn random_ty(r: &mut SplitMix64) -> Ty {
    let w = 1 + r.below(9) as u32;
    if r.below(2) == 0 {
        Ty::unsigned(w)
    } else {
        Ty::signed(w.max(2))
    }
}

fn random_expr(r: &mut SplitMix64, vars: &[Ty], depth: u32) -> Expr {
    if depth == 0 || r.below(4) == 0 {
        return if r.below(3) == 0 {
            let ty = random_ty(r);
            let v = ty.wrap(&BigInt::from(r.next_u64() as i64));
            Expr::Const(v, ty)
        } else {
            let id = r.below(vars.len() as u64) as usize;
            Expr::Var(id, vars[id])
        };
    }
    match r.below(8) {
        0 => {
            let op = [UnOp::Neg, UnOp::LNot, UnOp::BitNot][r.below(3) as usize];
            Expr::un(op, random_expr(r, vars, depth - 1))
        }
        1 => Expr::cast(random_expr(r, vars, depth - 1), random_ty(r)),
        2 => {
            let c = random_expr(r, vars, depth - 1);
            let a = random_expr(r, vars, depth - 1);
            let b = random_expr(r, vars, depth - 1);
            Expr::ite(c, a, b)
        }
        _ => {
            let op = OPS[r.below(OPS.len() as u64) as usize];
            let a = random_expr(r, vars, depth - 1);
            let b = random_expr(r, vars, depth - 1);
            Expr::bin(op, a, b)
        }
    }
}

fn problem(vars: &[Ty], items: Vec<Item>) -> Problem {
    Problem {
        owner: "diff".to_string(),
        vars: vars
            .iter()
            .enumerate()
            .map(|(i, t)| VarInfo {
                name: format!("v{i}"),
                ty: *t,
                domain: None,
            })
            .collect(),
        origins: vec![Origin {
            block: "diff".to_string(),
            loc: String::new(),
            text: String::new(),
        }],
        items,
        ..Problem::default()
    }
}

#[test]
fn circuit_matches_evaluator() {
    let mut r = SplitMix64::new(0x5eed);
    let mut checked = 0;
    for _ in 0..600 {
        let vars: Vec<Ty> = (0..3).map(|_| random_ty(&mut r)).collect();
        let e = random_expr(&mut r, &vars, 3);
        let values: Vec<BigInt> = vars
            .iter()
            .map(|t| t.wrap(&BigInt::from(r.next_u64() as i64)))
            .collect();
        let Some(expected) = eval(&e, &|id| values[id].clone()) else {
            continue;
        };
        assert_eq!(e.ty().wrap(&expected), expected, "value in range: {e:?}");
        let fix: Vec<Item> = values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                Item::Hard(
                    Expr::bin(
                        BinOp::Eq,
                        Expr::Var(i, vars[i]),
                        Expr::Const(v.clone(), vars[i]),
                    ),
                    0,
                )
            })
            .collect();
        let k = Expr::Const(expected.clone(), e.ty());

        let mut items = fix.clone();
        items.push(Item::Hard(Expr::bin(BinOp::Eq, e.clone(), k.clone()), 0));
        let p = problem(&vars, items);
        let mut sat = Batsat::new(1);
        let got = solve(&p, &mut r, &mut sat, None);
        assert!(got.is_ok(), "{e:?} == {expected} with {values:?}");

        let mut items = fix;
        items.push(Item::Hard(Expr::bin(BinOp::Ne, e.clone(), k), 0));
        let p = problem(&vars, items);
        let mut sat = Batsat::new(1);
        let got = solve(&p, &mut r, &mut sat, None);
        assert!(got.is_err(), "{e:?} != {expected} with {values:?}");
        checked += 1;
    }
    assert!(checked > 400, "{checked}");
}
