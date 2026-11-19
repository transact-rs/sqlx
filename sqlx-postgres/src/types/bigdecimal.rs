use bigdecimal::{BigDecimal, RoundingMode};
use num_bigint::{BigInt, Sign};
use std::cmp;

use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::types::numeric::{PgNumeric, PgNumericSign};
use crate::types::Type;
use crate::{PgArgumentBuffer, PgHasArrayType, PgTypeInfo, PgValueFormat, PgValueRef, Postgres};

impl Type<Postgres> for BigDecimal {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo::NUMERIC
    }
}

impl PgHasArrayType for BigDecimal {
    fn array_type_info() -> PgTypeInfo {
        PgTypeInfo::NUMERIC_ARRAY
    }
}

impl TryFrom<PgNumeric> for BigDecimal {
    type Error = BoxDynError;

    fn try_from(numeric: PgNumeric) -> Result<Self, BoxDynError> {
        Self::try_from(&numeric)
    }
}

impl TryFrom<&'_ PgNumeric> for BigDecimal {
    type Error = BoxDynError;

    fn try_from(numeric: &'_ PgNumeric) -> Result<Self, Self::Error> {
        let (digits, sign, weight, scale) = match *numeric {
            PgNumeric::Number {
                ref digits,
                sign,
                weight,
                scale,
            } => (digits, sign, weight, scale),

            PgNumeric::NotANumber => {
                return Err("BigDecimal does not support NaN values".into());
            }
        };

        if scale < 0 {
            return Err(format!("invalid scale value for Pg NUMERIC: {scale}").into());
        }

        let display_scale = i64::from(scale);

        if digits.is_empty() {
            // Postgres returns an empty digit array for 0 but BigInt expects at least one zero
            return Ok(BigDecimal::from(0u64).with_scale(display_scale));
        }

        let sign = match sign {
            PgNumericSign::Positive => Sign::Plus,
            PgNumericSign::Negative => Sign::Minus,
        };

        // weight is 0 if the decimal point falls after the first base-10000 digit
        //
        // `Vec` capacity cannot exceed `isize::MAX` bytes, so this cast can't wrap in practice.
        #[allow(clippy::cast_possible_wrap)]
        let digits_scale = (digits.len() as i64 - weight as i64 - 1) * 4;

        // no optimized algorithm for base-10 so use base-100 for faster processing
        let mut cents = Vec::with_capacity(digits.len() * 2);

        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_possible_wrap,
            clippy::cast_sign_loss
        )]
        for (i, &digit) in digits.iter().enumerate() {
            if !PgNumeric::is_valid_digit(digit) {
                return Err(format!(
                    "PgNumeric to BigDecimal: {i}th digit is out of range {digit}"
                )
                .into());
            }

            cents.push((digit / 100) as u8);
            cents.push((digit % 100) as u8);
        }

        let bigint = BigInt::from_radix_be(sign, &cents, 100)
            .ok_or("PgNumeric contained an out-of-range digit")?;

        // The base-10000 digits only give a scale that is a multiple of 4, so apply the `scale`
        // Postgres sent, to keep trailing zeros (`1.50`) and the scale of a whole number (`4.00`).
        //
        // Postgres rounds to `scale` before it sends a value, so there are no nonzero digits past
        // it. If a hand-built `PgNumeric` has some, round half away from zero like Postgres does.
        Ok(BigDecimal::new(bigint, digits_scale)
            .with_scale_round(display_scale, RoundingMode::HalfUp))
    }
}

