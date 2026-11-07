use sqlx_core::bytes::Bytes;

use crate::error::Error;
use crate::message::{BackendMessage, BackendMessageFormat};

/// The transaction status of a session, as the server reports it in every
/// `ReadyForQuery` message.
///
/// See [`PgConnection::transaction_status()`][crate::PgConnection::transaction_status].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PgTransactionStatus {
    /// Not in a transaction block.
    Idle = b'I',

    /// In a transaction block.
    Transaction = b'T',

    /// In a _failed_ transaction block. Queries will be rejected until block is ended.
    Error = b'E',
}

#[derive(Debug)]
pub struct ReadyForQuery {
    pub transaction_status: PgTransactionStatus,
}

impl BackendMessage for ReadyForQuery {
    const FORMAT: BackendMessageFormat = BackendMessageFormat::ReadyForQuery;

    fn decode_body(buf: Bytes) -> Result<Self, Error> {
        let status = match buf[0] {
            b'I' => PgTransactionStatus::Idle,
            b'T' => PgTransactionStatus::Transaction,
            b'E' => PgTransactionStatus::Error,

            status => {
                return Err(err_protocol!(
                    "unknown transaction status: {:?}",
                    status as char
                ));
            }
        };

        Ok(Self {
            transaction_status: status,
        })
    }
}

#[test]
fn test_decode_ready_for_query() -> Result<(), Error> {
    const DATA: &[u8] = b"E";

    let m = ReadyForQuery::decode_body(Bytes::from_static(DATA))?;

    assert!(matches!(m.transaction_status, PgTransactionStatus::Error));

    Ok(())
}
