use bytes::{Buf, Bytes};

use crate::column::MySqlColumn;
use crate::error::Error;
use crate::io::MySqlBufExt;
use crate::io::ProtocolDecode;
use crate::protocol::Row;

#[derive(Debug)]
pub(crate) struct TextRow(pub(crate) Row);

impl<'de> ProtocolDecode<'de, &'de [MySqlColumn]> for TextRow {
    fn decode_with(mut buf: Bytes, columns: &'de [MySqlColumn]) -> Result<Self, Error> {
        let storage = buf.clone();
        let offset = buf.len();

        let mut values = Vec::with_capacity(columns.len());

        for c in columns {
            if buf.is_empty() {
                return Err(err_protocol!(
                    "buffer exhausted when reading data for column {:?}; expected a length-encoded value or NULL marker, but no bytes remain in buffer. Malformed packet or protocol error?",
                    c
                ));
            }

            if buf[0] == 0xfb {
                // NULL is sent as 0xfb
                values.push(None);
                buf.advance(1);
            } else {
                let size = buf.get_uint_lenenc()?;
                if (buf.remaining() as u64) < size {
                    return Err(err_protocol!(
                        "buffer exhausted when reading data for column {:?}; decoded length is {}, but only {} bytes remain in buffer. Malformed packet or protocol error?",
                        c,
                        size,
                        buf.remaining()));
                }
                let size = usize::try_from(size)
                    .map_err(|_| err_protocol!("TextRow length out of range: {size}"))?;

                let offset = offset - buf.len();

                values.push(Some(offset..(offset + size)));

                buf.advance(size);
            }
        }

        Ok(TextRow(Row { values, storage }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ext::ustr::UStr;
    use crate::protocol::text::ColumnType;
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
    fn decodes_null_and_value() {
        let columns = [
            test_column("a", ColumnType::Long),
            test_column("b", ColumnType::VarString),
        ];

        let row = TextRow::decode_with(Bytes::from_static(&[0xfb, 2, b'h', b'i']), &columns)
            .unwrap()
            .0;

        assert_eq!(row.get(0), None);
        assert_eq!(row.get(1), Some(&b"hi"[..]));
    }

    #[test]
    fn rejects_exhausted_buffer_mid_row() {
        // two columns are declared but the buffer only holds the value for the first
        let columns = [
            test_column("a", ColumnType::Long),
            test_column("b", ColumnType::Long),
        ];

        let err = TextRow::decode_with(Bytes::from_static(&[1, b'x']), &columns).unwrap_err();

        assert!(
            err.to_string().contains("no bytes remain in buffer"),
            "{err}"
        );
    }
}
