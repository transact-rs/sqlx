use bytes::buf::Chain;
use bytes::{Buf, Bytes};
use std::cmp;

use crate::error::Error;
use crate::io::{BufExt, ProtocolDecode};
use crate::protocol::auth::AuthPlugin;
use crate::protocol::response::Status;
use crate::protocol::Capabilities;

// https://dev.mysql.com/doc/internals/en/connection-phase-packets.html#packet-Protocol::Handshake
// https://mariadb.com/kb/en/connection/#initial-handshake-packet

#[derive(Debug)]
pub(crate) struct Handshake {
    #[allow(unused)]
    pub(crate) protocol_version: u8,
    pub(crate) server_version: String,
    #[allow(unused)]
    pub(crate) connection_id: u32,
    pub(crate) server_capabilities: Capabilities,
    #[allow(unused)]
    pub(crate) server_default_collation: u8,
    #[allow(unused)]
    pub(crate) status: Status,
    pub(crate) auth_plugin: Option<AuthPlugin>,
    pub(crate) auth_plugin_data: Chain<Bytes, Bytes>,
}

impl ProtocolDecode<'_> for Handshake {
    fn decode_with(mut buf: Bytes, _: ()) -> Result<Self, Error> {
        // NOTE: the handshake packet is received before authentication, so a
        // hostile server can send anything here; every fixed-width read is
        // validated against the remaining buffer to avoid panicking on
        // truncated packets
        if buf.is_empty() {
            return Err(err_protocol!(
                "expected protocol version (1 byte) in Handshake packet but got 0 bytes"
            ));
        }

        let protocol_version = buf.get_u8(); // int<1>
        let server_version = buf.get_str_nul()?; // string<NUL>

        if buf.remaining() < 13 {
            return Err(err_protocol!(
                "expected connection id (4 bytes), auth-plugin-data-part-1 (8 bytes) and reserved byte (13 bytes total) in Handshake packet but got {} bytes",
                buf.remaining()
            ));
        }

        let connection_id = buf.get_u32_le(); // int<4>
        let auth_plugin_data_1 = buf.get_bytes(8); // string<8>

        buf.advance(1); // reserved: string<1>

        if buf.remaining() < 2 {
            return Err(err_protocol!(
                "expected capability flags part 1 (2 bytes) in Handshake packet but got {} bytes",
                buf.remaining()
            ));
        }

        let capabilities_1 = buf.get_u16_le(); // int<2>
        let mut capabilities = Capabilities::from_bits_truncate(capabilities_1.into());

        if buf.remaining() < 5 {
            return Err(err_protocol!(
                "expected collation (1 byte), status (2 bytes) and capability flags part 2 (2 bytes) in Handshake packet but got {} bytes",
                buf.remaining()
            ));
        }

        let collation = buf.get_u8(); // int<1>
        let status = Status::from_bits_truncate(buf.get_u16_le());

        let capabilities_2 = buf.get_u16_le(); // int<2>
        capabilities |= Capabilities::from_bits_truncate(((capabilities_2 as u32) << 16).into());

        if buf.is_empty() {
            return Err(err_protocol!(
                "expected auth-plugin-data length (1 byte) in Handshake packet but got 0 bytes"
            ));
        }

        let auth_plugin_data_len = if capabilities.contains(Capabilities::PLUGIN_AUTH) {
            buf.get_u8()
        } else {
            buf.advance(1); // int<1>
            0
        };

        if buf.remaining() < 10 {
            return Err(err_protocol!(
                "expected reserved bytes (6 bytes) and reserved bytes or capability flags part 3 (4 bytes) in Handshake packet but got {} bytes",
                buf.remaining()
            ));
        }

        buf.advance(6); // reserved: string<6>

        if capabilities.contains(Capabilities::MYSQL) {
            buf.advance(4); // reserved: string<4>
        } else {
            let capabilities_3 = buf.get_u32_le(); // int<4>
            capabilities |= Capabilities::from_bits_truncate((capabilities_3 as u64) << 32);
        }

        let auth_plugin_data_2 = if capabilities.contains(Capabilities::SECURE_CONNECTION) {
            let len = cmp::max(auth_plugin_data_len.saturating_sub(9), 12) as usize;

            // the attacker-controlled length must fit in the remaining buffer
            // *with* the trailing NUL-terminator, i.e. `len + 1` bytes
            if buf.remaining() <= len {
                return Err(err_protocol!(
                    "expected auth-plugin-data-part-2 ({len} bytes) and NUL-terminator in Handshake packet but got {} bytes",
                    buf.remaining()
                ));
            }

            let v = buf.get_bytes(len);
            buf.advance(1); // NUL-terminator

            v
        } else {
            Bytes::new()
        };

        let auth_plugin = if capabilities.contains(Capabilities::PLUGIN_AUTH) {
            Some(buf.get_str_nul()?.parse()?)
        } else {
            None
        };

        Ok(Self {
            protocol_version,
            server_version,
            connection_id,
            server_default_collation: collation,
            status,
            server_capabilities: capabilities,
            auth_plugin,
            auth_plugin_data: auth_plugin_data_1.chain(auth_plugin_data_2),
        })
    }
}

