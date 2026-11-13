//! Bit-blasting: turns typed expressions into CNF with Tseitin gates.

use num_bigint::BigInt;

use super::ir::{BinOp, Expr, UnOp, VarId};
use super::sat::{Lit, SatSolver};
use super::value::Ty;

/// A bit vector: literals, least significant bit first.
#[derive(Clone, Debug)]
pub struct Bv {
    pub bits: Vec<Lit>,
    pub ty: Ty,
}

pub struct Blaster<'s> {
    pub sat: &'s mut dyn SatSolver,
    t: Lit,
    vars: Vec<Option<Vec<Lit>>>,
    /// Conditions that must hold for the expressions encoded since the
    /// last `take_side` (non-zero divisors).
    side: Vec<Lit>,
}

impl<'s> Blaster<'s> {
    pub fn new(sat: &'s mut dyn SatSolver) -> Blaster<'s> {
        let v = sat.new_var(None);
        let t = Lit::pos(v);
        sat.add_clause(&[t]);
        Blaster {
            sat,
            t,
            vars: Vec::new(),
            side: Vec::new(),
        }
    }

    pub fn tru(&self) -> Lit {
        self.t
    }

    pub fn fls(&self) -> Lit {
        !self.t
    }

    /// Allocates the bits of a variable, with the given preferred values.
    pub fn declare_var(&mut self, id: VarId, ty: Ty, polarity: &[bool]) {
        let bits = (0..ty.width)
            .map(|i| Lit::pos(self.sat.new_var(polarity.get(i as usize).copied())))
            .collect();
        if self.vars.len() <= id {
            self.vars.resize(id + 1, None);
        }
        self.vars[id] = Some(bits);
    }

    pub fn var_bits(&self, id: VarId) -> &[Lit] {
        self.vars[id].as_deref().expect("declared variable")
    }

    pub fn take_side(&mut self) -> Vec<Lit> {
        std::mem::take(&mut self.side)
    }

    pub fn new_lit(&mut self) -> Lit {
        Lit::pos(self.sat.new_var(None))
    }

    // ---- gates ----

    fn is_t(&self, l: Lit) -> bool {
        l == self.t
    }

    fn is_f(&self, l: Lit) -> bool {
        l == !self.t
    }

    pub fn and2(&mut self, a: Lit, b: Lit) -> Lit {
        if self.is_f(a) || self.is_f(b) || a == !b {
            return self.fls();
        }
        if self.is_t(a) || a == b {
            return b;
        }
        if self.is_t(b) {
            return a;
        }
        let o = self.new_lit();
        self.sat.add_clause(&[!o, a]);
        self.sat.add_clause(&[!o, b]);
        self.sat.add_clause(&[o, !a, !b]);
        o
    }

    pub fn or2(&mut self, a: Lit, b: Lit) -> Lit {
        !self.and2(!a, !b)
    }

    pub fn xor2(&mut self, a: Lit, b: Lit) -> Lit {
        if self.is_f(a) {
            return b;
        }
        if self.is_f(b) {
            return a;
        }
        if self.is_t(a) {
            return !b;
        }
        if self.is_t(b) {
            return !a;
        }
        if a == b {
            return self.fls();
        }
        if a == !b {
            return self.tru();
        }
        let o = self.new_lit();
        self.sat.add_clause(&[!o, a, b]);
        self.sat.add_clause(&[!o, !a, !b]);
        self.sat.add_clause(&[o, !a, b]);
        self.sat.add_clause(&[o, a, !b]);
        o
    }

    /// `s ? a : b`
    pub fn mux(&mut self, s: Lit, a: Lit, b: Lit) -> Lit {
        if self.is_t(s) || a == b {
            return a;
        }
        if self.is_f(s) {
            return b;
        }
        let o = self.new_lit();
        self.sat.add_clause(&[!s, !a, o]);
        self.sat.add_clause(&[!s, a, !o]);
        self.sat.add_clause(&[s, !b, o]);
        self.sat.add_clause(&[s, b, !o]);
        o
    }

    pub fn and_n(&mut self, lits: &[Lit]) -> Lit {
        let mut acc = self.tru();
        for l in lits {
            acc = self.and2(acc, *l);
        }
        acc
    }

    pub fn or_n(&mut self, lits: &[Lit]) -> Lit {
        let mut acc = self.fls();
        for l in lits {
            acc = self.or2(acc, *l);
        }
        acc
    }

    // ---- bit vectors ----

    pub fn konst(&self, v: &BigInt, ty: Ty) -> Bv {
        let p = ty.pattern(v);
        let bits = (0..ty.width)
            .map(|i| {
                if p.bit(i as u64) {
                    self.tru()
                } else {
                    self.fls()
                }
            })
            .collect();
        Bv { bits, ty }
    }

    /// The bits of `a` extended (or truncated) to `w` bits, by `a`'s sign.
    fn ext(&self, a: &Bv, w: u32) -> Vec<Lit> {
        let mut bits = a.bits.clone();
        let fill = if a.ty.signed {
            *a.bits.last().expect("width >= 1")
        } else {
            self.fls()
        };
        bits.resize(w as usize, fill);
        bits
    }

    fn add_bits(&mut self, a: &[Lit], b: &[Lit], mut carry: Lit) -> Vec<Lit> {
        let mut out = Vec::with_capacity(a.len());
        for i in 0..a.len() {
            let x = self.xor2(a[i], b[i]);
            out.push(self.xor2(x, carry));
            let c1 = self.and2(a[i], b[i]);
            let c2 = self.and2(x, carry);
            carry = self.or2(c1, c2);
        }
        out
    }

    fn neg_bits(&mut self, a: &[Lit]) -> Vec<Lit> {
        let inv: Vec<Lit> = a.iter().map(|l| !*l).collect();
        let zero = vec![self.fls(); a.len()];
        let t = self.tru();
        self.add_bits(&inv, &zero, t)
    }

    fn sub_bits(&mut self, a: &[Lit], b: &[Lit]) -> Vec<Lit> {
        let inv: Vec<Lit> = b.iter().map(|l| !*l).collect();
        let t = self.tru();
        self.add_bits(a, &inv, t)
    }

    fn mul_bits(&mut self, a: &[Lit], b: &[Lit]) -> Vec<Lit> {
        let w = a.len();
        let mut acc = vec![self.fls(); w];
        for (i, bi) in b.iter().enumerate() {
            if self.is_f(*bi) {
                continue;
            }
            let mut part = vec![self.fls(); w];
            for j in 0..w - i {
                part[i + j] = self.and2(a[j], *bi);
            }
            let f = self.fls();
            acc = self.add_bits(&acc, &part, f);
        }
        acc
    }

    /// Unsigned `a < b` for equal widths.
    fn ult_bits(&mut self, a: &[Lit], b: &[Lit]) -> Lit {
        // a < b iff a - b borrows: no carry out of a + !b + 1.
        let mut carry = self.tru();
        for i in 0..a.len() {
            let nb = !b[i];
            let x = self.xor2(a[i], nb);
            let c1 = self.and2(a[i], nb);
            let c2 = self.and2(x, carry);
            carry = self.or2(c1, c2);
        }
        !carry
    }

    fn eq_bits(&mut self, a: &[Lit], b: &[Lit]) -> Lit {
        let mut acc = self.tru();
        for i in 0..a.len() {
            let x = self.xor2(a[i], b[i]);
            acc = self.and2(acc, !x);
        }
        acc
    }

    /// Unsigned division at equal widths: (quotient, remainder). The
    /// divisor must be non-zero.
    fn udivrem_bits(&mut self, a: &[Lit], b: &[Lit]) -> (Vec<Lit>, Vec<Lit>) {
        let w = a.len();
        let mut rem = vec![self.fls(); w];
        let mut q = vec![self.fls(); w];
        // Work one bit wider so that the shifted remainder never overflows.
        let bw: Vec<Lit> = b.iter().copied().chain([self.fls()]).collect();
        for i in (0..w).rev() {
            let mut r: Vec<Lit> = vec![a[i]];
            r.extend_from_slice(&rem);
            let lt = self.ult_bits(&r, &bw);
            let diff = self.sub_bits(&r, &bw);
            q[i] = !lt;
            for j in 0..w {
                rem[j] = self.mux(lt, r[j], diff[j]);
            }
        }
        (q, rem)
    }

    fn abs_bits(&mut self, a: &[Lit]) -> Vec<Lit> {
        let s = *a.last().expect("width >= 1");
        let n = self.neg_bits(a);
        (0..a.len()).map(|i| self.mux(s, n[i], a[i])).collect()
    }

    fn cond_neg(&mut self, s: Lit, a: &[Lit]) -> Vec<Lit> {
        let n = self.neg_bits(a);
        (0..a.len()).map(|i| self.mux(s, n[i], a[i])).collect()
    }

    fn shift_bits(&mut self, a: &Bv, amount: &Bv, left: bool) -> Vec<Lit> {
        let w = a.bits.len();
        let fill = if !left && a.ty.signed {
            *a.bits.last().expect("width >= 1")
        } else {
            self.fls()
        };
        let mut cur = a.bits.clone();
        let mut big = self.fls();
        for (k, bit) in amount.bits.iter().enumerate() {
            let step = 1usize.checked_shl(k as u32).unwrap_or(usize::MAX);
            if step >= w {
                big = self.or2(big, *bit);
                continue;
            }
            let shifted: Vec<Lit> = (0..w)
                .map(|i| {
                    if left {
                        if i >= step { cur[i - step] } else { self.fls() }
                    } else if i + step < w {
                        cur[i + step]
                    } else {
                        fill
                    }
                })
                .collect();
            cur = (0..w).map(|i| self.mux(*bit, shifted[i], cur[i])).collect();
        }
        (0..w).map(|i| self.mux(big, fill, cur[i])).collect()
    }

    /// `Some(k)` when `b` is the constant `2^k`.
    fn const_pow2(&self, b: &Bv) -> Option<usize> {
        let mut k = None;
        for (i, l) in b.bits.iter().enumerate() {
            if self.is_t(*l) {
                if k.is_some() {
                    return None;
                }
                k = Some(i);
            } else if !self.is_f(*l) {
                return None;
            }
        }
        k
    }

    /// The truth value of `a`: any bit set.
    pub fn truth(&mut self, a: &Bv) -> Lit {
        let bits = a.bits.clone();
        self.or_n(&bits)
    }

    // ---- expressions ----

    pub fn expr(&mut self, e: &Expr) -> Bv {
        match e {
            Expr::Const(v, ty) => self.konst(v, *ty),
            Expr::Var(id, ty) => Bv {
                bits: self.var_bits(*id).to_vec(),
                ty: *ty,
            },
            Expr::Skip => unreachable!("items with Skip are dropped"),
            Expr::Ite(c, a, b, ty) => {
                let c = self.expr(c);
                let s = self.truth(&c);
                // Side conditions of a branch hold only when it is taken.
                let n0 = self.side.len();
                let a = self.expr(a);
                for i in n0..self.side.len() {
                    self.side[i] = self.or2(!s, self.side[i]);
                }
                let n1 = self.side.len();
                let b = self.expr(b);
                for i in n1..self.side.len() {
                    self.side[i] = self.or2(s, self.side[i]);
                }
                let (ab, bb) = (self.ext(&a, ty.width), self.ext(&b, ty.width));
                let bits = (0..ty.width as usize)
                    .map(|i| self.mux(s, ab[i], bb[i]))
                    .collect();
                Bv { bits, ty: *ty }
            }
            Expr::Un(op, a, ty) => {
                let a = self.expr(a);
                let bits = match op {
                    UnOp::Neg => {
                        let x = self.ext(&a, ty.width);
                        self.neg_bits(&x)
                    }
                    UnOp::LNot => vec![!self.truth(&a)],
                    UnOp::BitNot => a.bits.iter().map(|l| !*l).collect(),
                    UnOp::Cast => self.ext(&a, ty.width),
                };
                Bv { bits, ty: *ty }
            }
            Expr::Bin(op, a, b, ty) => {
                let a = self.expr(a);
                let b = self.expr(b);
                let bits = self.bin(*op, &a, &b, *ty);
                Bv { bits, ty: *ty }
            }
        }
    }

    fn bin(&mut self, op: BinOp, a: &Bv, b: &Bv, ty: Ty) -> Vec<Lit> {
        use BinOp::*;
        let w = ty.width;
        // Unsigned division by a constant power of two: a shift or a mask.
        if matches!(op, Div | Rem)
            && !a.ty.signed
            && !b.ty.signed
            && let Some(k) = self.const_pow2(b)
        {
            let mut out: Vec<Lit> = if op == Div {
                a.bits.iter().skip(k).copied().collect()
            } else {
                a.bits.iter().take(k).copied().collect()
            };
            out.resize(w as usize, self.fls());
            return out;
        }
        match op {
            Add => {
                let (x, y) = (self.ext(a, w), self.ext(b, w));
                let f = self.fls();
                self.add_bits(&x, &y, f)
            }
            Sub => {
                let (x, y) = (self.ext(a, w), self.ext(b, w));
                self.sub_bits(&x, &y)
            }
            Mul => {
                let (x, y) = (self.ext(a, w), self.ext(b, w));
                self.mul_bits(&x, &y)
            }
            Div | Rem => {
                let nz = self.truth(b);
                self.side.push(nz);
                if !a.ty.signed && !b.ty.signed {
                    let cw = a.ty.width.max(b.ty.width);
                    let (x, y) = (self.ext(a, cw), self.ext(b, cw));
                    let (q, r) = self.udivrem_bits(&x, &y);
                    let mut out = if op == Div { q } else { r };
                    out.truncate(w as usize);
                    out
                } else {
                    let cw = a.ty.as_signed_width().max(b.ty.as_signed_width()) + 1;
                    let sa = Bv {
                        bits: self.ext(a, cw),
                        ty: Ty::signed(cw),
                    };
                    let sb = Bv {
                        bits: self.ext(b, cw),
                        ty: Ty::signed(cw),
                    };
                    let (na, nb) = (sa.bits[cw as usize - 1], sb.bits[cw as usize - 1]);
                    let (x, y) = (self.abs_bits(&sa.bits), self.abs_bits(&sb.bits));
                    let (q, r) = self.udivrem_bits(&x, &y);
                    let mut out = if op == Div {
                        let s = self.xor2(na, nb);
                        self.cond_neg(s, &q)
                    } else {
                        self.cond_neg(na, &r)
                    };
                    out.truncate(w as usize);
                    out
                }
            }
            And | Or | Xor => {
                let (x, y) = (self.ext(a, w), self.ext(b, w));
                (0..w as usize)
                    .map(|i| match op {
                        And => self.and2(x[i], y[i]),
                        Or => self.or2(x[i], y[i]),
                        _ => self.xor2(x[i], y[i]),
                    })
                    .collect()
            }
            Shl => self.shift_bits(a, b, true),
            Shr => self.shift_bits(a, b, false),
            Eq | Ne | Lt | Le | Gt | Ge => {
                let (x, y, signed) = if a.ty.signed || b.ty.signed {
                    let cw = a.ty.as_signed_width().max(b.ty.as_signed_width());
                    (self.ext(a, cw), self.ext(b, cw), true)
                } else {
                    let cw = a.ty.width.max(b.ty.width);
                    (self.ext(a, cw), self.ext(b, cw), false)
                };
                let lt = |s: &mut Self, x: &[Lit], y: &[Lit]| {
                    if signed {
                        // Flip the sign bits to compare as unsigned.
                        let n = x.len() - 1;
                        let mut x2 = x.to_vec();
                        let mut y2 = y.to_vec();
                        x2[n] = !x2[n];
                        y2[n] = !y2[n];
                        s.ult_bits(&x2, &y2)
                    } else {
                        s.ult_bits(x, y)
                    }
                };
                let l = match op {
                    Eq => self.eq_bits(&x, &y),
                    Ne => !self.eq_bits(&x, &y),
                    Lt => lt(self, &x, &y),
                    Gt => lt(self, &y, &x),
                    Le => !lt(self, &y, &x),
                    _ => !lt(self, &x, &y),
                };
                vec![l]
            }
            LAnd | LOr => {
                let (x, y) = (self.truth(a), self.truth(b));
                vec![if op == LAnd {
                    self.and2(x, y)
                } else {
                    self.or2(x, y)
                }]
            }
        }
    }

    /// The value of a variable in the last model.
    pub fn model_value(&self, id: VarId, ty: Ty) -> BigInt {
        let bits = self.var_bits(id);
        let mut p = BigInt::from(0u8);
        for (i, l) in bits.iter().enumerate() {
            if self.sat.value(l.var) == l.positive {
                p.set_bit(i as u64, true);
            }
        }
        ty.wrap(&p)
    }
}
