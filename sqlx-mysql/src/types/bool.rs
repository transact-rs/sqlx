use crate::collation::Collation;
use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::types::Type;
use crate::{
    protocol::text::{ColumnFlags, ColumnType},
    MySql, MySqlTypeInfo, MySqlValueRef,
};

impl Type<MySql> for bool {
    fn type_info() -> MySqlTypeInfo {
        // MySQL has no actual `BOOLEAN` type, the type is an alias of `[UNSIGNED] TINYINT(1)`
        MySqlTypeInfo {
            flags: ColumnFlags::BINARY | ColumnFlags::UNSIGNED,
            collation: Collation::BINARY,
            max_size: Some(1),
            r#type: ColumnType::Tiny,
        }
    }

    fn compatible(ty: &MySqlTypeInfo) -> bool {
        match ty.r#type {
            ColumnType::Bit => ty.max_size == Some(1),
            ColumnType::Tiny
            | ColumnType::Short
            | ColumnType::Long
            | ColumnType::Int24
            | ColumnType::LongLong => true,
            _ => false,
        }
    }
}

impl Encode<'_, MySql> for bool {
    fn encode_by_ref(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        <i8 as Encode<MySql>>::encode(*self as i8, buf)
    }
}

impl Decode<'_, MySql> for bool {
    fn decode(value: MySqlValueRef<'_>) -> Result<Self, BoxDynError> {
        match value.type_info.r#type {
            // BIT values are binary even in text-protocol rows.
            ColumnType::Bit => Ok(<u64 as Decode<MySql>>::decode(value)? != 0),
            _ => Ok(<i8 as Decode<MySql>>::decode(value)? != 0),
        }
    }
}