#[test]
fn test_decode_handshake_mysql_8_0_18() {
    const HANDSHAKE_MYSQL_8_0_18: &[u8] = b"\n8.0.18\x00\x19\x00\x00\x00\x114aB0c\x06g\x00\xff\xff\xff\x02\x00\xff\xc7\x15\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00tL\x03s\x0f[4\rl4. \x00caching_sha2_password\x00";

    let p = Handshake::decode(HANDSHAKE_MYSQL_8_0_18.into()).unwrap();

    assert_eq!(p.protocol_version, 10);

    assert_eq!(
        p.server_capabilities,
        Capabilities::MYSQL
            | Capabilities::FOUND_ROWS
            | Capabilities::LONG_FLAG
            | Capabilities::CONNECT_WITH_DB
            | Capabilities::NO_SCHEMA
            | Capabilities::COMPRESS
            | Capabilities::ODBC
            | Capabilities::LOCAL_FILES
            | Capabilities::IGNORE_SPACE
            | Capabilities::PROTOCOL_41
            | Capabilities::INTERACTIVE
            | Capabilities::SSL
            | Capabilities::TRANSACTIONS
            | Capabilities::SECURE_CONNECTION
            | Capabilities::MULTI_STATEMENTS
            | Capabilities::MULTI_RESULTS
            | Capabilities::PS_MULTI_RESULTS
            | Capabilities::PLUGIN_AUTH
            | Capabilities::CONNECT_ATTRS
            | Capabilities::PLUGIN_AUTH_LENENC_DATA
            | Capabilities::CAN_HANDLE_EXPIRED_PASSWORDS
            | Capabilities::SESSION_TRACK
            | Capabilities::DEPRECATE_EOF
            | Capabilities::ZSTD_COMPRESSION_ALGORITHM
            | Capabilities::SSL_VERIFY_SERVER_CERT
            | Capabilities::OPTIONAL_RESULTSET_METADATA
            | Capabilities::REMEMBER_OPTIONS,
    );

    assert_eq!(p.server_default_collation, 255);
    assert!(p.status.contains(Status::SERVER_STATUS_AUTOCOMMIT));

    assert!(matches!(
        p.auth_plugin,
        Some(AuthPlugin::CachingSha2Password)
    ));

    assert_eq!(
        &*p.auth_plugin_data.into_iter().collect::<Vec<_>>(),
        &[17, 52, 97, 66, 48, 99, 6, 103, 116, 76, 3, 115, 15, 91, 52, 13, 108, 52, 46, 32,]
    );
}

