use crate::any::{Any, AnyArgumentBuffer, AnyTypeInfo, AnyTypeInfoKind, AnyValueKind, AnyValueRef};
use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::types::{Json, Type};
use serde::{Deserialize, Serialize};

impl<T> Type<Any> for Json<T> {
    fn type_info() -> AnyTypeInfo {
        AnyTypeInfo {
            kind: AnyTypeInfoKind::Json,
        }
    }

    fn compatible(ty: &AnyTypeInfo) -> bool {
        matches!(
            ty.kind,
            AnyTypeInfoKind::Json | AnyTypeInfoKind::Text | AnyTypeInfoKind::Blob
        )
    }
}

impl<T> Encode<'_, Any> for Json<T>
where
    T: Serialize,
{
    fn encode_by_ref(&self, buf: &mut AnyArgumentBuffer) -> Result<IsNull, BoxDynError> {
        let json_string = self.encode_to_string()?;
        let raw_value = serde_json::value::RawValue::from_string(json_string)?;
        buf.0.push(AnyValueKind::Json(raw_value));
        Ok(IsNull::No)
    }
}

impl<T> Decode<'_, Any> for Json<T>
where
    T: for<'de> Deserialize<'de>,
{
    fn decode(value: AnyValueRef<'_>) -> Result<Self, BoxDynError> {
        match value.kind {
            #[cfg(feature = "json")]
            AnyValueKind::Json(raw) => Json::decode_from_string(raw.get()),
            AnyValueKind::Text(text) => Json::decode_from_string(text),
            AnyValueKind::Blob(blob) => Json::decode_from_bytes(blob),
            other => other.unexpected(),
        }
    }
}
