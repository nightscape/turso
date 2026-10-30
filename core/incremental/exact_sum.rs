//! Exact sum of REAL values for SUM/TOTAL/AVG in materialized views.
//!
//! Rows are both added and retracted, so the sum must give back exactly what
//! it held before a row was added once that row is removed. A running f64 can
//! not: its rounding depends on rows that may be gone. This sum is exact and
//! rounds to an f64 only when it is read.

use crate::{LimboError, Result, Value};
use num_bigint::{BigInt, Sign};
use num_traits::{ToPrimitive, Zero};

/// `mantissa * 2^exponent`, exact. Every finite f64 is such a number, so
/// sums of them are too. The mantissa is odd unless it is zero.
///
/// Stored as a BLOB: empty for zero, else the exponent as 4 little-endian
/// bytes followed by the mantissa as signed little-endian bytes.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DyadicSum {
    mantissa: BigInt,
    exponent: i32,
}

impl DyadicSum {
    pub(crate) fn from_blob(blob: &[u8]) -> Result<Self> {
        if blob.is_empty() {
            return Ok(Self::default());
        }
        let Some((exponent, mantissa)) = blob.split_first_chunk::<4>() else {
            return Err(LimboError::Corrupt(format!(
                "materialized view exact sum blob has {} bytes",
                blob.len()
            )));
        };
        Ok(Self {
            mantissa: BigInt::from_signed_bytes_le(mantissa),
            exponent: i32::from_le_bytes(*exponent),
        })
    }

    pub(crate) fn to_value(&self) -> Result<Value> {
        if self.mantissa.is_zero() {
            return Ok(Value::from_slice(&[])?);
        }
        let mut blob = self.exponent.to_le_bytes().to_vec();
        blob.extend(self.mantissa.to_signed_bytes_le());
        Ok(Value::from_slice(&blob)?)
    }

    /// Add `f` `weight` times; a negative weight retracts it.
    pub(crate) fn add(&mut self, f: f64, weight: i64) {
        assert!(f.is_finite(), "only finite values have an exact sum: {f}");
        let bits = f.to_bits();
        let biased_exponent = ((bits >> 52) & 0x7ff) as i32;
        let fraction = (bits & ((1 << 52) - 1)) as i64;
        let (mantissa, exponent) = if biased_exponent == 0 {
            (fraction, -1074)
        } else {
            (fraction | (1 << 52), biased_exponent - 1075)
        };
        let signed = if f.is_sign_negative() {
            -mantissa
        } else {
            mantissa
        };
        self.add_scaled(
            BigInt::from(i128::from(signed) * i128::from(weight)),
            exponent,
        );
    }

    pub(crate) fn add_integer(&mut self, i: BigInt) {
        self.add_scaled(i, 0);
    }

    fn add_scaled(&mut self, mantissa: BigInt, exponent: i32) {
        if self.mantissa.is_zero() {
            self.exponent = exponent;
        }
        if exponent < self.exponent {
            self.mantissa <<= (self.exponent - exponent) as u32;
            self.exponent = exponent;
        }
        self.mantissa += mantissa << ((exponent - self.exponent) as u32);
        match self.mantissa.trailing_zeros() {
            Some(zeros) => {
                self.mantissa >>= zeros;
                self.exponent += zeros as i32;
            }
            None => self.exponent = 0,
        }
    }

