use bytes::Buf;
use jiff::civil::{Date, DateTime, Time};
use jiff::tz::Offset;
use jiff::{SignedDuration, Timestamp};
use sqlx_core::database::Database;

use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::{BoxDynError, UnexpectedNullError};
use crate::protocol::text::ColumnType;
use crate::type_info::MySqlTypeInfo;
use crate::types::{MySqlTime, MySqlTimeSign, Type};
use crate::{MySql, MySqlValueFormat, MySqlValueRef};

impl Type<MySql> for Timestamp {
    fn type_info() -> MySqlTypeInfo {
        MySqlTypeInfo::binary(ColumnType::Timestamp)
    }

    fn compatible(ty: &MySqlTypeInfo) -> bool {
        matches!(ty.r#type, ColumnType::Datetime | ColumnType::Timestamp)
    }
}

/// Note: assumes the connection's `time_zone` is set to `+00:00` (UTC).
impl Encode<'_, MySql> for Timestamp {
    fn encode_by_ref(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        Encode::<MySql>::encode(Offset::UTC.to_datetime(*self), buf)
    }

    fn size_hint(&self) -> usize {
        Offset::UTC.to_datetime(*self).size_hint()
    }
}

/// Note: assumes the connection's `time_zone` is set to `+00:00` (UTC).
impl<'r> Decode<'r, MySql> for Timestamp {
    fn decode(value: MySqlValueRef<'r>) -> Result<Self, BoxDynError> {
        let datetime: DateTime = Decode::<MySql>::decode(value)?;

        Ok(Offset::UTC.to_timestamp(datetime)?)
    }
}

impl Type<MySql> for Time {
    fn type_info() -> MySqlTypeInfo {
        MySqlTypeInfo::binary(ColumnType::Time)
    }
}

impl Encode<'_, MySql> for Time {
    fn encode_by_ref(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        let len = time_encoded_len(self);
        buf.push(len);

        // sign byte: Time is never negative
        buf.push(0);

        // Number of days in the interval; always 0 for time-of-day values.
        // https://mariadb.com/kb/en/resultset-row/#teimstamp-binary-encoding
        buf.extend_from_slice(&[0_u8; 4]);

        encode_time(self, len > 8, buf);

        Ok(IsNull::No)
    }

    fn size_hint(&self) -> usize {
        time_encoded_len(self) as usize + 1 // plus length byte
    }
}

impl<'r> Decode<'r, MySql> for Time {
    fn decode(value: MySqlValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.format() {
            MySqlValueFormat::Binary => {
                // Should never panic.
                MySqlTime::decode(value)?.try_into()
            }

            // Retaining this parsing for now as it allows us to cross-check our impl.
            MySqlValueFormat::Text => value.as_str()?.parse().map_err(Into::into),
        }
    }
}

impl TryFrom<MySqlTime> for Time {
    type Error = BoxDynError;

    fn try_from(time: MySqlTime) -> Result<Self, Self::Error> {
        if !time.is_valid_time_of_day() {
            return Err(
                format!("MySqlTime value out of range for `jiff::civil::Time`: {time}").into(),
            );
        }

        // `is_valid_time_of_day()` ensures these won't overflow
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        Ok(Time::new(
            time.hours() as i8,
            time.minutes() as i8,
            time.seconds() as i8,
            time.subsec_nanos() as i32,
        )?)
    }
}

impl From<MySqlTime> for SignedDuration {
    fn from(time: MySqlTime) -> Self {
        // `subsec_nanos()` is guaranteed to be between 0 and 10^9
        #[allow(clippy::cast_possible_wrap)]
        let nanos = time.subsec_nanos() as i32;

        let duration = SignedDuration::new(time.whole_seconds() as i64, nanos);

        if time.is_negative() {
            -duration
        } else {
            duration
        }
    }
}

impl TryFrom<SignedDuration> for MySqlTime {
    type Error = BoxDynError;

    fn try_from(value: SignedDuration) -> Result<Self, Self::Error> {
        let sign = if value.is_negative() {
            MySqlTimeSign::Negative
        } else {
            MySqlTimeSign::Positive
        };

        // Similar to `TryFrom<chrono::TimeDelta>`, use `std::time::Duration` as an intermediate.
        Ok(MySqlTime::try_from(value.unsigned_abs())?.with_sign(sign))
    }
}

impl Type<MySql> for SignedDuration {
    fn type_info() -> MySqlTypeInfo {
        MySqlTime::type_info()
    }
}

impl<'r> Decode<'r, MySql> for SignedDuration {
    fn decode(value: <MySql as Database>::ValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(MySqlTime::decode(value)?.into())
    }
}

impl Type<MySql> for Date {
    fn type_info() -> MySqlTypeInfo {
        MySqlTypeInfo::binary(ColumnType::Date)
    }
}

impl Encode<'_, MySql> for Date {
    fn encode_by_ref(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        buf.push(4);

        encode_date(self, buf)?;

        Ok(IsNull::No)
    }

    fn size_hint(&self) -> usize {
        5
    }
}

impl<'r> Decode<'r, MySql> for Date {
    fn decode(value: MySqlValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.format() {
            MySqlValueFormat::Binary => {
                let buf = value.as_bytes()?;

                // Row decoding should leave the length byte on the front.
                if buf.is_empty() {
                    return Err("empty buffer".into());
                }

                Ok(decode_date(&buf[1..])?.ok_or(UnexpectedNullError)?)
            }
            MySqlValueFormat::Text => value.as_str()?.parse().map_err(Into::into),
        }
    }
}

