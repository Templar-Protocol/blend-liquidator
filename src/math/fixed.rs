//! Checked fixed-point arithmetic with the contract's rounding.
//!
//! Every helper computes `x * y / denominator` (or `x * denominator / y`)
//! with mathematical floor or ceiling, widening to 256 bits when the
//! 128-bit product overflows, exactly as the pool's `soroban-fixed-point-math`
//! does. Nothing here panics: a zero divisor or a result outside `i128` is
//! an error, and callers decide what that means for a decision.

use ethnum::I256;

/// Scalar for 7-decimal values: factors, utilisation, XLM-style amounts.
pub const SCALAR_7: i128 = 10_000_000;
/// Scalar for 12-decimal values: v2 `b_rate` and `d_rate`.
pub const SCALAR_12: i128 = 1_000_000_000_000;
/// Seconds in a year, the accrual time base.
pub const SECONDS_PER_YEAR: i128 = 31_536_000;

/// Arithmetic that could not produce a valid `i128`, or inputs a formula
/// cannot accept. Carried through every `math` and `chain::xdr` result.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MathError {
    /// A zero denominator or divisor.
    #[error("division by zero")]
    DivisionByZero,
    /// The result does not fit in `i128`, or `10^decimals` does not.
    #[error("arithmetic overflow")]
    Overflow,
    /// An argument outside the formula's domain, named.
    #[error("invalid input: {0}")]
    InvalidInput(&'static str),
    /// A position references a reserve index the pool does not have.
    #[error("no reserve at index {0}")]
    MissingReserve(u32),
    /// A position references an asset the oracle snapshot has no price for.
    #[error("no oracle price for {0}")]
    MissingPrice(String),
}

#[derive(Clone, Copy)]
enum Rounding {
    Floor,
    Ceil,
}

/// floor(x * y / denominator).
pub fn mul_floor(x: i128, y: i128, denominator: i128) -> Result<i128, MathError> {
    mul_div(x, y, denominator, Rounding::Floor)
}

/// ceil(x * y / denominator).
pub fn mul_ceil(x: i128, y: i128, denominator: i128) -> Result<i128, MathError> {
    mul_div(x, y, denominator, Rounding::Ceil)
}

/// floor(x * denominator / y).
pub fn div_floor(x: i128, y: i128, denominator: i128) -> Result<i128, MathError> {
    mul_div(x, denominator, y, Rounding::Floor)
}

/// ceil(x * denominator / y).
pub fn div_ceil(x: i128, y: i128, denominator: i128) -> Result<i128, MathError> {
    mul_div(x, denominator, y, Rounding::Ceil)
}

/// `10^decimals`, the scalar of a token with that many decimals.
pub fn pow10(decimals: u32) -> Result<i128, MathError> {
    10_i128.checked_pow(decimals).ok_or(MathError::Overflow)
}

fn mul_div(x: i128, y: i128, z: i128, rounding: Rounding) -> Result<i128, MathError> {
    if z == 0 {
        return Err(MathError::DivisionByZero);
    }
    match x.checked_mul(y) {
        Some(product) => divide_narrow(product, z, rounding),
        None => divide_wide(I256::new(x) * I256::new(y), I256::new(z), rounding),
    }
}

/// Division with a sign-normalised divisor so `div_euclid` is a true floor.
fn divide_narrow(r: i128, z: i128, rounding: Rounding) -> Result<i128, MathError> {
    // `i128::MIN` has no positive `i128` counterpart, so negating it below
    // would report `Overflow` even when the true quotient fits comfortably
    // in `i128` (e.g. `i128::MIN / -2`). The 256-bit path has the headroom
    // to negate exactly and is already proven correct for every sign
    // combination, so route these two cases through it instead.
    if r == i128::MIN || z == i128::MIN {
        return divide_wide(I256::new(r), I256::new(z), rounding);
    }
    let (r, z) = if z < 0 {
        (
            r.checked_neg().ok_or(MathError::Overflow)?,
            z.checked_neg().ok_or(MathError::Overflow)?,
        )
    } else {
        (r, z)
    };
    let quotient = r.checked_div_euclid(z).ok_or(MathError::Overflow)?;
    match rounding {
        Rounding::Floor => Ok(quotient),
        Rounding::Ceil if r.rem_euclid(z) == 0 => Ok(quotient),
        Rounding::Ceil => quotient.checked_add(1).ok_or(MathError::Overflow),
    }
}

