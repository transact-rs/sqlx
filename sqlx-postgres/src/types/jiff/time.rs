use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::types::jiff::to_micros;
use crate::types::Type;
use crate::{PgArgumentBuffer, PgHasArrayType, PgTypeInfo, PgValueFormat, PgValueRef, Postgres};
use jiff::civil::Time;
use jiff::SignedDuration;
use std::mem;

impl Type<Postgres> for Time {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo::TIME
    }
}

impl PgHasArrayType for Time {
    fn array_type_info() -> PgTypeInfo {
        PgTypeInfo::TIME_ARRAY
    }
}

impl Encode<'_, Postgres> for Time {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        // TIME is encoded as the microseconds since midnight.
        let micros = time_to_micros(*self);
        Encode::<Postgres>::encode(micros, buf)
    }

    fn size_hint(&self) -> usize {
        mem::size_of::<u64>()
    }
}

impl<'r> Decode<'r, Postgres> for Time {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(match value.format() {
            PgValueFormat::Binary => {
                // TIME is encoded as the microseconds since midnight
                let us: i64 = Decode::<Postgres>::decode(value)?;
                time_from_micros(us).ok_or_else(|| {
                    format!("Postgres TIME out of range for jiff::civil::Time: {us} microseconds from midnight")
                })?
            }

            // Postgres will not include the subsecond part if it's zero.
            // `24:00:00` is rejected by the parser.
            PgValueFormat::Text => value.as_str()?.parse()?,
        })
    }
}

/// Microseconds since midnight; shared with the `TIMETZ` implementation.
pub(crate) fn time_to_micros(time: Time) -> i64 {
    // A span of less than 24 hours always fits.
    to_micros(time.duration_since(Time::midnight()))
        .expect("BUG: time of day should always fit in i64 microseconds")
}

/// Inverse of [`time_to_micros`]; shared with the `TIMETZ` implementation.
///
/// Returns `None` if the value is out of range.
pub(crate) fn time_from_micros(us: i64) -> Option<Time> {
    // Postgres allows `24:00:00`, which would otherwise wrap around to midnight.
    // `checked_add` returns an error instead of wrapping.
    Time::midnight()
        .checked_add(SignedDuration::from_micros(us))
        .ok()
}
