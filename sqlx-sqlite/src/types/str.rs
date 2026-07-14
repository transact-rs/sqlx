use crate::arguments::SqliteArgumentsBuffer;
use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::type_info::DataType;
use crate::types::Type;
use crate::{Sqlite, SqliteArgumentValue, SqliteTypeInfo, SqliteValueRef};
use sqlx_core::database::Database;
use std::borrow::Cow;
use std::rc::Rc;
use std::sync::Arc;

impl Type<Sqlite> for str {
    fn type_info() -> SqliteTypeInfo {
        SqliteTypeInfo(DataType::Text)
    }
}

impl Encode<'_, Sqlite> for &'_ str {
    fn encode_by_ref(&self, args: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        args.push(SqliteArgumentValue::Text(Arc::new(self.to_string())));

        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Sqlite> for &'r str {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(value.text_borrowed()?)
    }
}

impl Encode<'_, Sqlite> for Box<str> {
    fn encode(self, args: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        args.push(SqliteArgumentValue::TextSlice(Arc::from(self)));

        Ok(IsNull::No)
    }

    fn encode_by_ref(&self, args: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        args.push(SqliteArgumentValue::Text(Arc::new(self.to_string())));

        Ok(IsNull::No)
    }
}

impl Type<Sqlite> for String {
    fn type_info() -> SqliteTypeInfo {
        <&str as Type<Sqlite>>::type_info()
    }
}

impl Encode<'_, Sqlite> for String {
    fn encode(self, args: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        args.push(SqliteArgumentValue::Text(Arc::new(self)));

        Ok(IsNull::No)
    }

    fn encode_by_ref(&self, args: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        args.push(SqliteArgumentValue::Text(Arc::new(self.clone())));

        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Sqlite> for String {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(value.text_owned()?)
    }
}

impl Encode<'_, Sqlite> for Cow<'_, str> {
    fn encode(self, args: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        args.push(SqliteArgumentValue::Text(Arc::new(self.into())));

        Ok(IsNull::No)
    }

    fn encode_by_ref(&self, args: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        args.push(SqliteArgumentValue::Text(Arc::new(self.to_string())));

        Ok(IsNull::No)
    }
}

impl Encode<'_, Sqlite> for Arc<str> {
    fn encode(self, args: &mut <Sqlite as Database>::ArgumentBuffer) -> Result<IsNull, BoxDynError>
    where
        Self: Sized,
    {
        args.push(SqliteArgumentValue::TextSlice(self));

        Ok(IsNull::No)
    }

    fn encode_by_ref(&self, args: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        <String as Encode<'_, Sqlite>>::encode(self.to_string(), args)
    }
}

impl Encode<'_, Sqlite> for Rc<str> {
    fn encode_by_ref(&self, args: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        <String as Encode<'_, Sqlite>>::encode(self.to_string(), args)
    }
}
