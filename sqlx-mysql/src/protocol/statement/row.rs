use bytes::{Buf, Bytes};

use crate::error::Error;
use crate::io::MySqlBufExt;
use crate::io::{BufExt, ProtocolDecode};
use crate::protocol::text::ColumnType;
use crate::protocol::Row;
use crate::MySqlColumn;

// https://dev.mysql.com/doc/internals/en/binary-protocol-resultset-row.html#packet-ProtocolBinary::ResultsetRow
// https://dev.mysql.com/doc/internals/en/binary-protocol-value.html

#[derive(Debug)]
pub(crate) struct BinaryRow(pub(crate) Row);

impl<'de> ProtocolDecode<'de, &'de [MySqlColumn]> for BinaryRow {
    fn decode_with(mut buf: Bytes, columns: &'de [MySqlColumn]) -> Result<Self, Error> {
        let header = buf.get_u8();
        if header != 0 {
            return Err(err_protocol!(
                "expected 0x00 (ROW) but found 0x{:02x}",
                header
            ));
        }

        let storage = buf.clone();
        let offset = buf.len();

        let null_bitmap_len = (columns.len() + 9) / 8;

        if buf.remaining() < null_bitmap_len {
            return Err(err_protocol!(
                "expected null-bitmap of {null_bitmap_len} bytes in binary row but got {} bytes",
                buf.remaining()
            ));
        }

        let null_bitmap = buf.get_bytes(null_bitmap_len);

        let mut values = Vec::with_capacity(columns.len());

        for (column_idx, column) in columns.iter().enumerate() {
            // NOTE: the column index starts at the 3rd bit
            let column_null_idx = column_idx + 2;

            let byte_idx = column_null_idx / 8;
            let bit_idx = column_null_idx % 8;

            let is_null = null_bitmap[byte_idx] & (1u8 << bit_idx) != 0;

            if is_null {
                values.push(None);
                continue;
            }

            // NOTE: MySQL will never generate NULL types for non-NULL values
            let type_info = &column.type_info;

            // Unlike Postgres, MySQL does not length-prefix every value in a binary row.
            // Values are *either* fixed-length or length-prefixed,
            // so we need to inspect the type code to be sure.
            let size: usize = match type_info.r#type {
                // All fixed-length types.
                ColumnType::LongLong => 8,
                ColumnType::Long | ColumnType::Int24 => 4,
                ColumnType::Short | ColumnType::Year => 2,
                ColumnType::Tiny => 1,
                ColumnType::Float => 4,
                ColumnType::Double => 8,

                // Blobs and strings are prefixed with their length,
                // which is itself a length-encoded integer.
                ColumnType::String
                | ColumnType::VarChar
                | ColumnType::VarString
                | ColumnType::Enum
                | ColumnType::Set
                | ColumnType::LongBlob
                | ColumnType::MediumBlob
                | ColumnType::Blob
                | ColumnType::TinyBlob
                | ColumnType::Geometry
                | ColumnType::Vector
                | ColumnType::Bit
                | ColumnType::Decimal
                | ColumnType::Json
                | ColumnType::NewDecimal => {
                    let size = buf.get_uint_lenenc()?;
                    usize::try_from(size)
                        .map_err(|_| err_protocol!("BLOB length out of range: {size}"))?
                }

                // Like strings and blobs, these values are variable-length.
                // Unlike strings and blobs, however, they exclusively use one byte for length.
                ColumnType::Time
                | ColumnType::Timestamp
                | ColumnType::Date
                | ColumnType::Datetime => {
                    // Leave the length byte on the front of the value because decoding uses it.
                    usize::from(*buf.first().ok_or(err_protocol!(
                        "buffer exhausted when reading data for column {:?}; expected a length byte but no bytes remain in buffer",
                        column
                    ))?) + 1
                }

                // NOTE: MySQL will never legitimately generate NULL types for non-NULL values,
                // but a hostile or broken server can send column type 0x06 (NULL) with the
                // null-bit clear; report a protocol error instead of panicking
                ColumnType::Null => {
                    return Err(err_protocol!(
                        "column {:?} has type NULL (0x06) but its null-bit is not set in the binary row",
                        column
                    ));
                }
            };

            if buf.remaining() < size {
                return Err(err_protocol!(
                    "buffer exhausted when reading data for column {:?}; expected value of {size} bytes but only {} bytes remain in buffer",
                    column,
                    buf.remaining()
                ));
            }

            let offset = offset - buf.len();

            values.push(Some(offset..(offset + size)));

            buf.advance(size);
        }

        Ok(BinaryRow(Row { values, storage }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ext::ustr::UStr;
    use crate::MySqlTypeInfo;
    use sqlx_core::column::ColumnOrigin;

    fn test_column(name: &str, ty: ColumnType) -> MySqlColumn {
        MySqlColumn {
            ordinal: 0,
            name: UStr::new(name),
            type_info: MySqlTypeInfo::binary(ty),
            origin: ColumnOrigin::Expression,
            flags: None,
        }
    }

    #[test]
    fn decodes_simple_row() {
        let columns = [
            test_column("a", ColumnType::Long),
            test_column("b", ColumnType::VarString),
        ];

        // 0x00 header, 1 byte null-bitmap (both columns non-NULL),
        // a = 16909060i32 little-endian, b = "hi"
        let row = BinaryRow::decode_with(
            Bytes::from_static(&[0x00, 0x00, 0x04, 0x03, 0x02, 0x01, 2, b'h', b'i']),
            &columns,
        )
        .unwrap()
        .0;

        assert_eq!(row.get(0), Some(&[4, 3, 2, 1][..]));
        assert_eq!(row.get(1), Some(&b"hi"[..]));
    }

    #[test]
    fn decodes_null_value() {
        let columns = [
            test_column("a", ColumnType::Long),
            test_column("b", ColumnType::VarString),
        ];

        // null-bitmap with bit 2 set => the first column is NULL
        let row =
            BinaryRow::decode_with(Bytes::from_static(&[0x00, 0x04, 2, b'h', b'i']), &columns)
                .unwrap()
                .0;

        assert_eq!(row.get(0), None);
        assert_eq!(row.get(1), Some(&b"hi"[..]));
    }

    #[test]
    fn rejects_truncated_null_bitmap() {
        // 8 columns require a 2 byte null-bitmap, only 1 byte is provided
        let columns: [_; 8] = std::array::from_fn(|_| test_column("a", ColumnType::Long));

        let err = BinaryRow::decode_with(Bytes::from_static(&[0x00, 0x00]), &columns).unwrap_err();

        assert!(err.to_string().contains("null-bitmap"), "{err}");
    }

    #[test]
    fn rejects_truncated_fixed_width_value() {
        // a LongLong value needs 8 bytes, only 3 are provided
        let columns = [test_column("a", ColumnType::LongLong)];

        let err = BinaryRow::decode_with(Bytes::from_static(&[0x00, 0x00, 1, 2, 3]), &columns)
            .unwrap_err();

        assert!(err.to_string().contains("value of 8 bytes"), "{err}");
    }

    #[test]
    fn rejects_missing_temporal_length_byte() {
        // the column type signals a length byte but the buffer is exhausted
        let columns = [test_column("a", ColumnType::Date)];

        let err = BinaryRow::decode_with(Bytes::from_static(&[0x00, 0x00]), &columns).unwrap_err();

        assert!(err.to_string().contains("expected a length byte"), "{err}");
    }

    #[test]
    fn rejects_null_type_with_null_bit_clear() {
        // type 0x06 (NULL) with the null-bit clear is not a panic, it is a protocol error
        let columns = [test_column("a", ColumnType::Null)];

        let err = BinaryRow::decode_with(Bytes::from_static(&[0x00, 0x00]), &columns).unwrap_err();

        assert!(err.to_string().contains("type NULL"), "{err}");
    }
}