impl TryFrom<&'_ BigDecimal> for PgNumeric {
    type Error = BoxDynError;

    fn try_from(decimal: &BigDecimal) -> Result<Self, BoxDynError> {
        let base_10_to_10000 = |chunk: &[u8]| chunk.iter().fold(0i16, |a, &d| a * 10 + d as i16);

        // NOTE: this unfortunately copies the BigInt internally
        let (integer, exp) = decimal.as_bigint_and_exponent();

        // this routine is specifically optimized for base-10
        // FIXME: is there a way to iterate over the digits to avoid the Vec allocation
        let (sign, base_10) = integer.to_radix_be(10);

        let base_10_len = i64::try_from(base_10.len()).map_err(|_| {
            format!(
                "BigDecimal base-10 length out of range for PgNumeric: {}",
                base_10.len()
            )
        })?;

        // weight is positive power of 10000
        // exp is the negative power of 10
        let weight_10 = base_10_len - exp;

        // scale is only nonzero when we have fractional digits
        // since `exp` is the _negative_ decimal exponent, it tells us
        // exactly what our scale should be
        let scale: i16 = cmp::max(0, exp).try_into()?;

        // there's an implicit +1 offset in the interpretation
        let weight: i16 = if weight_10 <= 0 {
            weight_10 / 4 - 1
        } else {
            // the `-1` is a fix for an off by 1 error (4 digits should still be 0 weight)
            (weight_10 - 1) / 4
        }
        .try_into()?;

        let digits_len = if base_10.len() % 4 != 0 {
            base_10.len() / 4 + 1
        } else {
            base_10.len() / 4
        };

        // For efficiency, we want to process the base-10 digits in chunks of 4,
        // but that means we need to deal with the non-divisible remainder first.
        let offset = weight_10.rem_euclid(4);

        // Do a checked conversion to the smallest integer,
        // so we can widen arbitrarily without triggering lints.
        let offset = u8::try_from(offset).unwrap_or_else(|_| {
            panic!("BUG: `offset` should be in the range [0, 4) but is {offset}")
        });

        let mut digits = Vec::with_capacity(digits_len);

        if let Some(first) = base_10.get(..offset as usize) {
            if !first.is_empty() {
                digits.push(base_10_to_10000(first));
            }
        } else if offset != 0 {
            // If we didn't hit the `if let Some` branch,
            // then `base_10.len()` must strictly be smaller
            #[allow(clippy::cast_possible_truncation)]
            let power = (offset as usize - base_10.len()) as u32;

            digits.push(base_10_to_10000(&base_10) * 10i16.pow(power));
        }

        if let Some(rest) = base_10.get(offset as usize..) {
            // `chunk.len()` is always between 1 and 4
            #[allow(clippy::cast_possible_truncation)]
            digits.extend(
                rest.chunks(4)
                    .map(|chunk| base_10_to_10000(chunk) * 10i16.pow(4 - chunk.len() as u32)),
            );
        }

        while let Some(&0) = digits.last() {
            digits.pop();
        }

        Ok(PgNumeric::Number {
            sign: sign_to_pg(sign),
            scale,
            weight,
            digits,
        })
    }
}

#[doc=include_str!("bigdecimal-range.md")]
impl Encode<'_, Postgres> for BigDecimal {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        PgNumeric::try_from(self)?.encode(buf)?;

        Ok(IsNull::No)
    }

    fn size_hint(&self) -> usize {
        PgNumeric::size_hint(self.digits())
    }
}

/// ### Note: `NaN`
/// `BigDecimal` has a greater range than `NUMERIC` (see the corresponding `Encode` impl for details)
/// but cannot represent `NaN`, so decoding may return an error.
impl Decode<'_, Postgres> for BigDecimal {
    fn decode(value: PgValueRef<'_>) -> Result<Self, BoxDynError> {
        match value.format() {
            PgValueFormat::Binary => PgNumeric::decode(value.as_bytes()?)?.try_into(),
            PgValueFormat::Text => Ok(value.as_str()?.parse::<BigDecimal>()?),
        }
    }
}

fn sign_to_pg(sign: Sign) -> PgNumericSign {
    match sign {
        Sign::Plus | Sign::NoSign => PgNumericSign::Positive,
        Sign::Minus => PgNumericSign::Negative,
    }
}

#[cfg(test)]
#[allow(clippy::zero_prefixed_literal)] // Used for clarity
mod tests {
    use super::{BigDecimal, PgNumeric, PgNumericSign};
    use std::convert::TryFrom;

