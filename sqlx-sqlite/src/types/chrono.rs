use std::fmt::Display;

use crate::value::ValueRef;
use crate::{
    decode::Decode,
    encode::{Encode, IsNull},
    error::BoxDynError,
    type_info::DataType,
    types::Type,
    Sqlite, SqliteArgumentsBuffer, SqliteTypeInfo, SqliteValueRef,
};
use chrono::FixedOffset;
use chrono::{
    DateTime, Local, NaiveDate, NaiveDateTime, NaiveTime, Offset, SecondsFormat, TimeZone, Utc,
};

impl<Tz: TimeZone> Type<Sqlite> for DateTime<Tz> {
    fn type_info() -> SqliteTypeInfo {
        SqliteTypeInfo(DataType::Datetime)
    }

    fn compatible(ty: &SqliteTypeInfo) -> bool {
        <NaiveDateTime as Type<Sqlite>>::compatible(ty)
    }
}

impl Type<Sqlite> for NaiveDateTime {
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

impl Type<Sqlite> for NaiveDate {
    fn type_info() -> SqliteTypeInfo {
        SqliteTypeInfo(DataType::Date)
    }

    fn compatible(ty: &SqliteTypeInfo) -> bool {
        matches!(ty.0, DataType::Date | DataType::Text)
    }
}

impl Type<Sqlite> for NaiveTime {
    fn type_info() -> SqliteTypeInfo {
        SqliteTypeInfo(DataType::Time)
    }

    fn compatible(ty: &SqliteTypeInfo) -> bool {
        matches!(ty.0, DataType::Time | DataType::Text)
    }
}

impl<Tz: TimeZone> Encode<'_, Sqlite> for DateTime<Tz>
where
    Tz::Offset: Display,
{
    fn encode_by_ref(&self, buf: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        Encode::<Sqlite>::encode(self.to_rfc3339_opts(SecondsFormat::AutoSi, false), buf)
    }
}

impl Encode<'_, Sqlite> for NaiveDateTime {
    fn encode_by_ref(&self, buf: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        Encode::<Sqlite>::encode(self.format("%F %T%.f").to_string(), buf)
    }
}

impl Encode<'_, Sqlite> for NaiveDate {
    fn encode_by_ref(&self, buf: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        Encode::<Sqlite>::encode(self.format("%F").to_string(), buf)
    }
}

impl Encode<'_, Sqlite> for NaiveTime {
    fn encode_by_ref(&self, buf: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        Encode::<Sqlite>::encode(self.format("%T%.f").to_string(), buf)
    }
}

impl<'r> Decode<'r, Sqlite> for DateTime<Utc> {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(Utc.from_utc_datetime(&decode_datetime(value)?.naive_utc()))
    }
}

impl<'r> Decode<'r, Sqlite> for DateTime<Local> {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(Local.from_utc_datetime(&decode_datetime(value)?.naive_utc()))
    }
}

impl<'r> Decode<'r, Sqlite> for DateTime<FixedOffset> {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        decode_datetime(value)
    }
}

fn decode_datetime(value: SqliteValueRef<'_>) -> Result<DateTime<FixedOffset>, BoxDynError> {
    let dt = match value.type_info().0 {
        DataType::Text => decode_datetime_from_text(value.text_borrowed()?),
        DataType::Int4 | DataType::Integer => decode_datetime_from_int(value.int64()?),
        DataType::Float => decode_datetime_from_float(value.double()?),

        _ => None,
    };

    if let Some(dt) = dt {
        Ok(dt)
    } else {
        Err(format!("invalid datetime: {}", value.text_borrowed()?).into())
    }
}

fn decode_datetime_from_text(value: &str) -> Option<DateTime<FixedOffset>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some(dt);
    }

    // Loop over common date time patterns, inspired by Diesel
    // https://github.com/diesel-rs/diesel/blob/93ab183bcb06c69c0aee4a7557b6798fd52dd0d8/diesel/src/sqlite/types/date_and_time/chrono.rs#L56-L97
    let sqlite_datetime_formats = &[
        // Most likely format
        "%F %T%.f",
        // Other formats in order of appearance in docs
        "%F %R",
        "%F %RZ",
        "%F %R%:z",
        "%F %T%.fZ",
        "%F %T%.f%:z",
        "%FT%R",
        "%FT%RZ",
        "%FT%R%:z",
        "%FT%T%.f",
        "%FT%T%.fZ",
        "%FT%T%.f%:z",
    ];

    for format in sqlite_datetime_formats {
        if let Ok(dt) = DateTime::parse_from_str(value, format) {
            return Some(dt);
        }

        if let Ok(dt) = NaiveDateTime::parse_from_str(value, format) {
            return Some(Utc.fix().from_utc_datetime(&dt));
        }
    }

    None
}

