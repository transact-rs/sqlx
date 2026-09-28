use std::collections::VecDeque;
use std::ops::{ControlFlow, Deref, DerefMut};

use bytes::{Bytes, BytesMut};

use crate::error::Error;
use crate::io::MySqlBufExt;
use crate::io::{ProtocolDecode, ProtocolEncode};
use crate::net::{BufferedSocket, Socket};
use crate::protocol::response::{EofPacket, ErrPacket, OkPacket, Status};
use crate::protocol::statement::{PrepareOk, StmtClose};
use crate::protocol::{Capabilities, Packet};
use crate::{MySqlConnectOptions, MySqlDatabaseError};

pub struct MySqlStream<S = Box<dyn Socket>> {
    // Wrapping the socket in `Box` allows us to unsize in-place.
    pub(crate) socket: BufferedSocket<S>,
    pub(crate) server_version: (u16, u16, u16),
    pub(super) capabilities: Capabilities,
    pub(crate) sequence_id: u8,
    pub(crate) waiting: VecDeque<Waiting>,
    // statements a dropped operation left open; closed by the drain
    pub(crate) close_pending: Vec<u32>,
    pub(crate) is_tls: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Waiting {
    // waiting for a result set
    Result,

    // waiting for a row within a result set
    Row,

    // waiting for (the rest of) a COM_STMT_PREPARE response
    Prepare(PrepareProgress),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PrepareProgress {
    // the stmt-prepare-ok header has not been read yet
    Header,

    // the header was read; this many definition and EOF packets follow
    Tail {
        statement_id: u32,
        packets_left: u32,
    },
}

impl<S: Socket> MySqlStream<S> {
    pub(crate) fn with_socket(options: &MySqlConnectOptions, socket: S) -> Self {
        let mut capabilities = Capabilities::PROTOCOL_41
            | Capabilities::IGNORE_SPACE
            | Capabilities::DEPRECATE_EOF
            | Capabilities::TRANSACTIONS
            | Capabilities::SECURE_CONNECTION
            | Capabilities::PLUGIN_AUTH_LENENC_DATA
            | Capabilities::MULTI_STATEMENTS
            | Capabilities::MULTI_RESULTS
            | Capabilities::PLUGIN_AUTH
            | Capabilities::PS_MULTI_RESULTS
            | Capabilities::SSL;

        if options.database.is_some() {
            capabilities |= Capabilities::CONNECT_WITH_DB;
        }

        if options.found_rows {
            capabilities |= Capabilities::FOUND_ROWS;
        }

        Self {
            waiting: VecDeque::new(),
            close_pending: Vec::new(),
            capabilities,
            server_version: (0, 0, 0),
            sequence_id: 0,
            socket: BufferedSocket::new(socket),
            is_tls: false,
        }
    }

    pub(crate) async fn wait_until_ready(&mut self) -> Result<(), Error> {
        // Close the statements a dropped operation left open.
        for statement in std::mem::take(&mut self.close_pending) {
            self.sequence_id = 0;
            self.write_packet(StmtClose { statement })?;
        }

        if !self.socket.write_buffer().is_empty() {
            self.socket.flush().await?;
        }

        while !self.waiting.is_empty() {
            while matches!(self.waiting.front(), Some(Waiting::Prepare(_))) {
                // The rest of a response a cancelled prepare left behind.
                let (_, prepared) = self.recv_packet_tracked().await?;

                // Close the statement in the poll that completed it, so a
                // cancellation cannot lose the id.
                if let Some(statement) = prepared {
                    self.sequence_id = 0;
                    self.write_packet(StmtClose { statement })?;
                    self.socket.flush().await?;
                }
            }

            while self.waiting.front() == Some(&Waiting::Row) {
                let packet = self.recv_packet().await?;

                if !packet.is_empty() && packet[0] == 0xfe && packet.len() < 9 {
                    let eof = packet.eof(self.capabilities)?;

                    if eof.status.contains(Status::SERVER_MORE_RESULTS_EXISTS) {
                        *self.waiting.front_mut().unwrap() = Waiting::Result;
                    } else {
                        self.waiting.pop_front();
                    };
                }
            }

            while self.waiting.front() == Some(&Waiting::Result) {
                let packet = self.recv_packet().await?;

                if !packet.is_empty() && (packet[0] == 0x00 || packet[0] == 0xff) {
                    let ok = packet.ok()?;

                    if !ok.status.contains(Status::SERVER_MORE_RESULTS_EXISTS) {
                        self.waiting.pop_front();
                    }
                } else {
                    *self.waiting.front_mut().unwrap() = Waiting::Row;
                    self.skip_result_metadata(packet).await?;
                }
            }
        }

        Ok(())
    }

    pub(crate) async fn send_packet<'en, T>(&mut self, payload: T) -> Result<(), Error>
    where
        T: ProtocolEncode<'en, Capabilities>,
    {
        self.sequence_id = 0;
        self.write_packet(payload)?;
        self.flush().await?;
        Ok(())
    }

    pub(crate) fn write_packet<'en, T>(&mut self, payload: T) -> Result<(), Error>
    where
        T: ProtocolEncode<'en, Capabilities>,
    {
        self.socket
            .write_with(Packet(payload), (self.capabilities, &mut self.sequence_id))
    }

    async fn recv_packet_part(&mut self) -> Result<Bytes, Error> {
        // https://dev.mysql.com/doc/dev/mysql-server/8.0.12/page_protocol_basic_packets.html
        // https://mariadb.com/kb/en/library/0-packet/#standard-packet

        // One `try_read` takes header and payload, so a cancelled read leaves
        // the stream at a part boundary. Payloads of 0xFFFFFF bytes or more
        // span several parts.
        const HEADER_LEN: usize = 4;

        let (sequence_id, payload) = self
            .socket
            .try_read(|buf| {
                if buf.len() < HEADER_LEN {
                    return Ok(ControlFlow::Continue(HEADER_LEN));
                }

                // cannot overflow
                #[allow(clippy::cast_possible_truncation)]
                let packet_size = u32::from_le_bytes([buf[0], buf[1], buf[2], 0]) as usize;

                let frame_len = HEADER_LEN + packet_size;

                if buf.len() < frame_len {
                    return Ok(ControlFlow::Continue(frame_len));
                }

                let mut frame = buf.split_to(frame_len);
                let sequence_id = frame[3];
                let payload = frame.split_off(HEADER_LEN).freeze();

                Ok(ControlFlow::Break((sequence_id, payload)))
            })
            .await?;

        self.sequence_id = sequence_id.wrapping_add(1);

        // TODO: packet compression

        Ok(payload)
    }

    // receive the next packet from the database server
    // may block (async) on more data from the server
    pub(crate) async fn recv_packet(&mut self) -> Result<Packet<Bytes>, Error> {
        let (packet, _) = self.recv_packet_tracked().await?;

        Ok(packet)
    }

    /// Like `recv_packet`, plus the statement id of a COM_STMT_PREPARE
    /// response this packet completed.
    async fn recv_packet_tracked(&mut self) -> Result<(Packet<Bytes>, Option<u32>), Error> {
        let payload = self.recv_packet_part().await?;
        let payload = if payload.len() < 0xFF_FF_FF {
            payload
        } else {
            let mut final_payload = BytesMut::with_capacity(0xFF_FF_FF * 2);
            final_payload.extend_from_slice(&payload);

            drop(payload); // we don't need the allocation anymore

            let mut last_read = 0xFF_FF_FF;
            while last_read == 0xFF_FF_FF {
                let part = self.recv_packet_part().await?;
                last_read = part.len();
                final_payload.extend_from_slice(&part);
            }
            final_payload.into()
        };

        if payload
            .first()
            .ok_or(err_protocol!("Packet empty"))?
            .eq(&0xff)
        {
            self.waiting.pop_front();

            // instead of letting this packet be looked at everywhere, we check here
            // and emit a proper Error
            return Err(
                MySqlDatabaseError(ErrPacket::decode_with(payload, self.capabilities)?).into(),
            );
        }

        let prepared = self.note_prepare_response_packet(&payload)?;

        Ok((Packet(payload), prepared))
    }

    /// Counts the packets of a pending COM_STMT_PREPARE response and returns
    /// its statement id once complete. Only stmt-prepare-ok says how many
    /// packets follow, so every read path counts here.
    fn note_prepare_response_packet(&mut self, payload: &Bytes) -> Result<Option<u32>, Error> {
        let capabilities = self.capabilities;

        let Some(Waiting::Prepare(progress)) = self.waiting.front_mut() else {
            return Ok(None);
        };

        let (statement_id, packets_left) = match *progress {
            PrepareProgress::Header => {
                let ok = PrepareOk::decode_with(payload.clone(), capabilities)?;

                let eof_packets = if capabilities.contains(Capabilities::DEPRECATE_EOF) {
                    0
                } else {
                    u32::from(ok.params > 0) + u32::from(ok.columns > 0)
                };

                let packets_left = u32::from(ok.params) + u32::from(ok.columns) + eof_packets;

                (ok.statement_id, packets_left)
            }
            PrepareProgress::Tail {
                statement_id,
                packets_left,
            } => (statement_id, packets_left - 1),
        };

        if packets_left == 0 {
            self.waiting.pop_front();

            return Ok(Some(statement_id));
        }

        *progress = PrepareProgress::Tail {
            statement_id,
            packets_left,
        };

        Ok(None)
    }

    pub(crate) async fn recv<'de, T>(&mut self) -> Result<T, Error>
    where
        T: ProtocolDecode<'de, Capabilities>,
    {
        self.recv_packet().await?.decode_with(self.capabilities)
    }

    pub(crate) async fn recv_ok(&mut self) -> Result<OkPacket, Error> {
        self.recv_packet().await?.ok()
    }

    pub(crate) async fn maybe_recv_eof(&mut self) -> Result<Option<EofPacket>, Error> {
        if self.capabilities.contains(Capabilities::DEPRECATE_EOF) {
            Ok(None)
        } else {
            self.recv().await.map(Some)
        }
    }

    async fn skip_result_metadata(&mut self, mut packet: Packet<Bytes>) -> Result<(), Error> {
        let num_columns: u64 = packet.get_uint_lenenc()?; // column count

        for _ in 0..num_columns {
            let _ = self.recv_packet().await?;
        }

        self.maybe_recv_eof().await?;

        Ok(())
    }

    pub fn boxed_socket(self) -> MySqlStream {
        MySqlStream {
            socket: self.socket.boxed(),
            server_version: self.server_version,
            capabilities: self.capabilities,
            sequence_id: self.sequence_id,
            waiting: self.waiting,
            close_pending: self.close_pending,
            is_tls: self.is_tls,
        }
    }
}

impl<S> Deref for MySqlStream<S> {
    type Target = BufferedSocket<S>;

    fn deref(&self) -> &Self::Target {
        &self.socket
    }
}

impl<S> DerefMut for MySqlStream<S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.socket
    }
}