    /// Decode a hand-built `PgNumeric::Number` and check how the `BigDecimal` is rendered.
    ///
    /// `BigDecimal` equality ignores the scale, so compare the string, and check the scale itself
    /// because `to_string()` never shows the scale of zero.
    #[track_caller]
    fn assert_decodes_to(
        sign: PgNumericSign,
        weight: i16,
        scale: i16,
        digits: Vec<i16>,
        expected: &str,
    ) {
        let decoded = BigDecimal::try_from(PgNumeric::Number {
            sign,
            scale,
            weight,
            digits,
        })
        .unwrap();

        assert_eq!(decoded.to_string(), expected);
        assert_eq!(decoded.as_bigint_and_exponent().1, i64::from(scale));
    }

    #[test]
    fn decode_keeps_trailing_zeros_of_fraction() {
        // `1.50::numeric(10, 2)`
        assert_decodes_to(PgNumericSign::Positive, 0, 2, vec![1, 5000], "1.50");
    }

    #[test]
    fn decode_keeps_scale_of_integer_value() {
        // `4.00::numeric(10, 2)`
        assert_decodes_to(PgNumericSign::Positive, 0, 2, vec![4], "4.00");
        // `100.00::numeric(10, 2)`
        assert_decodes_to(PgNumericSign::Positive, 0, 2, vec![100], "100.00");
    }

    #[test]
    fn decode_keeps_scale_of_zero() {
        // Postgres sends no digits at all for `0.00::numeric(10, 2)`;
        // `BigDecimal` displays zero as "0" whatever its scale, `assert_decodes_to` checks the scale.
        assert_decodes_to(PgNumericSign::Positive, 0, 2, vec![], "0");
        assert_decodes_to(PgNumericSign::Positive, 0, 0, vec![], "0");
    }

    #[test]
    fn decode_keeps_scale_with_positive_weight() {
        // `12345.60::numeric(10, 2)`
        assert_decodes_to(
            PgNumericSign::Positive,
            1,
            2,
            vec![1, 2345, 6000],
            "12345.60",
        );
        // `10000.00::numeric(10, 2)`: the digits end before the decimal point
        assert_decodes_to(PgNumericSign::Positive, 1, 2, vec![1], "10000.00");
        // `10000::numeric`
        assert_decodes_to(PgNumericSign::Positive, 1, 0, vec![1], "10000");
    }

    #[test]
    fn decode_keeps_scale_of_negative() {
        // `-1.50::numeric(10, 2)`
        assert_decodes_to(PgNumericSign::Negative, 0, 2, vec![1, 5000], "-1.50");
    }

    #[test]
    fn decode_keeps_scale_of_small_fraction() {
        // `0.0001::numeric(10, 6)`
        assert_decodes_to(PgNumericSign::Positive, -1, 6, vec![1], "0.000100");
        // `0.00001234::numeric`
        assert_decodes_to(PgNumericSign::Positive, -2, 8, vec![1234], "0.00001234");
    }

    #[test]
    fn decode_rounds_digits_beyond_scale_half_away_from_zero() {
        // Postgres never sends nonzero digits past `scale` (it rounds to `scale` before it sends),
        // but a hand-built value can have them. Round like Postgres and like the `Decimal` impl.
        assert_decodes_to(PgNumericSign::Positive, 0, 2, vec![1, 5678], "1.57");
        assert_decodes_to(PgNumericSign::Positive, 0, 2, vec![1, 5650], "1.57");
        assert_decodes_to(PgNumericSign::Positive, 0, 2, vec![1, 5649], "1.56");
        assert_decodes_to(PgNumericSign::Negative, 0, 2, vec![1, 5650], "-1.57");
        assert_decodes_to(PgNumericSign::Positive, 0, 0, vec![1, 5000], "2");
        assert_decodes_to(PgNumericSign::Positive, 0, 0, vec![1, 4999], "1");
        // carries into the integer part
        assert_decodes_to(PgNumericSign::Positive, 0, 2, vec![1, 9999], "2.00");
    }

    #[test]
    fn decode_rejects_negative_scale() {
        assert!(BigDecimal::try_from(PgNumeric::Number {
            sign: PgNumericSign::Positive,
            scale: -1,
            weight: 0,
            digits: vec![1],
        })
        .is_err());
    }