    /// Rounds to the nearest f64, ties to even, ±inf past the f64 range.
    pub(crate) fn to_f64(&self) -> f64 {
        let negative = self.mantissa.sign() == Sign::Minus;
        let magnitude = self.mantissa.magnitude();
        let bits = magnitude.bits();
        if bits == 0 {
            return 0.0;
        }
        let (mut significand, mut lowest_bit) = if bits <= 53 {
            (
                magnitude.to_u64().expect("53 bits fit a u64"),
                self.exponent,
            )
        } else {
            let shift = bits - 53;
            let kept = (magnitude >> shift).to_u64().expect("53 bits fit a u64");
            let round_bit = magnitude.bit(shift - 1);
            let below_round_bit = magnitude
                .trailing_zeros()
                .is_some_and(|zeros| zeros < shift - 1);
            let round_up = round_bit && (below_round_bit || kept & 1 == 1);
            (kept + u64::from(round_up), self.exponent + shift as i32)
        };
        if significand == 1 << 53 {
            significand >>= 1;
            lowest_bit += 1;
        }
        let highest_bit = lowest_bit + 63 - significand.leading_zeros() as i32;
        let magnitude = if highest_bit >= 1024 {
            f64::INFINITY
        } else {
            significand as f64 * power_of_two(lowest_bit)
        };
        if negative {
            -magnitude
        } else {
            magnitude
        }
    }
}

/// Valid for every exponent an f64 can represent exactly: -1074..=1023.
fn power_of_two(exponent: i32) -> f64 {
    assert!(
        (-1074..=1023).contains(&exponent),
        "2^{exponent} is not an f64"
    );
    if exponent >= -1022 {
        f64::from_bits(((exponent + 1023) as u64) << 52)
    } else {
        f64::from_bits(1 << (exponent + 1074))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum_of(values: &[f64]) -> f64 {
        let mut sum = DyadicSum::default();
        for v in values {
            sum.add(*v, 1);
        }
        sum.to_f64()
    }

    #[test]
    fn rounds_to_nearest_f64() {
        assert_eq!(sum_of(&[0.1, 0.2]), 0.30000000000000004);
        assert_eq!(sum_of(&[1e16, 1.0, 1.0]), 1e16 + 2.0);
        assert_eq!(sum_of(&[1e100, 1.0, -1e100]), 1.0);
        assert_eq!(sum_of(&[f64::MIN_POSITIVE / 4.0]), f64::MIN_POSITIVE / 4.0);
        assert_eq!(sum_of(&[5e-324, 5e-324]), 1e-323);
        assert_eq!(sum_of(&[-0.5, -0.25]), -0.75);
    }

    #[test]
    fn ties_round_to_even() {
        let two_53 = 9007199254740992.0;
        assert_eq!(sum_of(&[two_53, 1.0]), two_53);
        assert_eq!(sum_of(&[two_53, 3.0]), two_53 + 4.0);
        assert_eq!(sum_of(&[two_53, 1.0, 1e-300]), two_53 + 2.0);
    }

    #[test]
    fn overflows_to_infinity_past_the_f64_range() {
        assert_eq!(sum_of(&[f64::MAX, f64::MAX]), f64::INFINITY);
        assert_eq!(sum_of(&[-f64::MAX, -f64::MAX]), f64::NEG_INFINITY);
        assert_eq!(sum_of(&[f64::MAX, f64::MAX, -f64::MAX]), f64::MAX);
        let half_ulp_of_max = power_of_two(970);
        assert_eq!(sum_of(&[f64::MAX, half_ulp_of_max]), f64::INFINITY);
        assert_eq!(sum_of(&[f64::MAX, half_ulp_of_max / 2.0]), f64::MAX);
    }

    #[test]
    fn retraction_restores_the_previous_sum() {
        let mut sum = DyadicSum::default();
        sum.add(0.001, 1);
        let before = sum.clone();
        sum.add(1e308, 2);
        sum.add(0.3, 3);
        sum.add(0.3, -3);
        sum.add(1e308, -2);
        assert_eq!(sum, before);
        assert_eq!(sum.to_f64(), 0.001);
    }

    #[test]
    fn round_trips_through_a_blob() {
        for values in [&[][..], &[-0.375, 1e-300][..], &[1e308, 1e308, 5e-324][..]] {
            let mut sum = DyadicSum::default();
            for v in values {
                sum.add(*v, 3);
            }
            let Value::Blob(blob) = sum.to_value().unwrap() else {
                panic!("an exact sum is stored as a blob");
            };
            assert_eq!(DyadicSum::from_blob(&blob).unwrap(), sum);
        }
    }
}
