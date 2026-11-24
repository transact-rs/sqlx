use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::types::jiff::{to_micros, PG_EPOCH_TIMESTAMP};
use crate::types::Type;
use crate::{PgArgumentBuffer, PgHasArrayType, PgTypeInfo, PgValueFormat, PgValueRef, Postgres};
use jiff::{SignedDuration, Timestamp};
use std::mem;

impl Type<Postgres> for Timestamp {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo::TIMESTAMPTZ
    }
}

impl PgHasArrayType for Timestamp {
    fn array_type_info() -> PgTypeInfo {
        PgTypeInfo::TIMESTAMPTZ_ARRAY
    }
}

impl Encode<'_, Postgres> for Timestamp {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        // TIMESTAMPTZ is encoded as the microseconds since the epoch (in UTC)
        let micros = to_micros(self.duration_since(PG_EPOCH_TIMESTAMP)).ok_or_else(|| {
            format!("value {self:?} would overflow binary encoding for Postgres TIMESTAMPTZ")
        })?;
        Encode::<Postgres>::encode(micros, buf)
    }

    fn size_hint(&self) -> usize {
        mem::size_of::<i64>()
    }
}

impl<'r> Decode<'r, Postgres> for Timestamp {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(match value.format() {
            PgValueFormat::Binary => {
                // TIMESTAMPTZ is encoded as the microseconds since the epoch (in UTC)
                let us: i64 = Decode::<Postgres>::decode(value)?;

                // `infinity`, `-infinity` and years beyond 9999 don't fit in `Timestamp`
                PG_EPOCH_TIMESTAMP
                    .checked_add(SignedDuration::from_micros(us))
                    .map_err(|_| {
                        format!("Postgres TIMESTAMPTZ out of range for jiff::Timestamp: {us} microseconds from 2000-01-01")
                    })?
            }

            // e.g. `2019-01-02 05:10:20.1151+00`, in the session's `TimeZone`
            PgValueFormat::Text => value.as_str()?.parse()?,
        })
    }
}
