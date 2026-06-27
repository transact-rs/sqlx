use crate::arguments::SqliteArgumentsBuffer;
use crate::value::ValueRef;
use crate::{
    decode::Decode,
    encode::{Encode, IsNull},
    error::BoxDynError,
    type_info::DataType,
    types::Type,
    Sqlite, SqliteTypeInfo, SqliteValueRef,
};
use time::format_description::{well_known::Rfc3339, BorrowedFormatItem};
use time::macros::format_description as fd;
use time::{Date, OffsetDateTime, PrimitiveDateTime, Time};

impl Type<Sqlite> for OffsetDateTime {
    fn type_info() -> SqliteTypeInfo {
        SqliteTypeInfo(DataType::Datetime)
    }

    fn compatible(ty: &SqliteTypeInfo) -> bool {
        <PrimitiveDateTime as Type<Sqlite>>::compatible(ty)
    }
}

impl Type<Sqlite> for PrimitiveDateTime {
    fn type_info() -> SqliteTypeInfo {
        SqliteTypeInfo(DataType::Datetime)
    }

    fn compatible(ty: &SqliteTypeInfo) -> bool {
        matches!(
            ty.0,
            DataType::Datetime | DataType::Text | DataType::Integer | DataType::Int4
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

impl Encode<'_, Sqlite> for OffsetDateTime {
    fn encode_by_ref(&self, buf: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        Encode::<Sqlite>::encode(self.format(&Rfc3339)?, buf)
    }
}

impl Encode<'_, Sqlite> for PrimitiveDateTime {
    fn encode_by_ref(&self, buf: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        let format = fd!("[year]-[month]-[day] [hour]:[minute]:[second].[subsecond]");
        Encode::<Sqlite>::encode(self.format(&format)?, buf)
    }
}

impl Encode<'_, Sqlite> for Date {
    fn encode_by_ref(&self, buf: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        let format = fd!("[year]-[month]-[day]");
        Encode::<Sqlite>::encode(self.format(&format)?, buf)
    }
}

impl Encode<'_, Sqlite> for Time {
    fn encode_by_ref(&self, buf: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        let format = fd!("[hour]:[minute]:[second].[subsecond]");
        Encode::<Sqlite>::encode(self.format(&format)?, buf)
    }
}

impl<'r> Decode<'r, Sqlite> for OffsetDateTime {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        decode_offset_datetime(value)
    }
}

impl<'r> Decode<'r, Sqlite> for PrimitiveDateTime {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        decode_datetime(value)
    }
}

impl<'r> Decode<'r, Sqlite> for Date {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(Date::parse(
            value.text_borrowed()?,
            &fd!("[year]-[month]-[day]"),
        )?)
    }
}

impl<'r> Decode<'r, Sqlite> for Time {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        let value = value.text_borrowed()?;

        let sqlite_time_formats = &[
            fd!("[hour]:[minute]:[second].[subsecond]"),
            fd!("[hour]:[minute]:[second]"),
            fd!("[hour]:[minute]"),
        ];

        for format in sqlite_time_formats {
            if let Ok(dt) = Time::parse(value, &format) {
                return Ok(dt);
            }
        }

        Err(format!("invalid time: {value}").into())
    }
}

fn decode_offset_datetime(value: SqliteValueRef<'_>) -> Result<OffsetDateTime, BoxDynError> {
    let dt = match value.type_info().0 {
        DataType::Text => decode_offset_datetime_from_text(value.text_borrowed()?),
        DataType::Int4 | DataType::Integer => {
            Some(OffsetDateTime::from_unix_timestamp(value.int64()?)?)
        }

        _ => None,
    };

    if let Some(dt) = dt {
        Ok(dt)
    } else {
        Err(format!("invalid offset datetime: {}", value.text_borrowed()?).into())
    }
}

fn decode_offset_datetime_from_text(value: &str) -> Option<OffsetDateTime> {
    if let Ok(dt) = OffsetDateTime::parse(value, &Rfc3339) {
        return Some(dt);
    }

    if let Ok(dt) = OffsetDateTime::parse(value, formats::OFFSET_DATE_TIME) {
        return Some(dt);
    }

    if let Some(dt) = decode_datetime_from_text(value) {
        return Some(dt.assume_utc());
    }

    None
}

fn decode_datetime(value: SqliteValueRef<'_>) -> Result<PrimitiveDateTime, BoxDynError> {
    let dt = match value.type_info().0 {
        DataType::Text => decode_datetime_from_text(value.text_borrowed()?),
        DataType::Int4 | DataType::Integer => {
            let parsed = OffsetDateTime::from_unix_timestamp(value.int64()?).unwrap();
            Some(PrimitiveDateTime::new(parsed.date(), parsed.time()))
        }

        _ => None,
    };

    if let Some(dt) = dt {
        Ok(dt)
    } else {
        Err(format!("invalid datetime: {}", value.text_borrowed()?).into())
    }
}

