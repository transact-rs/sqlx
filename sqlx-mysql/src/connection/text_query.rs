//! Client-side parameter interpolation for the COM_QUERY (text) protocol.
//!
//! When `statement_cache_capacity == 0`, the executor opts out of prepared
//! statements entirely. For queries with bind arguments we splice the values
//! directly into the SQL using `mysql_common::Value::as_sql`, which applies
//! the same quoting/escaping rules used by the canonical MySQL client.
//!
//! This module exists as a vendored patch on top of upstream sqlx; it is
//! self-contained so future upstream merges only touch tiny call-site hooks
//! in `executor.rs` and `mod.rs`.
use mysql_common::value::Value;

use crate::error::Error;
use crate::protocol::text::{ColumnFlags, ColumnType};
use crate::{MySqlArguments, MySqlTypeInfo};

/// Interpolate `arguments` into `sql` and return the resulting SQL string.
///
/// `no_backslash_escape` should reflect the connection's current
/// `SERVER_STATUS_NO_BACKSLASH_ESCAPES` status flag.
pub(crate) fn interpolate(
    sql: &str,
    arguments: &MySqlArguments,
    no_backslash_escape: bool,
) -> Result<String, Error> {
    let values = decode_arguments(arguments)?;
    splice(sql, &values, no_backslash_escape)
}

#[derive(Copy, Clone)]
enum State {
    Normal,
    /// Inside a `'…'`, `"…"`, or `` `…` `` literal; field is the opening byte.
    Quoted(u8),
    LineComment,
    BlockComment,
}

