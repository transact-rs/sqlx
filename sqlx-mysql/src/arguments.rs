use crate::encode::{Encode, IsNull};
use crate::types::Type;
use crate::{MySql, MySqlTypeInfo};
pub(crate) use sqlx_core::arguments::*;
use sqlx_core::error::BoxDynError;
use std::ops::Deref;

/// Implementation of [`Arguments`] for MySQL.
#[derive(Debug, Default, Clone)]
pub struct MySqlArguments {
    pub(crate) values: Vec<u8>,
    pub(crate) types: Vec<MySqlTypeInfo>,
    pub(crate) null_bitmap: NullBitMap,
}

impl MySqlArguments {
    pub(crate) fn add<'q, T>(&mut self, value: T) -> Result<(), BoxDynError>
    where
        T: Encode<'q, MySql> + Type<MySql>,
    {
        let ty = value.produces().unwrap_or_else(T::type_info);

        let value_length_before_encoding = self.values.len();
        let is_null = match value.encode(&mut self.values) {
            Ok(is_null) => is_null,
            Err(error) => {
                // reset the value buffer to its previous value if encoding failed so we don't leave a half-encoded value behind
                self.values.truncate(value_length_before_encoding);
                return Err(error);
            }
        };

        self.types.push(ty);
        self.null_bitmap.push(is_null);

        Ok(())
    }

    /// The type of each bound parameter, in bind order.
    pub fn types(&self) -> &[MySqlTypeInfo] {
        &self.types
    }

    /// The bound parameters, concatenated in bind order, in the binary protocol encoding of
    /// `COM_STMT_EXECUTE`. A parameter that [`is_null`][Self::is_null] has no bytes here.
    pub fn values(&self) -> &[u8] {
        &self.values
    }

    /// Whether the parameter at `index` was bound as `NULL`.
    ///
    /// Returns `false` if `index` is out of bounds.
    pub fn is_null(&self, index: usize) -> bool {
        index < self.types.len()
            && self.null_bitmap[index / (u8::BITS as usize)] & (1 << (index % (u8::BITS as usize)))
                != 0
    }
}

impl Arguments for MySqlArguments {
    type Database = MySql;

    fn reserve(&mut self, len: usize, size: usize) {
        self.types.reserve(len);
        self.values.reserve(size);
    }

    fn add<'t, T>(&mut self, value: T) -> Result<(), BoxDynError>
    where
        T: Encode<'t, Self::Database> + Type<Self::Database>,
    {
        self.add(value)
    }

    fn len(&self) -> usize {
        self.types.len()
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct NullBitMap {
    bytes: Vec<u8>,
    length: usize,
}

impl NullBitMap {
    fn push(&mut self, is_null: IsNull) {
        let byte_index = self.length / (u8::BITS as usize);
        let bit_offset = self.length % (u8::BITS as usize);

        if bit_offset == 0 {
            self.bytes.push(0);
        }

        self.bytes[byte_index] |= u8::from(is_null.is_null()) << bit_offset;
        self.length += 1;
    }
}

impl Deref for NullBitMap {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn null_bit_map_should_push_is_null() {
        let mut bit_map = NullBitMap::default();

        bit_map.push(IsNull::Yes);
        bit_map.push(IsNull::No);
        bit_map.push(IsNull::Yes);
        bit_map.push(IsNull::No);
        bit_map.push(IsNull::Yes);
        bit_map.push(IsNull::No);
        bit_map.push(IsNull::Yes);
        bit_map.push(IsNull::No);
        bit_map.push(IsNull::Yes);

        assert_eq!([0b01010101, 0b1].as_slice(), bit_map.deref());
    }

    #[test]
    fn arguments_expose_types_values_and_nulls() {
        use crate::type_info::TypeInfo;

        let mut args = MySqlArguments::default();
        args.add(7_u64).unwrap();
        args.add(Option::<i32>::None).unwrap();
        args.add("ab").unwrap();

        let names: Vec<_> = args.types().iter().map(TypeInfo::name).collect();
        assert_eq!(names, ["BIGINT UNSIGNED", "INT", "VARCHAR"]);
        assert_eq!(args.values(), [7, 0, 0, 0, 0, 0, 0, 0, 2, b'a', b'b']);
        assert_eq!(
            [
                args.is_null(0),
                args.is_null(1),
                args.is_null(2),
                args.is_null(3)
            ],
            [false, true, false, false],
        );
    }
}