fn decode_datetime_from_text(value: &str) -> Option<PrimitiveDateTime> {
    let default_format = fd!("[year]-[month]-[day] [hour]:[minute]:[second].[subsecond]");
    if let Ok(dt) = PrimitiveDateTime::parse(value, &default_format) {
        return Some(dt);
    }

    let formats = [
        BorrowedFormatItem::Compound(formats::PRIMITIVE_DATE_TIME_SPACE_SEPARATED),
        BorrowedFormatItem::Compound(formats::PRIMITIVE_DATE_TIME_T_SEPARATED),
    ];

    if let Ok(dt) = PrimitiveDateTime::parse(value, &BorrowedFormatItem::First(&formats)) {
        return Some(dt);
    }

    None
}

mod formats {
    use time::format_description::BorrowedFormatItem::{Component, Optional, StringLiteral};
    use time::format_description::{modifier, BorrowedFormatItem, Component::*};

    const YEAR: BorrowedFormatItem<'_> = Component(CalendarYearFullStandardRange(
        modifier::CalendarYearFullStandardRange::default().with_padding(modifier::Padding::Zero),
    ));

    const MONTH: BorrowedFormatItem<'_> = Component(MonthNumerical(
        modifier::MonthNumerical::default().with_padding(modifier::Padding::Zero),
    ));

    const DAY: BorrowedFormatItem<'_> = Component(Day({
        let mut value = modifier::Day::default();
        value.padding = modifier::Padding::Zero;
        value
    }));

    const HOUR: BorrowedFormatItem<'_> = Component(Hour24(
        modifier::Hour24::default().with_padding(modifier::Padding::Zero),
    ));

    const MINUTE: BorrowedFormatItem<'_> = Component(Minute({
        let mut value = modifier::Minute::default();
        value.padding = modifier::Padding::Zero;
        value
    }));

    const SECOND: BorrowedFormatItem<'_> = Component(Second({
        let mut value = modifier::Second::default();
        value.padding = modifier::Padding::Zero;
        value
    }));

    const SUBSECOND: BorrowedFormatItem<'_> = Component(Subsecond({
        let mut value = modifier::Subsecond::default();
        value.digits = modifier::SubsecondDigits::OneOrMore;
        value
    }));

    const OFFSET_HOUR: BorrowedFormatItem<'_> = Component(OffsetHour({
        let mut value = modifier::OffsetHour::default();
        value.sign_is_mandatory = true;
        value.padding = modifier::Padding::Zero;
        value
    }));

    const OFFSET_MINUTE: BorrowedFormatItem<'_> = Component(OffsetMinute({
        let mut value = modifier::OffsetMinute::default();
        value.padding = modifier::Padding::Zero;
        value
    }));

    pub(super) const OFFSET_DATE_TIME: &[BorrowedFormatItem<'_>] = {
        &[
            YEAR,
            StringLiteral("-"),
            MONTH,
            StringLiteral("-"),
            DAY,
            Optional(&StringLiteral(" ")),
            Optional(&StringLiteral("T")),
            HOUR,
            StringLiteral(":"),
            MINUTE,
            Optional(&StringLiteral(":")),
            Optional(&SECOND),
            Optional(&StringLiteral(".")),
            Optional(&SUBSECOND),
            Optional(&OFFSET_HOUR),
            Optional(&StringLiteral(":")),
            Optional(&OFFSET_MINUTE),
        ]
    };

    pub(super) const PRIMITIVE_DATE_TIME_SPACE_SEPARATED: &[BorrowedFormatItem<'_>] = {
        &[
            YEAR,
            StringLiteral("-"),
            MONTH,
            StringLiteral("-"),
            DAY,
            StringLiteral(" "),
            HOUR,
            StringLiteral(":"),
            MINUTE,
            Optional(&StringLiteral(":")),
            Optional(&SECOND),
            Optional(&StringLiteral(".")),
            Optional(&SUBSECOND),
            Optional(&StringLiteral("Z")),
        ]
    };

    pub(super) const PRIMITIVE_DATE_TIME_T_SEPARATED: &[BorrowedFormatItem<'_>] = {
        &[
            YEAR,
            StringLiteral("-"),
            MONTH,
            StringLiteral("-"),
            DAY,
            StringLiteral("T"),
            HOUR,
            StringLiteral(":"),
            MINUTE,
            Optional(&StringLiteral(":")),
            Optional(&SECOND),
            Optional(&StringLiteral(".")),
            Optional(&SUBSECOND),
            Optional(&StringLiteral("Z")),
        ]
    };
}