fn splice(sql: &str, values: &[Value], no_backslash_escape: bool) -> Result<String, Error> {
    let mut out = String::with_capacity(sql.len() + values.len() * 8);
    let bytes = sql.as_bytes();
    let mut next_param = 0usize;
    let mut state = State::Normal;
    let mut seg_start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        match state {
            State::Normal => match b {
                b'\'' | b'"' | b'`' => {
                    state = State::Quoted(b);
                    i += 1;
                }
                b'-' if bytes.get(i + 1) == Some(&b'-') => {
                    state = State::LineComment;
                    i += 2;
                }
                b'#' => {
                    state = State::LineComment;
                    i += 1;
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    state = State::BlockComment;
                    i += 2;
                }
                b'?' => {
                    out.push_str(&sql[seg_start..i]);
                    let value = values.get(next_param).ok_or_else(|| {
                        err_protocol!(
                            "interpolation: SQL has more `?` placeholders than bound arguments ({})",
                            values.len()
                        )
                    })?;
                    out.push_str(&value.as_sql(no_backslash_escape));
                    next_param += 1;
                    i += 1;
                    seg_start = i;
                }
                _ => i += 1,
            },
            State::Quoted(q) => {
                // Backslash escapes apply only in '…' and "…" string literals,
                // and only when sql_mode does not include NO_BACKSLASH_ESCAPES.
                if b == b'\\' && !no_backslash_escape && q != b'`' && i + 1 < bytes.len() {
                    i += 2;
                } else if b == q {
                    // Doubled-quote escape: `''`, `""`, or `` `` ``.
                    if bytes.get(i + 1) == Some(&q) {
                        i += 2;
                    } else {
                        state = State::Normal;
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            State::LineComment => {
                if b == b'\n' {
                    state = State::Normal;
                }
                i += 1;
            }
            State::BlockComment => {
                if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    state = State::Normal;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }
    }
    out.push_str(&sql[seg_start..]);

    if next_param != values.len() {
        return Err(err_protocol!(
            "interpolation: bound {} arguments but SQL contains {} `?` placeholders",
            values.len(),
            next_param
        ));
    }

    Ok(out)
}

fn decode_arguments(arguments: &MySqlArguments) -> Result<Vec<Value>, Error> {
    let mut out = Vec::with_capacity(arguments.types.len());
    let mut buf: &[u8] = &arguments.values;

    for (i, ty) in arguments.types.iter().enumerate() {
        if is_null(arguments, i) {
            out.push(Value::NULL);
            continue;
        }

        out.push(decode_one(ty, &mut buf)?);
    }

    if !buf.is_empty() {
        return Err(err_protocol!(
            "interpolation: {} trailing bytes after decoding {} parameters",
            buf.len(),
            arguments.types.len()
        ));
    }

    Ok(out)
}

fn is_null(arguments: &MySqlArguments, i: usize) -> bool {
    let bitmap: &[u8] = &arguments.null_bitmap;
    let byte = i / 8;
    let bit = i % 8;
    bitmap.get(byte).is_some_and(|b| (b >> bit) & 1 == 1)
}

fn decode_one(ty: &MySqlTypeInfo, buf: &mut &[u8]) -> Result<Value, Error> {
    let unsigned = ty.flags.contains(ColumnFlags::UNSIGNED);

    match ty.r#type {
        ColumnType::Tiny => {
            let bytes = take_fixed::<1>(buf)?;
            Ok(if unsigned {
                Value::UInt(u64::from(u8::from_le_bytes(bytes)))
            } else {
                Value::Int(i64::from(i8::from_le_bytes(bytes)))
            })
        }
        ColumnType::Short | ColumnType::Year => {
            let bytes = take_fixed::<2>(buf)?;
            Ok(if unsigned {
                Value::UInt(u64::from(u16::from_le_bytes(bytes)))
            } else {
                Value::Int(i64::from(i16::from_le_bytes(bytes)))
            })
        }
        ColumnType::Long | ColumnType::Int24 => {
            let bytes = take_fixed::<4>(buf)?;
            Ok(if unsigned {
                Value::UInt(u64::from(u32::from_le_bytes(bytes)))
            } else {
                Value::Int(i64::from(i32::from_le_bytes(bytes)))
            })
        }
        ColumnType::LongLong => {
            let bytes = take_fixed::<8>(buf)?;
            Ok(if unsigned {
                Value::UInt(u64::from_le_bytes(bytes))
            } else {
                Value::Int(i64::from_le_bytes(bytes))
            })
        }
        ColumnType::Float => {
            let bytes = take_fixed::<4>(buf)?;
            Ok(Value::Float(f32::from_le_bytes(bytes)))
        }
        ColumnType::Double => {
            let bytes = take_fixed::<8>(buf)?;
            Ok(Value::Double(f64::from_le_bytes(bytes)))
        }
        ColumnType::Date | ColumnType::Datetime | ColumnType::Timestamp => decode_date(buf),
        ColumnType::Time => decode_time(buf),

        // Lenenc-bytes types: strings, blobs, decimals, json, bit, geometry,
        // enum, set. `Value::Bytes` round-trips through `as_sql` as a quoted
        // string for textual payloads and as `x'..'` hex for non-UTF8 binary;
        // both are valid in COM_QUERY.
        ColumnType::Decimal
        | ColumnType::NewDecimal
        | ColumnType::VarChar
        | ColumnType::VarString
        | ColumnType::String
        | ColumnType::Blob
        | ColumnType::TinyBlob
        | ColumnType::MediumBlob
        | ColumnType::LongBlob
        | ColumnType::Json
        | ColumnType::Bit
        | ColumnType::Geometry
        | ColumnType::Enum
        | ColumnType::Set => {
            let bytes = take_lenenc_bytes(buf)?;
            Ok(Value::Bytes(bytes.to_vec()))
        }

        ColumnType::Null => Ok(Value::NULL),
    }
}

fn decode_date(buf: &mut &[u8]) -> Result<Value, Error> {
    let len = take_u8(buf)?;
    match len {
        0 => Ok(Value::Date(0, 0, 0, 0, 0, 0, 0)),
        4 | 7 | 11 => {
            let payload = take_n(buf, usize::from(len))?;
            let year = u16::from_le_bytes([payload[0], payload[1]]);
            let month = payload[2];
            let day = payload[3];
            let (hour, minute, second) = if len >= 7 {
                (payload[4], payload[5], payload[6])
            } else {
                (0, 0, 0)
            };
            let micros = if len == 11 {
                u32::from_le_bytes([payload[7], payload[8], payload[9], payload[10]])
            } else {
                0
            };
            Ok(Value::Date(year, month, day, hour, minute, second, micros))
        }
        other => Err(err_protocol!(
            "interpolation: unexpected DATE/DATETIME length {other}"
        )),
    }
}

fn decode_time(buf: &mut &[u8]) -> Result<Value, Error> {
    let len = take_u8(buf)?;
    match len {
        0 => Ok(Value::Time(false, 0, 0, 0, 0, 0)),
        8 | 12 => {
            let payload = take_n(buf, usize::from(len))?;
            let is_neg = payload[0] != 0;
            let days = u32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]);
            let hours = payload[5];
            let minutes = payload[6];
            let seconds = payload[7];
            let micros = if len == 12 {
                u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]])
            } else {
                0
            };
            Ok(Value::Time(is_neg, days, hours, minutes, seconds, micros))
        }
        other => Err(err_protocol!(
            "interpolation: unexpected TIME length {other}"
        )),
    }
}

fn take_u8(buf: &mut &[u8]) -> Result<u8, Error> {
    let (head, tail) = buf
        .split_first()
        .ok_or_else(|| err_protocol!("interpolation: unexpected end of argument buffer"))?;
    *buf = tail;
    Ok(*head)
}

fn take_fixed<const N: usize>(buf: &mut &[u8]) -> Result<[u8; N], Error> {
    if buf.len() < N {
        return Err(err_protocol!(
            "interpolation: need {N} bytes, have {}",
            buf.len()
        ));
    }
    let (head, tail) = buf.split_at(N);
    let mut out = [0u8; N];
    out.copy_from_slice(head);
    *buf = tail;
    Ok(out)
}

fn take_n<'a>(buf: &mut &'a [u8], n: usize) -> Result<&'a [u8], Error> {
    if buf.len() < n {
        return Err(err_protocol!(
            "interpolation: need {n} bytes, have {}",
            buf.len()
        ));
    }
    let (head, tail) = buf.split_at(n);
    *buf = tail;
    Ok(head)
}