    #[test]
    fn zero() {
        let zero: BigDecimal = "0".parse().unwrap();

        assert_eq!(
            PgNumeric::try_from(&zero).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 0,
                weight: 0,
                digits: vec![]
            }
        );
    }

    #[test]
    fn one() {
        let one: BigDecimal = "1".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&one).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 0,
                weight: 0,
                digits: vec![1]
            }
        );
    }

    #[test]
    fn ten() {
        let ten: BigDecimal = "10".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&ten).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 0,
                weight: 0,
                digits: vec![10]
            }
        );
    }

    #[test]
    fn one_hundred() {
        let one_hundred: BigDecimal = "100".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&one_hundred).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 0,
                weight: 0,
                digits: vec![100]
            }
        );
    }

    #[test]
    fn ten_thousand() {
        // BigDecimal doesn't normalize here
        let ten_thousand: BigDecimal = "10000".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&ten_thousand).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 0,
                weight: 1,
                digits: vec![1]
            }
        );
    }

    #[test]
    fn two_digits() {
        let two_digits: BigDecimal = "12345".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&two_digits).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 0,
                weight: 1,
                digits: vec![1, 2345]
            }
        );
    }

    #[test]
    fn one_tenth() {
        let one_tenth: BigDecimal = "0.1".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&one_tenth).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 1,
                weight: -1,
                digits: vec![1000]
            }
        );
    }

    #[test]
    fn one_hundredth() {
        let one_hundredth: BigDecimal = "0.01".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&one_hundredth).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 2,
                weight: -1,
                digits: vec![100]
            }
        );
    }

    #[test]
    fn twelve_thousandths() {
        let twelve_thousandths: BigDecimal = "0.012".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&twelve_thousandths).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 3,
                weight: -1,
                digits: vec![120]
            }
        );
    }

    #[test]
    fn decimal_1() {
        let decimal: BigDecimal = "1.2345".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&decimal).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 4,
                weight: 0,
                digits: vec![1, 2345]
            }
        );
    }

    #[test]
    fn decimal_2() {
        let decimal: BigDecimal = "0.12345".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&decimal).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 5,
                weight: -1,
                digits: vec![1234, 5000]
            }
        );
    }

    #[test]
    fn decimal_3() {
        let decimal: BigDecimal = "0.01234".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&decimal).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 5,
                weight: -1,
                digits: vec![0123, 4000]
            }
        );
    }

    #[test]
    fn decimal_4() {
        let decimal: BigDecimal = "12345.67890".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&decimal).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 5,
                weight: 1,
                digits: vec![1, 2345, 6789]
            }
        );
    }

    #[test]
    fn one_digit_decimal() {
        let one_digit_decimal: BigDecimal = "0.00001234".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&one_digit_decimal).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 8,
                weight: -2,
                digits: vec![1234]
            }
        );
    }

    #[test]
    fn issue_423_four_digit() {
        // This is a regression test for https://github.com/launchbadge/sqlx/issues/423
        let four_digit: BigDecimal = "1234".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&four_digit).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 0,
                weight: 0,
                digits: vec![1234]
            }
        );
    }

    #[test]
    fn issue_423_negative_four_digit() {
        // This is a regression test for https://github.com/launchbadge/sqlx/issues/423
        let negative_four_digit: BigDecimal = "-1234".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&negative_four_digit).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Negative,
                scale: 0,
                weight: 0,
                digits: vec![1234]
            }
        );
    }

    #[test]
    fn issue_423_eight_digit() {
        // This is a regression test for https://github.com/launchbadge/sqlx/issues/423
        let eight_digit: BigDecimal = "12345678".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&eight_digit).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Positive,
                scale: 0,
                weight: 1,
                digits: vec![1234, 5678]
            }
        );
    }

    #[test]
    fn issue_423_negative_eight_digit() {
        // This is a regression test for https://github.com/launchbadge/sqlx/issues/423
        let negative_eight_digit: BigDecimal = "-12345678".parse().unwrap();
        assert_eq!(
            PgNumeric::try_from(&negative_eight_digit).unwrap(),
            PgNumeric::Number {
                sign: PgNumericSign::Negative,
                scale: 0,
                weight: 1,
                digits: vec![1234, 5678]
            }
        );
    }
}
