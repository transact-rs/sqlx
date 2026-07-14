use crate::any::types::str;
use crate::any::{Any, AnyTypeInfo, AnyTypeInfoKind, AnyValueKind};
use crate::database::Database;
use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::types::Type;
use std::sync::Arc;

impl Type<Any> for str {
    fn type_info() -> AnyTypeInfo {
        AnyTypeInfo {
            kind: AnyTypeInfoKind::Text,
        }
    }
}

impl<'a> Encode<'a, Any> for &'a str {
    fn encode(self, buf: &mut <Any as Database>::ArgumentBuffer) -> Result<IsNull, BoxDynError>
    where
        Self: Sized,
    {
        buf.0.push(AnyValueKind::Text(Arc::new(self.into())));
        Ok(IsNull::No)
    }

    fn encode_by_ref(
        &self,
        buf: &mut <Any as Database>::ArgumentBuffer,
    ) -> Result<IsNull, BoxDynError> {
        (*self).encode(buf)
    }
}

impl<'a> Decode<'a, Any> for &'a str {
    fn decode(value: <Any as Database>::ValueRef<'a>) -> Result<Self, BoxDynError> {
        match value.kind {
            AnyValueKind::Text(text) => Ok(text.as_str()),
            other => other.unexpected(),
        }
    }
}

impl Type<Any> for String {
    fn type_info() -> AnyTypeInfo {
        <str as Type<Any>>::type_info()
    }
}

impl Encode<'_, Any> for String {
    fn encode(self, buf: &mut <Any as Database>::ArgumentBuffer) -> Result<IsNull, BoxDynError> {
        buf.0.push(AnyValueKind::Text(Arc::new(self)));
        Ok(IsNull::No)
    }

    fn encode_by_ref(
        &self,
        buf: &mut <Any as Database>::ArgumentBuffer,
    ) -> Result<IsNull, BoxDynError> {
        buf.0.push(AnyValueKind::Text(Arc::new(self.clone())));
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Any> for String {
    fn decode(value: <Any as Database>::ValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.kind {
            AnyValueKind::Text(text) => Ok(text.to_string()),
            other => other.unexpected(),
        }
    }
}
