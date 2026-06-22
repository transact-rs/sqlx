use bytes::{Buf, Bytes};

use crate::error::Error;
use crate::io::BufExt;

pub trait MySqlBufExt: Buf {
    // Read a length-encoded integer.
    // NOTE: 0xfb or NULL is only returned for binary value encoding to indicate NULL.
    // NOTE: 0xff is only returned during a result set to indicate ERR.
    // <https://dev.mysql.com/doc/internals/en/integer.html#packet-Protocol::LengthEncodedInteger>
    fn get_uint_lenenc(&mut self) -> Result<u64, Error>;

    // Read a length-encoded string.
    #[allow(dead_code)]
    fn get_str_lenenc(&mut self) -> Result<String, Error>;

    // Read a length-encoded byte sequence.
    fn get_bytes_lenenc(&mut self) -> Result<Bytes, Error>;
}

impl MySqlBufExt for Bytes {
    fn get_uint_lenenc(&mut self) -> Result<u64, Error> {
        if self.remaining() < 1 {
            return Err(err_protocol!("lenenc int: no bytes remaining"));
        }

        match self.get_u8() {
            0xfc => {
                if self.remaining() < 2 {
                    return Err(err_protocol!(
                        "lenenc int: need 2 more bytes, have {}",
                        self.remaining()
                    ));
                }
                Ok(u64::from(self.get_u16_le()))
            }
            0xfd => {
                if self.remaining() < 3 {
                    return Err(err_protocol!(
                        "lenenc int: need 3 more bytes, have {}",
                        self.remaining()
                    ));
                }
                Ok(self.get_uint_le(3))
            }
            0xfe => {
                if self.remaining() < 8 {
                    return Err(err_protocol!(
                        "lenenc int: need 8 more bytes, have {}",
                        self.remaining()
                    ));
                }
                Ok(self.get_u64_le())
            }

            v => Ok(u64::from(v)),
        }
    }

    fn get_str_lenenc(&mut self) -> Result<String, Error> {
        let size = self.get_uint_lenenc()?;
        let size = usize::try_from(size)
            .map_err(|_| err_protocol!("string length overflows usize: {size}"))?;

        self.get_str(size)
    }

    fn get_bytes_lenenc(&mut self) -> Result<Bytes, Error> {
        let size = self.get_uint_lenenc()?;
        let size = usize::try_from(size)
            .map_err(|_| err_protocol!("string length overflows usize: {size}"))?;

        Ok(self.split_to(size))
    }
}
