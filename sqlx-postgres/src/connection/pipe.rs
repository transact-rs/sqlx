use futures_channel::mpsc::UnboundedReceiver;
use futures_util::StreamExt;
use sqlx_core::Error;

use crate::{
    message::{BackendMessage, BackendMessageFormat, ReadyForQuery, ReceivedMessage},
    PgDatabaseError,
};

/// A temporary stream of responses sent from the background worker. The steam is stopped when
/// either a [ReadyForQuery] of [CopyInResponse] is received.
pub struct Pipe {
    receiver: UnboundedReceiver<ReceivedMessage>,
}

impl Pipe {
    pub fn new(receiver: UnboundedReceiver<ReceivedMessage>) -> Pipe {
        Self { receiver }
    }

    pub(crate) async fn recv_expect<B: BackendMessage>(&mut self) -> Result<B, Error> {
        self.recv().await?.decode()
    }

    pub async fn recv_ready_for_query(&mut self) -> Result<(), Error> {
        // The transaction status is updated in the bg worker.
        let _: ReadyForQuery = self.recv_expect().await?;
        Ok(())
    }

    pub(crate) async fn wait_ready_for_query(&mut self) -> Result<(), Error> {
        loop {
            let message = self.recv().await?;

            if let BackendMessageFormat::ReadyForQuery = message.format {
                // The transaction status is updated in the bg worker.
                break;
            }
        }

        Ok(())
    }

    // wait for CloseComplete to indicate a statement was closed
    pub async fn wait_for_close_complete(&mut self, mut count: usize) -> Result<(), Error> {
        // we need to wait for the [CloseComplete] to be returned from the server
        while count > 0 {
            match self.recv().await? {
                message if message.format == BackendMessageFormat::PortalSuspended => {
                    // there was an open portal
                    // this can happen if the last time a statement was used it was not fully executed
                }

                message if message.format == BackendMessageFormat::CloseComplete => {
                    // successfully closed the statement (and freed up the server resources)
                    count -= 1;
                }

                message => {
                    return Err(err_protocol!(
                        "expecting PortalSuspended or CloseComplete but received {:?}",
                        message.format
                    ));
                }
            }
        }

        Ok(())
    }

    pub(crate) async fn recv(&mut self) -> Result<ReceivedMessage, Error> {
        let message = self
            .receiver
            .next()
            .await
            .ok_or_else(|| sqlx_core::Error::WorkerCrashed)?;

        if message.format == BackendMessageFormat::ErrorResponse {
            Err(message.decode::<PgDatabaseError>()?.into())
        } else {
            Ok(message)
        }
    }
}
