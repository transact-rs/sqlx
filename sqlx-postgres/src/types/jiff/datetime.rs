use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::types::jiff::{to_micros, PG_EPOCH_DATETIME};
use crate::types::Type;
use crate::{PgArgumentBuffer, PgHasArrayType, PgTypeInfo, PgValueFormat, PgValueRef, Postgres};
use jiff::civil::DateTime;
use jiff::SignedDuration;
use std::mem;

impl Type<Postgres> for DateTime {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo::TIMESTAMP
    }
}

impl PgHasArrayType for DateTime {
    fn array_type_info() -> PgTypeInfo {
        PgTypeInfo::TIMESTAMP_ARRAY
    }
}

impl Encode<'_, Postgres> for DateTime {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        // TIMESTAMP is encoded as the microseconds since the epoch
        let micros = to_micros(self.duration_since(PG_EPOCH_DATETIME)).ok_or_else(|| {
            format!("value {self:?} would overflow binary encoding for Postgres TIMESTAMP")
        })?;
        Encode::<Postgres>::encode(micros, buf)
    }

    fn size_hint(&self) -> usize {
        mem::size_of::<i64>()
    }
}

impl<'r> Decode<'r, Postgres> for DateTime {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(match value.format() {
            PgValueFormat::Binary => {
                // TIMESTAMP is encoded as the microseconds since the epoch
                let us: i64 = Decode::<Postgres>::decode(value)?;

                // `infinity`, `-infinity` and years beyond 9999 don't fit in `DateTime`
                PG_EPOCH_DATETIME
                    .checked_add(SignedDuration::from_micros(us))
                    .map_err(|_| {
                        format!("Postgres TIMESTAMP out of range for jiff::civil::DateTime: {us} microseconds from 2000-01-01")
                    })?
            }

            // e.g. `2019-01-02 05:10:20.1151`
            //
            // If the value is actually a `TIMESTAMPTZ` (e.g. `2019-01-02 05:10:20+00`),
            // the offset is ignored. Postgres already guarantees this to always be UTC.
            PgValueFormat::Text => value.as_str()?.parse()?,
        })
    }
}
