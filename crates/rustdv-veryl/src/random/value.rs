//! Values that can be random variables or constants in constraints.

use num_bigint::{BigInt, BigUint, Sign};
use num_traits::{One, Zero};

/// The width and signedness of a value in a constraint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ty {
    pub width: u32,
    pub signed: bool,
}

impl Ty {
    pub const BOOL: Ty = Ty {
        width: 1,
        signed: false,
    };

    pub const fn unsigned(width: u32) -> Ty {
        Ty {
            width,
            signed: false,
        }
    }

    pub const fn signed(width: u32) -> Ty {
        Ty {
            width,
            signed: true,
        }
    }

    /// The smallest type holding `v`: unsigned for a non-negative value.
    pub fn of_value(v: &BigInt) -> Ty {
        if v.sign() == Sign::Minus {
            // -2^(n-1) needs n bits; -(2^(n-1)) - 1 needs n + 1.
            let m: BigInt = -v - 1;
            Ty::signed(m.bits() as u32 + 1)
        } else {
            Ty::unsigned((v.bits() as u32).max(1))
        }
    }

    /// The smallest signed width that holds every value of `self`.
    pub fn as_signed_width(self) -> u32 {
        if self.signed {
            self.width
        } else {
            self.width + 1
        }
    }

    /// Wraps `v` into the range of this type, as a two's complement
    /// truncation.
    pub fn wrap(self, v: &BigInt) -> BigInt {
        let modulus = BigInt::one() << self.width;
        let mut p = v % &modulus;
        if p.sign() == Sign::Minus {
            p += &modulus;
        }
        if self.signed && p.bit(self.width as u64 - 1) {
            p - modulus
        } else {
            p
        }
    }

    /// The bit pattern of `v` (already in range) at this width.
    pub fn pattern(self, v: &BigInt) -> BigUint {
        let w = Ty::unsigned(self.width).wrap(v);
        w.to_biguint().expect("non-negative after wrapping")
    }

    pub fn min(self) -> BigInt {
        if self.signed {
            -(BigInt::one() << (self.width - 1))
        } else {
            BigInt::zero()
        }
    }

    pub fn max(self) -> BigInt {
        if self.signed {
            (BigInt::one() << (self.width - 1)) - 1
        } else {
            (BigInt::one() << self.width) - 1
        }
    }
}

/// A value that can be a random variable or a constant in a constraint:
/// `bool`, the integers up to 128 bits, `BigUint` (with an explicit
/// width) and enums deriving [`RandEnum`](crate::random::RandEnum).
pub trait RandValue: Sized {
    /// The type of the value; `None` when the width must be given with
    /// `#[rand(width = N)]` (`BigUint`).
    const TY: Option<Ty>;

    /// The values the variable may take, when not every bit pattern is
    /// valid (an enum's variants).
    fn domain() -> Option<Vec<BigInt>> {
        None
    }

    fn to_bigint(&self) -> BigInt;

    /// Converts a solved value back. `v` is in the range of the type.
    fn from_bigint(v: &BigInt) -> Self;
}

macro_rules! int_value {
    ($($t:ty, $signed:expr;)*) => {$(
        impl RandValue for $t {
            const TY: Option<Ty> = Some(Ty {
                width: <$t>::BITS,
                signed: $signed,
            });

            fn to_bigint(&self) -> BigInt {
                BigInt::from(*self)
            }

            fn from_bigint(v: &BigInt) -> Self {
                <$t>::try_from(v).expect("solved value in range")
            }
        }
    )*};
}

int_value! {
    u8, false; u16, false; u32, false; u64, false; u128, false;
    i8, true; i16, true; i32, true; i64, true; i128, true;
}

impl RandValue for bool {
    const TY: Option<Ty> = Some(Ty::BOOL);

    fn to_bigint(&self) -> BigInt {
        BigInt::from(*self as u8)
    }

    fn from_bigint(v: &BigInt) -> Self {
        !v.is_zero()
    }
}

impl RandValue for BigUint {
    const TY: Option<Ty> = None;

    fn to_bigint(&self) -> BigInt {
        BigInt::from(self.clone())
    }

    fn from_bigint(v: &BigInt) -> Self {
        v.to_biguint().expect("solved value in range")
    }
}

/// `usize` is only used for constants (loop indices, lengths).
impl RandValue for usize {
    const TY: Option<Ty> = Some(Ty::unsigned(usize::BITS));

    fn to_bigint(&self) -> BigInt {
        BigInt::from(*self)
    }

    fn from_bigint(v: &BigInt) -> Self {
        usize::try_from(v).expect("solved value in range")
    }
}

/// The smallest type holding every value of both types.
pub fn join_holding(a: Ty, b: Ty) -> Ty {
    if a.signed || b.signed {
        Ty::signed(a.as_signed_width().max(b.as_signed_width()))
    } else {
        Ty::unsigned(a.width.max(b.width))
    }
}
