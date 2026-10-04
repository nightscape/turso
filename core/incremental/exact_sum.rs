//! Exact running state for sum() and avg() in materialized views.
//!
//! Rows are both added and retracted, so the state must give back exactly what
//! it held before a row was added once that row is removed. A running f64 can
//! not: rounding, an overflow to infinity and the INTEGER/REAL result type all
//! depend on rows that may be gone. This state keeps every part exactly and
//! decides the result only when it is read.

use crate::numeric::Numeric;
use crate::vdbe::execute::{classify_numeric_arg, NumericArg};
use crate::{LimboError, Result, Value};
use num_bigint::{BigInt, Sign};
use num_traits::{ToPrimitive, Zero};

const VALUES_PER_STATE: usize = 8;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SumState {
    non_null_inputs: i64,
    real_inputs: i64,
    integer_sum: i128,
    real_sum: DyadicSum,
    positive_infinities: i64,
    negative_infinities: i64,
}

impl SumState {
    pub fn apply(&mut self, value: &Value, weight: i64) {
        match classify_numeric_arg(value) {
            NumericArg::Null => return,
            NumericArg::Integer(i) => {
                self.integer_sum = self
                    .integer_sum
                    .checked_add(i128::from(i) * i128::from(weight))
                    .expect("an i128 sum of i64 values times row weights cannot overflow");
            }
            NumericArg::Float(f) => {
                self.real_inputs += weight;
                if f == f64::INFINITY {
                    self.positive_infinities += weight;
                } else if f == f64::NEG_INFINITY {
                    self.negative_infinities += weight;
                } else {
                    self.real_sum.add(f, weight);
                }
            }
        }
        self.non_null_inputs += weight;
        assert!(
            self.non_null_inputs >= 0
                && self.real_inputs >= 0
                && self.positive_infinities >= 0
                && self.negative_infinities >= 0,
            "sum state retracted a value it never held: {self:?}"
        );
        if self.real_inputs == 0 {
            assert!(
                self.real_sum.is_zero(),
                "sum state has no real inputs left but a non-zero real sum: {self:?}"
            );
        }
    }

    /// The value of sum(): INTEGER while every input is an integer, REAL as
    /// soon as one is not, NULL without non-NULL inputs.
    pub fn sum(&self) -> Result<Value> {
        if self.non_null_inputs == 0 {
            return Ok(Value::Null);
        }
        if self.real_inputs == 0 {
            return i64::try_from(self.integer_sum)
                .map(Value::from_i64)
                .map_err(|_| LimboError::IntegerOverflow);
        }
        Ok(Value::from_f64(self.real_total()))
    }

    pub fn avg(&self) -> Value {
        if self.non_null_inputs == 0 {
            return Value::Null;
        }
        let total = if self.real_inputs == 0 {
            self.integer_sum as f64
        } else {
            self.real_total()
        };
        Value::from_f64(total / self.non_null_inputs as f64)
    }

    /// NaN when both infinities are present; `Value::from_f64` turns it into
    /// NULL, as SQLite does.
    fn real_total(&self) -> f64 {
        match (self.positive_infinities > 0, self.negative_infinities > 0) {
            (true, true) => f64::NAN,
            (true, false) => f64::INFINITY,
            (false, true) => f64::NEG_INFINITY,
            (false, false) => {
                let mut total = self.real_sum.clone();
                total.add_integer(self.integer_sum);
                total.to_f64()
            }
        }
    }

    pub fn to_values(&self, out: &mut Vec<Value>) -> Result<()> {
        out.push(Value::from_i64(self.non_null_inputs));
        out.push(Value::from_i64(self.real_inputs));
        out.push(Value::from_i64((self.integer_sum >> 64) as i64));
        out.push(Value::from_i64(self.integer_sum as i64));
        out.push(Value::from_i64(self.positive_infinities));
        out.push(Value::from_i64(self.negative_infinities));
        out.push(Value::from_i64(i64::from(self.real_sum.exponent)));
        out.push(Value::from_slice(
            &self.real_sum.mantissa.to_signed_bytes_le(),
        )?);
        Ok(())
    }

    pub fn from_values(values: &[Value], cursor: &mut usize) -> Result<Self> {
        let Some(slice) = values.get(*cursor..*cursor + VALUES_PER_STATE) else {
            return Err(LimboError::InternalError(format!(
                "sum state needs {VALUES_PER_STATE} values at position {cursor}, the record has {}",
                values.len()
            )));
        };
        let integer = |i: usize| match &slice[i] {
            Value::Numeric(Numeric::Integer(v)) => Ok(*v),
            other => Err(LimboError::InternalError(format!(
                "sum state value {i} must be an integer, got {other:?}"
            ))),
        };
        let Value::Blob(mantissa) = &slice[7] else {
            return Err(LimboError::InternalError(format!(
                "sum state mantissa must be a blob, got {:?}",
                slice[7]
            )));
        };
        let exponent = i32::try_from(integer(6)?).map_err(|_| {
            LimboError::InternalError("sum state exponent out of range".to_string())
        })?;
        let state = Self {
            non_null_inputs: integer(0)?,
            real_inputs: integer(1)?,
            integer_sum: (i128::from(integer(2)?) << 64) | i128::from(integer(3)? as u64),
            positive_infinities: integer(4)?,
            negative_infinities: integer(5)?,
            real_sum: DyadicSum {
                mantissa: BigInt::from_signed_bytes_le(mantissa),
                exponent,
            },
        };
        *cursor += VALUES_PER_STATE;
        Ok(state)
    }
}

/// `mantissa * 2^exponent`, exact. Every finite f64 is such a number, so
/// sums of them are too. The mantissa is odd unless it is zero.
#[derive(Debug, Clone, Default, PartialEq)]
struct DyadicSum {
    mantissa: BigInt,
    exponent: i32,
}

impl DyadicSum {
    fn is_zero(&self) -> bool {
        self.mantissa.is_zero()
    }

    fn add(&mut self, f: f64, weight: i64) {
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

    fn add_integer(&mut self, i: i128) {
        self.add_scaled(BigInt::from(i), 0);
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
    fn to_f64(&self) -> f64 {
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
    fn retraction_restores_the_previous_state() {
        let mut state = SumState::default();
        state.apply(&Value::from_i64(5), 1);
        let before = state.clone();
        state.apply(&Value::from_f64(1e308), 2);
        state.apply(&Value::from_f64(f64::INFINITY), 1);
        state.apply(&Value::from_f64(0.1), 1);
        state.apply(&Value::from_f64(0.1), -1);
        state.apply(&Value::from_f64(f64::INFINITY), -1);
        state.apply(&Value::from_f64(1e308), -2);
        assert_eq!(state, before);
        assert_eq!(state.sum().unwrap(), Value::from_i64(5));
    }

    #[test]
    fn state_round_trips_through_values() {
        let mut state = SumState::default();
        state.apply(&Value::from_i64(i64::MIN), 3);
        state.apply(&Value::from_f64(-0.375), 1);
        state.apply(&Value::from_f64(f64::NEG_INFINITY), 1);
        let mut values = vec![Value::Null];
        state.to_values(&mut values).unwrap();
        let mut cursor = 1;
        assert_eq!(SumState::from_values(&values, &mut cursor).unwrap(), state);
        assert_eq!(cursor, values.len());
    }
}
