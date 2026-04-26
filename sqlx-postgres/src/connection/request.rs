use futures_channel::mpsc::UnboundedSender;
use sqlx_core::{io::ProtocolEncode, Error};

use crate::message::{self, EncodeMessage, FrontendMessage, ReceivedMessage};

/// A request for the background worker.
#[derive(Debug)]
pub struct IoRequest {
    pub chan: Option<UnboundedSender<ReceivedMessage>>,
    pub data: Vec<u8>,
}

/// A buffer that contains encoded postgres messages, ready to be sent over the wire.
pub struct MessageBuf {
    data: Vec<u8>,
}

impl MessageBuf {
    pub fn new() -> Self {
        Self { data: Vec::new() }
    }

    #[inline(always)]
    pub fn write<'en, T>(&mut self, value: T) -> sqlx_core::Result<()>
    where
        T: ProtocolEncode<'en, ()>,
    {
        value.encode(&mut self.data)
    }

    #[inline(always)]
    pub fn write_sync(&mut self) {
        self.write_msg(message::Sync)
            .expect("BUG: Sync should not be too big for protocol");
    }

    #[inline(always)]
    pub(crate) fn write_msg(&mut self, message: impl FrontendMessage) -> Result<(), Error> {
        self.write(EncodeMessage(message))
    }

    pub(crate) fn buf_mut(&mut self) -> &mut Vec<u8> {
        &mut self.data
    }

    pub fn finish(self) -> IoRequest {
        IoRequest {
            data: self.data,
            chan: None,
        }
    }
}