#[test]
fn test_decode_handshake_mariadb_10_4_7() {
    const HANDSHAKE_MARIA_DB_10_4_7: &[u8] = b"\n5.5.5-10.4.7-MariaDB-1:10.4.7+maria~bionic\x00\x0b\x00\x00\x00t6L\\j\"dS\x00\xfe\xf7\x08\x02\x00\xff\x81\x15\x00\x00\x00\x00\x00\x00\x07\x00\x00\x00U14Oph9\"<H5n\x00mysql_native_password\x00";

    let p = Handshake::decode(HANDSHAKE_MARIA_DB_10_4_7.into()).unwrap();

    assert_eq!(p.protocol_version, 10);

    assert_eq!(
        &*p.server_version,
        "5.5.5-10.4.7-MariaDB-1:10.4.7+maria~bionic"
    );

    assert_eq!(
        p.server_capabilities,
        Capabilities::FOUND_ROWS
            | Capabilities::LONG_FLAG
            | Capabilities::CONNECT_WITH_DB
            | Capabilities::NO_SCHEMA
            | Capabilities::COMPRESS
            | Capabilities::ODBC
            | Capabilities::LOCAL_FILES
            | Capabilities::IGNORE_SPACE
            | Capabilities::PROTOCOL_41
            | Capabilities::INTERACTIVE
            | Capabilities::TRANSACTIONS
            | Capabilities::SECURE_CONNECTION
            | Capabilities::MULTI_STATEMENTS
            | Capabilities::MULTI_RESULTS
            | Capabilities::PS_MULTI_RESULTS
            | Capabilities::PLUGIN_AUTH
            | Capabilities::CONNECT_ATTRS
            | Capabilities::PLUGIN_AUTH_LENENC_DATA
            | Capabilities::CAN_HANDLE_EXPIRED_PASSWORDS
            | Capabilities::SESSION_TRACK
            | Capabilities::DEPRECATE_EOF
            | Capabilities::REMEMBER_OPTIONS
            | Capabilities::MARIADB_CLIENT_PROGRESS
            | Capabilities::MARIADB_CLIENT_MULTI
            | Capabilities::MARIADB_CLIENT_STMT_BULK_OPERATIONS
    );

    assert_eq!(p.server_default_collation, 8);
    assert!(p.status.contains(Status::SERVER_STATUS_AUTOCOMMIT));
    assert!(matches!(
        p.auth_plugin,
        Some(AuthPlugin::MySqlNativePassword)
    ));

    assert_eq!(
        &*p.auth_plugin_data.into_iter().collect::<Vec<_>>(),
        &[116, 54, 76, 92, 106, 34, 100, 83, 85, 49, 52, 79, 112, 104, 57, 34, 60, 72, 53, 110,]
    );
}

#[test]
fn test_decode_handshake_truncated_never_panics() {
    const MYSQL_8_0_18: &[u8] = b"\n8.0.18\x00\x19\x00\x00\x00\x114aB0c\x06g\x00\xff\xff\xff\x02\x00\xff\xc7\x15\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00tL\x03s\x0f[4\rl4. \x00caching_sha2_password\x00";
    const MARIA_DB_10_4_7: &[u8] = b"\n5.5.5-10.4.7-MariaDB-1:10.4.7+maria~bionic\x00\x0b\x00\x00\x00t6L\\j\"dS\x00\xfe\xf7\x08\x02\x00\xff\x81\x15\x00\x00\x00\x00\x00\x00\x07\x00\x00\x00U14Oph9\"<H5n\x00mysql_native_password\x00";

    for packet in [MYSQL_8_0_18, MARIA_DB_10_4_7] {
        // every strict prefix of a valid handshake is an error, never a panic
        for len in 0..packet.len() {
            assert!(
                Handshake::decode(packet[..len].into()).is_err(),
                "expected error for handshake truncated to {len} bytes"
            );
        }

        // and the full packet still decodes
        assert!(Handshake::decode(packet.into()).is_ok());
    }
}

#[test]
fn test_decode_handshake_rejects_oversized_auth_plugin_data_len() {
    // 0xc7ff capability flags part 2 sets SECURE_CONNECTION and PLUGIN_AUTH,
    // so the auth-plugin-data length byte governs the part-2 read; it claims
    // 255 bytes (246 after subtracting 9) but only a few bytes follow
    let mut packet = Vec::new();
    packet.push(10_u8); // protocol version
    packet.extend_from_slice(b"8.0.0\0"); // server version
    packet.extend_from_slice(&1_u32.to_le_bytes()); // connection id
    packet.extend_from_slice(b"01234567"); // auth-plugin-data-part-1
    packet.push(0); // reserved
    packet.extend_from_slice(&0xffff_u16.to_le_bytes()); // capability flags part 1
    packet.push(0); // collation
    packet.extend_from_slice(&2_u16.to_le_bytes()); // status
    packet.extend_from_slice(&0xc7ff_u16.to_le_bytes()); // capability flags part 2
    packet.push(255); // auth-plugin-data length
    packet.extend_from_slice(&[0; 6]); // reserved
    packet.extend_from_slice(&[0; 4]); // reserved
    packet.extend_from_slice(b"short-junk\0");

    let err = Handshake::decode(packet.into()).unwrap_err();

    assert!(err.to_string().contains("auth-plugin-data-part-2"), "{err}");
}
