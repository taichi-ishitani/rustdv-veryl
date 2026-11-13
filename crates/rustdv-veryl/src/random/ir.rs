//! The constraint IR: typed expressions and constraint items.
//!
//! Expressions are typed when they are built (see `random-design.md` and
//! the semantics in `random.md`): `+`, `-`, `*` never overflow, bitwise
//! operators and shifts work at the width of their operands.

use num_bigint::BigInt;
use num_traits::{One, ToPrimitive, Zero};

use super::value::Ty;

pub type VarId = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    /// Arithmetic negation (exact).
    Neg,
    /// Logical NOT: `x == 0`.
    LNot,
    /// Bitwise complement at the operand's width.
    BitNot,
    /// Conversion to the expression's type, as Rust `as` does.
    Cast,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    And,
    Or,
    Xor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    LAnd,
    LOr,
}

impl BinOp {
    pub fn is_compare(self) -> bool {
        matches!(
            self,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
        )
    }
}

#[derive(Clone, Debug)]
pub enum Expr {
    Const(BigInt, Ty),
    Var(VarId, Ty),
    Un(UnOp, Box<Expr>, Ty),
    Bin(BinOp, Box<Expr>, Box<Expr>, Ty),
    /// `if c { a } else { b }` as a value.
    Ite(Box<Expr>, Box<Expr>, Box<Expr>, Ty),
    /// A value not known in this phase (an element of a `Vec` while its
    /// size is being solved). An item containing it is dropped.
    Skip,
}

impl Expr {
    pub fn ty(&self) -> Ty {
        match self {
            Expr::Const(_, t) | Expr::Var(_, t) | Expr::Un(_, _, t) => *t,
            Expr::Bin(_, _, _, t) | Expr::Ite(_, _, _, t) => *t,
            Expr::Skip => Ty::BOOL,
        }
    }

    pub fn konst(v: BigInt) -> Expr {
        let ty = Ty::of_value(&v);
        Expr::Const(v, ty)
    }

    pub fn konst_ty(v: BigInt, ty: Ty) -> Expr {
        Expr::Const(ty.wrap(&v), ty)
    }

    pub fn bool(b: bool) -> Expr {
        Expr::Const(BigInt::from(b as u8), Ty::BOOL)
    }

    pub fn as_const(&self) -> Option<&BigInt> {
        match self {
            Expr::Const(v, _) => Some(v),
            _ => None,
        }
    }

    pub fn has_skip(&self) -> bool {
        match self {
            Expr::Skip => true,
            Expr::Const(..) | Expr::Var(..) => false,
            Expr::Un(_, a, _) => a.has_skip(),
            Expr::Bin(_, a, b, _) => a.has_skip() || b.has_skip(),
            Expr::Ite(c, a, b, _) => c.has_skip() || a.has_skip() || b.has_skip(),
        }
    }

    pub fn vars(&self, out: &mut Vec<VarId>) {
        match self {
            Expr::Var(v, _) => out.push(*v),
            Expr::Const(..) | Expr::Skip => {}
            Expr::Un(_, a, _) => a.vars(out),
            Expr::Bin(_, a, b, _) => {
                a.vars(out);
                b.vars(out);
            }
            Expr::Ite(c, a, b, _) => {
                c.vars(out);
                a.vars(out);
                b.vars(out);
            }
        }
    }

    // ---- constructors with typing and constant folding ----

    pub fn un(op: UnOp, a: Expr) -> Expr {
        let ty = match op {
            UnOp::Neg => Ty::signed(a.ty().as_signed_width() + 1),
            UnOp::LNot => Ty::BOOL,
            UnOp::BitNot => a.ty(),
            UnOp::Cast => unreachable!("use Expr::cast"),
        };
        Self::fold(Expr::Un(op, Box::new(a), ty))
    }

    pub fn cast(a: Expr, ty: Ty) -> Expr {
        if a.ty() == ty {
            return a;
        }
        Self::fold(Expr::Un(UnOp::Cast, Box::new(a), ty))
    }