/// The 256-bit path: the product of two `i128`s always fits, only the
/// quotient may not.
fn divide_wide(r: I256, z: I256, rounding: Rounding) -> Result<i128, MathError> {
    // Both call sites reach here only after `mul_div` has already returned
    // `DivisionByZero` for `z == 0`, so `z` is never zero on this path.
    // Asserted rather than re-checked, so this module's "nothing here
    // panics" holds because the invariant is guaranteed upstream, not
    // because it goes unstated.
    debug_assert!(
        z != I256::ZERO,
        "divide_wide: z is guarded nonzero by mul_div"
    );
    let (r, z) = if z < I256::ZERO { (-r, -z) } else { (r, z) };
    let quotient = r.div_euclid(z);
    let quotient = match rounding {
        Rounding::Floor => quotient,
        Rounding::Ceil if r.rem_euclid(z) == I256::ZERO => quotient,
        Rounding::Ceil => quotient + I256::ONE,
    };
    i128::try_from(quotient).map_err(|_| MathError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_and_ceil_agree_with_mathematics_for_positive_values() {
        assert_eq!(mul_floor(3, 2, 4), Ok(1));
        assert_eq!(mul_ceil(3, 2, 4), Ok(2));
        assert_eq!(mul_floor(4, 2, 4), Ok(2));
        assert_eq!(mul_ceil(4, 2, 4), Ok(2));
        assert_eq!(div_floor(1, 3, SCALAR_7), Ok(3_333_333));
        assert_eq!(div_ceil(1, 3, SCALAR_7), Ok(3_333_334));
    }

    #[test]
    fn floor_and_ceil_agree_with_mathematics_for_negative_values() {
        // -1.5 floors to -2 and ceils to -1, as the contract's library does.
        assert_eq!(mul_floor(-3, 1, 2), Ok(-2));
        assert_eq!(mul_ceil(-3, 1, 2), Ok(-1));
        // A negative divisor is normalised first: 7 / -2 = -3.5.
        assert_eq!(mul_floor(7, 1, -2), Ok(-4));
        assert_eq!(mul_ceil(7, 1, -2), Ok(-3));
        assert_eq!(div_floor(-7, 2, 1), Ok(-4));
        assert_eq!(div_ceil(-7, 2, 1), Ok(-3));
    }

    #[test]
    fn zero_divisor_is_an_error_not_a_panic() {
        assert_eq!(mul_floor(1, 1, 0), Err(MathError::DivisionByZero));
        assert_eq!(mul_ceil(1, 1, 0), Err(MathError::DivisionByZero));
        assert_eq!(div_floor(1, 0, SCALAR_7), Err(MathError::DivisionByZero));
        assert_eq!(div_ceil(1, 0, SCALAR_7), Err(MathError::DivisionByZero));
    }

    #[test]
    fn product_overflow_widens_to_256_bits() {
        assert_eq!(mul_floor(i128::MAX, 3, 3), Ok(i128::MAX));
        assert_eq!(mul_ceil(i128::MAX, 3, 3), Ok(i128::MAX));
        assert_eq!(mul_floor(i128::MIN, 2, 2), Ok(i128::MIN));
        // 2^126 * 2^126 / 2^125 = 2^127, which does not fit.
        let big = 1_i128 << 126;
        assert_eq!(mul_floor(big, big, 1_i128 << 125), Err(MathError::Overflow));
        assert_eq!(mul_floor(i128::MAX, 2, 1), Err(MathError::Overflow));
    }

    #[test]
    fn i128_min_as_divisor_or_dividend_does_not_spuriously_overflow() {
        // Regression: sign-normalisation used to negate `i128::MIN`
        // directly, which has no positive `i128` counterpart, and reported
        // `Overflow` even though the true quotient fits. `1 / i128::MIN` is
        // a tiny negative fraction: floor is -1, ceil is 0.
        assert_eq!(mul_floor(1, 1, i128::MIN), Ok(-1));
        assert_eq!(mul_ceil(1, 1, i128::MIN), Ok(0));
        // i128::MIN / -2 = 2^126 exactly, well inside i128.
        assert_eq!(mul_floor(i128::MIN, 1, -2), Ok(1_i128 << 126));
        assert_eq!(mul_ceil(i128::MIN, 1, -2), Ok(1_i128 << 126));
        // Control: i128::MIN / -1 = 2^127, which truly does not fit.
        assert_eq!(mul_floor(i128::MIN, 1, -1), Err(MathError::Overflow));
    }

    #[test]
    fn pow10_covers_token_decimals_and_rejects_overflow() {
        assert_eq!(pow10(0), Ok(1));
        assert_eq!(pow10(7), Ok(SCALAR_7));
        assert_eq!(pow10(12), Ok(SCALAR_12));
        assert_eq!(
            pow10(38),
            Ok(100_000_000_000_000_000_000_000_000_000_000_000_000)
        );
        assert_eq!(pow10(39), Err(MathError::Overflow));
    }
}
