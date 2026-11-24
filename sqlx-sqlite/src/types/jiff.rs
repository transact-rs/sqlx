use crate::value::ValueRef;
use crate::{
    decode::Decode,
    encode::{Encode, IsNull},
    error::BoxDynError,
    type_info::DataType,
    types::Type,
    Sqlite, SqliteArgumentsBuffer, SqliteTypeInfo, SqliteValueRef,
};
use jiff::civil::{Date, DateTime, Time};
use jiff::fmt::temporal::DateTimePrinter;
use jiff::tz::Offset;
use jiff::{SignedDuration, Timestamp};

impl Type<Sqlite> for Timestamp {
    fn type_info() -> SqliteTypeInfo {
        SqliteTypeInfo(DataType::Datetime)
    }

    fn compatible(ty: &SqliteTypeInfo) -> bool {
        <DateTime as Type<Sqlite>>::compatible(ty)
    }
}

impl Type<Sqlite> for DateTime {
    fn type_info() -> SqliteTypeInfo {
        SqliteTypeInfo(DataType::Datetime)
    }

    fn compatible(ty: &SqliteTypeInfo) -> bool {
        matches!(
            ty.0,
            DataType::Datetime
                | DataType::Text
                | DataType::Integer
                | DataType::Int4
                | DataType::Float
        )
    }
}

impl Type<Sqlite> for Date {
    fn type_info() -> SqliteTypeInfo {
        SqliteTypeInfo(DataType::Date)
    }

    fn compatible(ty: &SqliteTypeInfo) -> bool {
        matches!(ty.0, DataType::Date | DataType::Text)
    }
}

impl Type<Sqlite> for Time {
    fn type_info() -> SqliteTypeInfo {
        SqliteTypeInfo(DataType::Time)
    }

    fn compatible(ty: &SqliteTypeInfo) -> bool {
        matches!(ty.0, DataType::Time | DataType::Text)
    }
}

impl Encode<'_, Sqlite> for Timestamp {
    fn encode_by_ref(&self, buf: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        // RFC 3339, e.g. `2019-01-02T05:10:20.1151Z`
        Encode::<Sqlite>::encode(self.to_string(), buf)
    }
}

impl Encode<'_, Sqlite> for DateTime {
    fn encode_by_ref(&self, buf: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        // SQLite's own format, e.g. `2019-01-02 05:10:20.1151`
        const PRINTER: DateTimePrinter = DateTimePrinter::new().separator(b' ');
        Encode::<Sqlite>::encode(PRINTER.datetime_to_string(self), buf)
    }
}

impl Encode<'_, Sqlite> for Date {
    fn encode_by_ref(&self, buf: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        // e.g. `2019-01-02`
        Encode::<Sqlite>::encode(self.to_string(), buf)
    }
}

impl Encode<'_, Sqlite> for Time {
    fn encode_by_ref(&self, buf: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        // e.g. `05:10:20.1151`
        Encode::<Sqlite>::encode(self.to_string(), buf)
    }
}

impl<'r> Decode<'r, Sqlite> for Timestamp {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        let ts = match value.type_info().0 {
            DataType::Text => decode_timestamp_from_text(value.text_borrowed()?),
            DataType::Int4 | DataType::Integer => Timestamp::from_second(value.int64()?).ok(),
            DataType::Float => decode_timestamp_from_float(value.double()?),

            _ => None,
        };

        if let Some(ts) = ts {
            Ok(ts)
        } else {
            Err(format!("invalid timestamp: {}", value.text_borrowed()?).into())
        }
    }
}

impl<'r> Decode<'r, Sqlite> for DateTime {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        let dt = match value.type_info().0 {
            DataType::Text => decode_datetime_from_text(value.text_borrowed()?),
            DataType::Int4 | DataType::Integer => Timestamp::from_second(value.int64()?)
                .ok()
                .map(|ts| Offset::UTC.to_datetime(ts)),
            DataType::Float => {
                decode_timestamp_from_float(value.double()?).map(|ts| Offset::UTC.to_datetime(ts))
            }

            _ => None,
        };

        if let Some(dt) = dt {
            Ok(dt)
        } else {
            Err(format!("invalid datetime: {}", value.text_borrowed()?).into())
        }
    }
}

impl<'r> Decode<'r, Sqlite> for Date {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(value.text_borrowed()?.parse()?)
    }
}

impl<'r> Decode<'r, Sqlite> for Time {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        let value = value.text_borrowed()?;

        // Handles e.g. `05:10`, `05:10:20`, `05:10:20.1151` and `05:10:20+02:00`
        // (in which case the offset is ignored).
        if let Ok(time) = value.parse() {
            return Ok(time);
        }