    pub fn bin(op: BinOp, a: Expr, b: Expr) -> Expr {
        let ty = bin_ty(op, a.ty(), b.ty());
        Self::fold(Expr::Bin(op, Box::new(a), Box::new(b), ty))
    }

    pub fn ite(c: Expr, a: Expr, b: Expr) -> Expr {
        let ty = super::value::join_holding(a.ty(), b.ty());
        if let Some(v) = c.as_const() {
            let pick = if v.is_zero() { b } else { a };
            return Expr::cast(pick, ty);
        }
        Expr::Ite(Box::new(c), Box::new(a), Box::new(b), ty)
    }

    /// `e != 0`, unless `e` is already a truth value.
    pub fn truth(e: Expr) -> Expr {
        if e.ty() == Ty::BOOL {
            e
        } else {
            let zero = Expr::Const(BigInt::zero(), Ty::unsigned(1));
            Expr::bin(BinOp::Ne, e, zero)
        }
    }

    fn fold(e: Expr) -> Expr {
        let all_const = match &e {
            Expr::Un(_, a, _) => a.as_const().is_some(),
            Expr::Bin(op, a, b, _) => {
                // Short-circuit logical operators with one constant side.
                if matches!(op, BinOp::LAnd | BinOp::LOr) {
                    let lit = |x: &Expr| x.as_const().map(|v| !v.is_zero());
                    match (op, lit(a), lit(b)) {
                        (BinOp::LAnd, Some(false), _) | (BinOp::LAnd, _, Some(false)) => {
                            return Expr::bool(false);
                        }
                        (BinOp::LOr, Some(true), _) | (BinOp::LOr, _, Some(true)) => {
                            return Expr::bool(true);
                        }
                        (_, Some(_), None) => return Expr::truth((**b).clone()),
                        (_, None, Some(_)) => return Expr::truth((**a).clone()),
                        _ => {}
                    }
                }
                a.as_const().is_some() && b.as_const().is_some()
            }
            _ => false,
        };
        if all_const && let Some(v) = eval(&e, &|_| unreachable!()) {
            return Expr::Const(v, e.ty());
        }
        e
    }
}

/// The type of a binary operation.
pub fn bin_ty(op: BinOp, a: Ty, b: Ty) -> Ty {
    use BinOp::*;
    match op {
        Add | Sub | Mul => {
            if !a.signed && !b.signed {
                match op {
                    Add => Ty::unsigned(a.width.max(b.width) + 1),
                    Sub => Ty::signed(a.width.max(b.width) + 1),
                    _ => Ty::unsigned(a.width + b.width),
                }
            } else {
                let (wa, wb) = (a.as_signed_width(), b.as_signed_width());
                match op {
                    Mul => Ty::signed(wa + wb),
                    _ => Ty::signed(wa.max(wb) + 1),
                }
            }
        }
        Div => {
            if !a.signed && !b.signed {
                a
            } else {
                Ty::signed(a.as_signed_width() + 1)
            }
        }
        Rem => {
            if !a.signed && !b.signed {
                b
            } else {
                Ty::signed(a.as_signed_width().max(b.as_signed_width()))
            }
        }
        And | Or | Xor => Ty {
            width: a.width.max(b.width),
            signed: a.signed && b.signed,
        },
        Shl | Shr => a,
        Eq | Ne | Lt | Le | Gt | Ge | LAnd | LOr => Ty::BOOL,
    }
}