fn take_lenenc_bytes<'a>(buf: &mut &'a [u8]) -> Result<&'a [u8], Error> {
    let len = take_lenenc_int(buf)?;
    let len = usize::try_from(len)
        .map_err(|_| err_protocol!("interpolation: lenenc length overflows usize: {len}"))?;
    take_n(buf, len)
}

fn take_lenenc_int(buf: &mut &[u8]) -> Result<u64, Error> {
    let first = take_u8(buf)?;
    match first {
        0xfc => Ok(u64::from(u16::from_le_bytes(take_fixed::<2>(buf)?))),
        0xfd => {
            let b = take_fixed::<3>(buf)?;
            Ok(u64::from(u32::from_le_bytes([b[0], b[1], b[2], 0])))
        }
        0xfe => Ok(u64::from_le_bytes(take_fixed::<8>(buf)?)),
        // 0xfb / 0xff aren't valid as encoded parameter lengths. Bind values
        // shouldn't produce them; treat as a single-byte length.
        v => Ok(u64::from(v)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arguments::MySqlArguments;

    fn args_with<F: FnOnce(&mut MySqlArguments)>(f: F) -> MySqlArguments {
        let mut a = MySqlArguments::default();
        f(&mut a);
        a
    }

    #[test]
    fn splice_basic_int_string() {
        let args = args_with(|a| {
            a.add(42i32).unwrap();
            a.add("o'reilly").unwrap();
        });
        let out = interpolate("SELECT ?, ?", &args, false).unwrap();
        assert_eq!(out, "SELECT 42, 'o\\'reilly'");
    }

    #[test]
    fn splice_no_backslash_escape() {
        let args = args_with(|a| {
            a.add("o'reilly").unwrap();
        });
        let out = interpolate("SELECT ?", &args, true).unwrap();
        assert_eq!(out, "SELECT 'o''reilly'");
    }

    #[test]
    fn splice_skips_question_mark_in_string_literal() {
        let args = args_with(|a| {
            a.add(1i32).unwrap();
        });
        let out = interpolate("SELECT '?', ?", &args, false).unwrap();
        assert_eq!(out, "SELECT '?', 1");
    }

    #[test]
    fn splice_skips_question_mark_in_line_comment() {
        let args = args_with(|a| {
            a.add(1i32).unwrap();
        });
        let out = interpolate("SELECT 1 -- ?\n, ?", &args, false).unwrap();
        assert_eq!(out, "SELECT 1 -- ?\n, 1");
    }

    #[test]
    fn splice_skips_question_mark_in_block_comment() {
        let args = args_with(|a| {
            a.add(1i32).unwrap();
        });
        let out = interpolate("SELECT /* ? */ ?", &args, false).unwrap();
        assert_eq!(out, "SELECT /* ? */ 1");
    }

    #[test]
    fn splice_preserves_multibyte_utf8() {
        let args = args_with(|a| {
            a.add(7i32).unwrap();
        });
        // The crab emoji is 4 bytes in UTF-8; ensure it round-trips.
        let out = interpolate("SELECT '🦀', ?", &args, false).unwrap();
        assert_eq!(out, "SELECT '🦀', 7");
    }

    #[test]
    fn splice_arity_mismatch_too_few_placeholders() {
        let args = args_with(|a| {
            a.add(1i32).unwrap();
            a.add(2i32).unwrap();
        });
        assert!(interpolate("SELECT ?", &args, false).is_err());
    }

    #[test]
    fn splice_arity_mismatch_too_many_placeholders() {
        let args = args_with(|a| {
            a.add(1i32).unwrap();
        });
        assert!(interpolate("SELECT ?, ?", &args, false).is_err());
    }

    #[test]
    fn splice_null_argument() {
        let args = args_with(|a| {
            a.add(Option::<i32>::None).unwrap();
        });
        let out = interpolate("SELECT ?", &args, false).unwrap();
        assert_eq!(out, "SELECT NULL");
    }

    #[test]
    fn splice_doubled_quote_inside_string() {
        let args = args_with(|a| {
            a.add(1i32).unwrap();
        });
        // The inner `''` is an escaped single quote; the `?` between them is
        // still inside the string literal.
        let out = interpolate("SELECT 'a''?b', ?", &args, false).unwrap();
        assert_eq!(out, "SELECT 'a''?b', 1");
    }

    #[test]
    fn splice_unsigned_int() {
        let args = args_with(|a| {
            a.add(u32::MAX).unwrap();
        });
        let out = interpolate("SELECT ?", &args, false).unwrap();
        assert_eq!(out, "SELECT 4294967295");
    }

    #[test]
    fn splice_negative_int() {
        let args = args_with(|a| {
            a.add(-7i64).unwrap();
        });
        let out = interpolate("SELECT ?", &args, false).unwrap();
        assert_eq!(out, "SELECT -7");
    }
}
