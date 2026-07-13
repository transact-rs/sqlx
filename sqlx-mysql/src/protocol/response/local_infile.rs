use bytes::{Buf, Bytes};
use sqlx_core::io::{BufExt, ProtocolDecode};

use crate::error::Error;

/// Requests the client to send a file to the server, following a LOCAL INFILE statement
///
/// https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_com_query_response_local_infile_request.html
#[derive(Debug)]
pub struct LocalInfilePacket {
    pub filename: Vec<u8>,
}

impl ProtocolDecode<'_> for LocalInfilePacket {
    fn decode_with(mut buf: Bytes, _: ()) -> Result<Self, Error> {
        let header = buf.get_u8();
        if header != 0xfb {
            return Err(err_protocol!(
                "expected 0xfb (LocalInfileRequest) but found 0x{:02x}",
                header
            ));
        }

        let filename = buf.get_bytes(buf.len()).to_vec();

        Ok(Self { filename })
    }
}

#[test]
fn test_decode_localinfile_packet() {
    const DATA: &[u8] = b"\xfb\x64\x75\x6d\x6d\x79";

    let p = LocalInfilePacket::decode(DATA.into()).unwrap();

    assert_eq!(p.filename, b"dummy");
}