        // Jiff refuses to parse a civil time with a Zulu offset, e.g. `05:10:20Z`,
        // but SQLite allows it.
        if let Some(time) = value.strip_suffix(['Z', 'z']).and_then(|v| v.parse().ok()) {
            return Ok(time);
        }

        Err(format!("invalid time: {value}").into())
    }
}

fn decode_timestamp_from_text(value: &str) -> Option<Timestamp> {
    // With an offset, e.g. RFC 3339 (`2019-01-02T05:10:20Z`) or `2019-01-02 05:10:20+02:00`
    if let Ok(ts) = value.parse::<Timestamp>() {
        return Some(ts);
    }

    // Without an offset (e.g. SQLite's own format `2019-01-02 05:10:20`), assume UTC.
    value
        .parse::<DateTime>()
        .ok()
        .and_then(|dt| Offset::UTC.to_timestamp(dt).ok())
}

fn decode_datetime_from_text(value: &str) -> Option<DateTime> {
    // e.g. SQLite's own format, `2019-01-02 05:10:20.1151`.
    //
    // If the value has a numeric offset (e.g. `2019-01-02 05:10:20+02:00`),
    // it is ignored and the local date and time is returned.
    if let Ok(dt) = value.parse::<DateTime>() {
        return Some(dt);
    }

    // Jiff refuses to parse a civil datetime with a Zulu offset, e.g. `2019-01-02T05:10:20Z`.
    value
        .parse::<Timestamp>()
        .ok()
        .map(|ts| Offset::UTC.to_datetime(ts))
}

/// Decode a (possibly fractional) number of days since the Julian epoch,
/// November 24, 4714 BCE 12:00 UTC, as if returned from SQLite's `julianday()` function.
fn decode_timestamp_from_float(value: f64) -> Option<Timestamp> {
    const EPOCH_IN_JULIAN_DAYS: f64 = 2_440_587.5;
    const SECONDS_IN_DAY: f64 = 86_400.0;

    let seconds = (value - EPOCH_IN_JULIAN_DAYS) * SECONDS_IN_DAY;

    // Returns an error for NaN, infinite and out-of-range values.
    let duration = SignedDuration::try_from_secs_f64(seconds).ok()?;

    Timestamp::from_duration(duration).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil;

    #[test]
    fn decodes_float_timestamp() {
        let julian_day = |unix_seconds: f64| 2_440_587.5 + unix_seconds / 86_400.0;

        for (unix_seconds, subsec_nanos) in [
            (0, 0),
            (1_546_405_820, 500_000_000),
            // Before the UNIX epoch, the sub-second part must not be applied in the wrong direction.
            (-1, 500_000_000),
            (-2, 250_000_000),
        ] {
            let expected = Timestamp::new(unix_seconds, subsec_nanos).unwrap();
            let input = julian_day(unix_seconds as f64 + f64::from(subsec_nanos) / 1e9);

            let decoded = decode_timestamp_from_float(input).unwrap();

            // Tolerance absorbs f64 round-off in the round-trip.
            let diff = decoded.duration_since(expected).abs();
            assert!(
                diff < SignedDuration::from_millis(1),
                "decoded {decoded} differs from {expected} by {diff:?}"
            );
        }

        assert_eq!(decode_timestamp_from_float(f64::NAN), None);
        assert_eq!(decode_timestamp_from_float(f64::INFINITY), None);
        assert_eq!(decode_timestamp_from_float(f64::MAX), None);
    }

    #[test]
    fn decodes_text_datetime() {
        let expected = civil::date(2019, 1, 2).at(5, 10, 20, 115_100_000);

        for s in [
            "2019-01-02 05:10:20.1151",
            "2019-01-02T05:10:20.1151",
            "2019-01-02T05:10:20.1151Z",
            "2019-01-02 05:10:20.1151+02:00",
        ] {
            assert_eq!(decode_datetime_from_text(s), Some(expected), "{s}");
        }

        assert_eq!(
            decode_datetime_from_text("2019-01-02 05:10"),
            Some(civil::date(2019, 1, 2).at(5, 10, 0, 0))
        );
        assert_eq!(decode_datetime_from_text("not a datetime"), None);
    }

    #[test]
    fn decodes_text_timestamp() {
        let expected = Offset::UTC
            .to_timestamp(civil::date(2019, 1, 2).at(5, 10, 20, 115_100_000))
            .unwrap();

        for s in [
            "2019-01-02 05:10:20.1151",
            "2019-01-02T05:10:20.1151Z",
            "2019-01-02 07:10:20.1151+02:00",
        ] {
            assert_eq!(decode_timestamp_from_text(s), Some(expected), "{s}");
        }
    }
}