impl Type<MySql> for DateTime {
    fn type_info() -> MySqlTypeInfo {
        MySqlTypeInfo::binary(ColumnType::Datetime)
    }
}

impl Encode<'_, MySql> for DateTime {
    fn encode_by_ref(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        let len = datetime_encoded_len(self);
        buf.push(len);

        encode_date(&self.date(), buf)?;

        if len > 4 {
            encode_time(&self.time(), len > 7, buf);
        }

        Ok(IsNull::No)
    }

    fn size_hint(&self) -> usize {
        datetime_encoded_len(self) as usize + 1 // plus length byte
    }
}

impl<'r> Decode<'r, MySql> for DateTime {
    fn decode(value: MySqlValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.format() {
            MySqlValueFormat::Binary => {
                let mut buf = value.as_bytes()?;

                if buf.is_empty() {
                    return Err("empty buffer".into());
                }

                let len = buf.get_u8();

                let date = decode_date(buf)?.ok_or(UnexpectedNullError)?;

                let dt = if len > 4 {
                    date.to_datetime(decode_time(&buf[4..])?)
                } else {
                    date.to_datetime(Time::midnight())
                };

                Ok(dt)
            }

            // e.g. `2019-01-02 05:10:20` or `2019-01-02 05:10:20.115100`
            MySqlValueFormat::Text => value.as_str()?.parse().map_err(Into::into),
        }
    }
}

fn encode_date(date: &Date, buf: &mut Vec<u8>) -> Result<(), BoxDynError> {
    // MySQL supports years from 1000 - 9999
    let year =
        u16::try_from(date.year()).map_err(|_| format!("Date out of range for Mysql: {date}"))?;

    buf.extend_from_slice(&year.to_le_bytes());
    // `jiff` guarantees `1..=12` and `1..=31` respectively
    #[allow(clippy::cast_sign_loss)]
    {
        buf.push(date.month() as u8);
        buf.push(date.day() as u8);
    }

    Ok(())
}

fn decode_date(mut buf: &[u8]) -> Result<Option<Date>, BoxDynError> {
    match buf.len() {
        // zero buffer means a zero date (null)
        0 => return Ok(None),
        4.. => {}
        len => return Err(format!("expected at least 4 bytes for date, got {len}").into()),
    }

    let year = buf.get_u16_le();
    let month = buf.get_u8();
    let day = buf.get_u8();

    let year = i16::try_from(year)
        .map_err(|_| format!("MySQL returned year out of range for Date: {year}"))?;

    // A `u8` that doesn't fit in an `i8` is out of range for `Date::new()` anyway.
    #[allow(clippy::cast_possible_wrap)]
    Date::new(year, month as i8, day as i8)
        .map_err(Into::into)
        .map(Some)
}

fn encode_time(time: &Time, include_micros: bool, buf: &mut Vec<u8>) {
    // `jiff` guarantees these are all non-negative
    #[allow(clippy::cast_sign_loss)]
    {
        buf.push(time.hour() as u8);
        buf.push(time.minute() as u8);
        buf.push(time.second() as u8);

        if include_micros {
            buf.extend(&((time.subsec_nanosecond() / 1000) as u32).to_le_bytes());
        }
    }
}

fn decode_time(mut buf: &[u8]) -> Result<Time, BoxDynError> {
    let hour = buf.get_u8();
    let minute = buf.get_u8();
    let seconds = buf.get_u8();

    let micros = if !buf.is_empty() {
        // microseconds : int<EOF>
        buf.get_uint_le(buf.len())
    } else {
        0
    };

    let nanos = u32::try_from(micros)
        .ok()
        .and_then(|micros| micros.checked_mul(1000))
        .and_then(|nanos| i32::try_from(nanos).ok())
        .ok_or_else(|| format!("MySQL returned microseconds out of range: {micros}"))?;

    // A `u8` that doesn't fit in an `i8` is out of range for `Time::new()` anyway.
    #[allow(clippy::cast_possible_wrap)]
    Time::new(hour as i8, minute as i8, seconds as i8, nanos)
        .map_err(|e| format!("Time out of range for MySQL: {e}").into())
}

#[inline(always)]
fn datetime_encoded_len(datetime: &DateTime) -> u8 {
    // to save space the packet can be compressed:
    match (
        datetime.hour(),
        datetime.minute(),
        datetime.second(),
        datetime.subsec_nanosecond() / 1000,
    ) {
        // if hour, minutes, seconds and micro_seconds are all 0,
        // length is 4 and no other field is sent
        (0, 0, 0, 0) => 4,

        // if micro_seconds is 0, length is 7
        // and micro_seconds is not sent
        (_, _, _, 0) => 7,

        // otherwise length is 11
        (_, _, _, _) => 11,
    }
}

#[inline(always)]
fn time_encoded_len(time: &Time) -> u8 {
    if time.subsec_nanosecond() / 1000 == 0 {
        // if micro_seconds is 0, length is 8 and micro_seconds is not sent
        8
    } else {
        // otherwise length is 12
        12
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_date_rejects_short_buffer() {
        assert_eq!(decode_date(&[]).unwrap(), None);

        for len in 1..4 {
            assert!(decode_date(&[0xE3, 0x07, 1][..len]).is_err(), "len {len}");
        }

        assert_eq!(
            decode_date(&[0xE3, 0x07, 1, 2]).unwrap(),
            Some(Date::constant(2019, 1, 2))
        );
    }
}