fn decode_datetime_from_int(value: i64) -> Option<DateTime<FixedOffset>> {
    Utc.fix().timestamp_opt(value, 0).single()
}

fn decode_datetime_from_float(value: f64) -> Option<DateTime<FixedOffset>> {
    let epoch_in_julian_days = 2_440_587.5;
    let seconds_in_day = 86400.0;
    let timestamp = (value - epoch_in_julian_days) * seconds_in_day;

    if !timestamp.is_finite() {
        return None;
    }

    // We don't really have a choice but to do lossy casts for this conversion
    // We checked above if the value is infinite or NaN which could otherwise cause problems
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        // Split into whole seconds and a non-negative sub-second remainder.
        // `timestamp_opt` always adds `nanos` *forward* in time, so we must round
        // `seconds` toward negative infinity (not toward zero) and take the
        // fraction relative to that. Using `trunc()`/`fract().abs()` here would
        // push pre-epoch timestamps up to ~2 seconds off (and onto the wrong side
        // of the epoch), since the fractional part is negative below zero.
        let seconds = timestamp.floor();
        let nanos = ((timestamp - seconds) * 1E9) as u32;

        Utc.fix().timestamp_opt(seconds as i64, nanos).single()
    }
}

impl<'r> Decode<'r, Sqlite> for NaiveDateTime {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(decode_datetime(value)?.naive_local())
    }
}

impl<'r> Decode<'r, Sqlite> for NaiveDate {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(NaiveDate::parse_from_str(value.text_borrowed()?, "%F")?)
    }
}

impl<'r> Decode<'r, Sqlite> for NaiveTime {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        let value = value.text_borrowed()?;

        // Loop over common time patterns, inspired by Diesel
        // https://github.com/diesel-rs/diesel/blob/93ab183bcb06c69c0aee4a7557b6798fd52dd0d8/diesel/src/sqlite/types/date_and_time/chrono.rs#L29-L47
        #[rustfmt::skip] // don't like how rustfmt mangles the comments
        let sqlite_time_formats = &[
            // Most likely format
            "%T.f", "%T%.f",
            // Other formats in order of appearance in docs
            "%R", "%RZ", "%T%.fZ", "%R%:z", "%T%.f%:z",
        ];

        for format in sqlite_time_formats {
            if let Ok(dt) = NaiveTime::parse_from_str(value, format) {
                return Ok(dt);
            }
        }

        Err(format!("invalid time: {value}").into())
    }
}

#[cfg(test)]
mod tests {
    use super::decode_datetime_from_float;
    use chrono::{Offset, TimeZone, Utc};

    // SQLite may store a datetime as a REAL holding a Julian day number, e.g. the
    // result of `julianday(...)`. This mirrors that conversion so a test can feed
    // `decode_datetime_from_float` the value for a known instant.
    fn julian_day(unix_seconds: f64) -> f64 {
        2_440_587.5 + unix_seconds / 86_400.0
    }

    // Assert that the Julian day for `unix_seconds + subsec_nanos` decodes back to
    // that same instant. The tolerance only absorbs f64 round-off in the
    // round-trip (tens of microseconds), well below the errors under test.
    #[track_caller]
    fn assert_decodes_near(unix_seconds: i64, subsec_nanos: u32) {
        let input = julian_day(unix_seconds as f64 + f64::from(subsec_nanos) / 1e9);
        let decoded = decode_datetime_from_float(input).expect("valid Julian day should decode");
        let expected = Utc
            .fix()
            .timestamp_opt(unix_seconds, subsec_nanos)
            .single()
            .unwrap();

        let diff_us = (decoded - expected)
            .num_microseconds()
            .expect("difference fits in microseconds")
            .abs();
        assert!(
            diff_us < 1_000,
            "decoded {decoded} differs from expected {expected} by {diff_us} us",
        );
    }

    // A Julian day landing before the UNIX epoch must keep its sub-second part in
    // the correct direction. Before the fix these decoded up to ~2 seconds off,
    // and could even cross to the wrong side of the epoch.
    #[test]
    fn decodes_pre_epoch_float_datetime() {
        // 1969-12-31 23:59:59.500 UTC
        assert_decodes_near(-1, 500_000_000);
        // 1969-12-31 23:59:58.750 UTC
        assert_decodes_near(-2, 750_000_000);
    }

    // Post-epoch values were already correct; guard against a regression.
    #[test]
    fn decodes_post_epoch_float_datetime() {
        // 1970-01-01 00:00:01.500 UTC
        assert_decodes_near(1, 500_000_000);
    }
}
