use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::types::jiff::PG_EPOCH;
use crate::types::Type;
use crate::{PgArgumentBuffer, PgHasArrayType, PgTypeInfo, PgValueFormat, PgValueRef, Postgres};
use jiff::civil::Date;
use jiff::SignedDuration;
use std::mem;

impl Type<Postgres> for Date {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo::DATE
    }
}

impl PgHasArrayType for Date {
    fn array_type_info() -> PgTypeInfo {
        PgTypeInfo::DATE_ARRAY
    }
}

impl Encode<'_, Postgres> for Date {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        // DATE is encoded as number of days since epoch (2000-01-01).
        //
        // The range of `jiff::civil::Date` (years -9999 to 9999) always fits in an `i32`
        // number of days, and civil days are always exactly 24 hours.
        #[allow(clippy::cast_possible_truncation)]
        let days = (self.duration_since(PG_EPOCH).as_hours() / 24) as i32;
        Encode::<Postgres>::encode(days, buf)
    }

    fn size_hint(&self) -> usize {
        mem::size_of::<i32>()
    }
}

impl<'r> Decode<'r, Postgres> for Date {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(match value.format() {
            PgValueFormat::Binary => {
                // DATE is encoded as the days since epoch
                let days: i32 = Decode::<Postgres>::decode(value)?;

                // `infinity`, `-infinity` and years beyond 9999 don't fit in `Date`
                PG_EPOCH
                    .checked_add(SignedDuration::from_hours(i64::from(days) * 24))
                    .map_err(|_| {
                        format!("Postgres DATE out of range for jiff::civil::Date: {days} days from 2000-01-01")
                    })?
            }

            PgValueFormat::Text => value.as_str()?.parse()?,
        })
    }
}