/// Evaluates an expression; `None` when a divisor is zero.
pub fn eval(e: &Expr, var: &dyn Fn(VarId) -> BigInt) -> Option<BigInt> {
    Some(match e {
        Expr::Const(v, _) => v.clone(),
        Expr::Var(id, _) => var(*id),
        Expr::Skip => BigInt::zero(),
        Expr::Ite(c, a, b, ty) => {
            let v = if eval(c, var)?.is_zero() {
                eval(b, var)?
            } else {
                eval(a, var)?
            };
            ty.wrap(&v)
        }
        Expr::Un(op, a, ty) => {
            let x = eval(a, var)?;
            match op {
                UnOp::Neg => -x,
                UnOp::LNot => BigInt::from(x.is_zero() as u8),
                UnOp::BitNot => {
                    let t = a.ty();
                    let p = BigInt::from(t.pattern(&x));
                    let mask = (BigInt::one() << t.width) - 1;
                    t.wrap(&(p ^ mask))
                }
                UnOp::Cast => ty.wrap(&x),
            }
        }
        Expr::Bin(op, a, b, ty) => {
            let x = eval(a, var)?;
            let y = eval(b, var)?;
            let flag = |c: bool| BigInt::from(c as u8);
            use BinOp::*;
            match op {
                Add => x + y,
                Sub => x - y,
                Mul => x * y,
                Div => {
                    if y.is_zero() {
                        return None;
                    }
                    ty.wrap(&(x / y))
                }
                Rem => {
                    if y.is_zero() {
                        return None;
                    }
                    ty.wrap(&(x % y))
                }
                And | Or | Xor => {
                    // Two's complement patterns at the result width: a
                    // negative value is sign-extended by the wrapping.
                    let w = Ty::unsigned(ty.width);
                    let px = BigInt::from(w.pattern(&x));
                    let py = BigInt::from(w.pattern(&y));
                    let r = match op {
                        And => px & py,
                        Or => px | py,
                        _ => px ^ py,
                    };
                    ty.wrap(&r)
                }
                Shl | Shr => {
                    let n = shift_amount(&y, b.ty());
                    let t = a.ty();
                    if *op == Shl {
                        if n >= t.width as u64 {
                            BigInt::zero()
                        } else {
                            t.wrap(&(x << n))
                        }
                    } else if t.signed {
                        // Arithmetic shift: floor division by 2^n.
                        let n = n.min(t.width as u64);
                        x >> n
                    } else if n >= t.width as u64 {
                        BigInt::zero()
                    } else {
                        x >> n
                    }
                }
                Eq => flag(x == y),
                Ne => flag(x != y),
                Lt => flag(x < y),
                Le => flag(x <= y),
                Gt => flag(x > y),
                Ge => flag(x >= y),
                LAnd => flag(!x.is_zero() && !y.is_zero()),
                LOr => flag(!x.is_zero() || !y.is_zero()),
            }
        }
    })
}

/// A shift amount: the unsigned bit pattern of the right operand.
fn shift_amount(y: &BigInt, ty: Ty) -> u64 {
    let p = BigInt::from(ty.pattern(y));
    if p.bits() > 32 {
        u64::MAX
    } else {
        p.to_u64().unwrap_or(u64::MAX)
    }
}

/// Where a constraint item comes from, for the unsat report.
#[derive(Clone, Debug)]
pub struct Origin {
    pub block: String,
    pub loc: String,
    pub text: String,
}

pub type OriginId = usize;

/// A weighted range of a `dist`.
#[derive(Clone, Debug)]
pub struct DistRange {
    pub lo: BigInt,
    pub hi: BigInt,
    /// The weight of the whole range.
    pub weight: BigInt,
}

#[derive(Clone, Debug)]
pub enum Item {
    /// The expression must be true.
    Hard(Expr, OriginId),
    /// Under `cond`, `then` holds; otherwise `els` holds.
    If(Expr, Vec<Item>, Vec<Item>),
    /// Kept if the other constraints allow; `priority` grows with the
    /// order of blocks (later blocks win).
    Soft(Expr, usize),
    /// `expr` is in one of the ranges with a non-zero weight, and its
    /// value is picked by the weights where the other constraints allow.
    Dist(Expr, Vec<DistRange>, OriginId),
}

impl Item {
    pub fn has_skip(&self) -> bool {
        match self {
            Item::Hard(e, _) | Item::Soft(e, _) | Item::Dist(e, _, _) => e.has_skip(),
            Item::If(c, a, b) => {
                c.has_skip() || a.iter().any(Item::has_skip) || b.iter().any(Item::has_skip)
            }
        }
    }
}
