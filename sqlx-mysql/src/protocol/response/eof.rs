use bytes::{Buf, Bytes};

use crate::error::Error;
use crate::io::ProtocolDecode;
use crate::protocol::response::Status;
use crate::protocol::Capabilities;

/// Marks the end of a result set, returning status and warnings.
///
/// # Note
///
/// The EOF packet is deprecated as of MySQL 5.7.5. SQLx only uses this packet for MySQL
/// prior MySQL versions.
#[derive(Debug)]
pub struct EofPacket {
    #[allow(dead_code)]
    pub warnings: u16,
    pub status: Status,
}

impl ProtocolDecode<'_, Capabilities> for EofPacket {
    fn decode_with(mut buf: Bytes, _: Capabilities) -> Result<Self, Error> {
        let header = buf.get_u8();
        if header != 0xfe {
            return Err(err_protocol!(
                "expected 0xfe (EOF_Packet) but found 0x{:x}",
                header
            ));
        }

        if buf.remaining() < 4 {
            return Err(err_protocol!(
                "expected warnings (2 bytes) and status flags (2 bytes) in EOF_Packet but got {} bytes",
                buf.remaining()
            ));
        }

        let warnings = buf.get_u16_le();
        let status = Status::from_bits_truncate(buf.get_u16_le());

        Ok(Self { status, warnings })
    }
}

#[test]
fn test_decode_eof_packet() {
    let p = EofPacket::decode_with(
        b"\xfe\x00\x00\x02\x00".as_ref().into(),
        Capabilities::empty(),
    )
    .unwrap();

    assert_eq!(p.warnings, 0);
    assert!(p.status.contains(Status::SERVER_STATUS_AUTOCOMMIT));
}

#[test]
fn test_decode_eof_packet_truncated() {
    // only 2 bytes after the 0xfe header, need 4
    let p = EofPacket::decode_with(b"\xfe\x00\x00".as_ref().into(), Capabilities::empty());

    assert!(p.is_err());
}
