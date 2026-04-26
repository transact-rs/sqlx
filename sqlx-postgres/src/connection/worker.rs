use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    ops::ControlFlow,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    task::{ready, Context, Poll},
};

use crate::{
    message::{
        BackendMessageFormat, FrontendMessage, Notice, Notification, ParameterStatus,
        ReadyForQuery, ReceivedMessage, Terminate, TransactionStatus,
    },
    PgConnectOptions,
};
use futures_channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures_util::{SinkExt, StreamExt};
use sqlx_core::{
    bytes::Buf,
    net::{self, BufferedSocket, Socket},
    rt::spawn,
    Result,
};

use super::{request::IoRequest, tls::MaybeUpgradeTls};

#[derive(PartialEq, Debug)]
enum WorkerState {
    // The connection is open and ready for requests.
    Open,
    // Responding to the last messages but not receiving new ones. After handling the last message
    // a [Terminate] message is issued.
    Closing,
    // Last messages are handled, [Terminate] message is sent and the session is closed. Nog try
    // and close the socket.
    Closed,
}

pub struct Worker {
    state: WorkerState,
    should_flush: bool,
    chan: UnboundedReceiver<IoRequest>,
    back_log: VecDeque<UnboundedSender<ReceivedMessage>>,
    socket: BufferedSocket<Box<dyn Socket>>,
    notif_chan: UnboundedSender<Notification>,
    shared: Shared,
}

impl Worker {
    pub(super) async fn connect(
        options: &PgConnectOptions,
        notif_chan: UnboundedSender<Notification>,
        shared: Shared,
    ) -> crate::Result<UnboundedSender<IoRequest>> {
        let socket_result = match options.fetch_socket() {
            Some(ref path) => net::connect_uds(path, MaybeUpgradeTls(options)).await?,
            None => net::connect_tcp(&options.host, options.port, MaybeUpgradeTls(options)).await?,
        };

        let socket = BufferedSocket::new(socket_result?);

        Ok(Worker::spawn(socket, notif_chan, shared))
    }

    pub fn spawn(
        socket: BufferedSocket<Box<dyn Socket>>,
        notif_chan: UnboundedSender<Notification>,
        shared: Shared,
    ) -> UnboundedSender<IoRequest> {
        let (tx, rx) = unbounded();

        let worker = Worker {
            state: WorkerState::Open,
            should_flush: false,
            chan: rx,
            back_log: VecDeque::new(),
            socket,
            notif_chan,
            shared: shared.clone(),
        };

        spawn(worker);
        tx
    }

    // Tries to receive the next message from the channel. Also handles termination if needed.
    #[inline(always)]
    fn poll_next_request(&mut self, cx: &mut Context<'_>) -> Poll<IoRequest> {
        match self.chan.poll_next_unpin(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(request)) => Poll::Ready(request),
            Poll::Ready(None) => {
                // Channel was closed, explicitly or because the sender was dropped. Either way
                // we should start a graceful shutdown.
                self.state = WorkerState::Closing;
                Poll::Pending
            }
        }
    }

    #[inline(always)]
    fn poll_receiver(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        // Only try and receive io requests if we're open.
        if self.state != WorkerState::Open {
            return Poll::Ready(Ok(()));
        }

        loop {
            ready!(self.socket.poll_ready_unpin(cx))?;

            let request = ready!(self.poll_next_request(cx));

            self.socket.start_send_unpin(&request.data)?;
            self.should_flush = true;

            if let Some(chan) = request.chan {
                // We should send the responses back
                self.back_log.push_back(chan);
            }
        }
    }

    #[inline(always)]
    fn handle_poll_flush(&mut self, cx: &mut Context<'_>) -> Result<()> {
        if self.should_flush && self.socket.poll_flush_unpin(cx).is_ready() {
            self.should_flush = false;
        }
        Ok(())
    }

    #[inline(always)]
    fn send_back(&mut self, response: ReceivedMessage) -> Result<()> {
        if let Some(chan) = self.back_log.front_mut() {
            let _ = chan.unbounded_send(response);
            Ok(())
        } else {
            Err(err_protocol!("Received response but did not expect one."))
        }
    }

    #[inline(always)]
    fn poll_backlog(&mut self, cx: &mut Context<'_>) -> Result<()> {
        while let Poll::Ready(response) = self.poll_next_message(cx)? {
            match response.format {
                BackendMessageFormat::ReadyForQuery => {
                    // Cloning a `ReceivedMessage` here is cheap because it only clones the
                    // underlying `Bytes`
                    let rfq: ReadyForQuery = response.clone().decode()?;
                    self.shared.set_transaction_status(rfq.transaction_status);

                    self.send_back(response)?;
                    // Remove from the backlog so we dont send more responses back.
                    let _ = self.back_log.pop_front();
                }
                BackendMessageFormat::CopyInResponse => {
                    // End of response
                    self.send_back(response)?;
                    // Remove from the backlog so we dont send more responses back.
                    let _ = self.back_log.pop_front();
                }
                BackendMessageFormat::NotificationResponse => {
                    // Notification
                    let notif: Notification = response.decode()?;
                    let _ = self.notif_chan.unbounded_send(notif);
                }
                BackendMessageFormat::ParameterStatus => {
                    // Asynchronous response
                    let ParameterStatus { name, value } = response.decode()?;
                    self.shared.insert_parameter_status(name, value);
                }
                BackendMessageFormat::NoticeResponse => {
                    // do we need this to be more configurable?
                    // if you are reading this comment and think so, open an issue

                    let notice: Notice = response.decode()?;

                    notice.emit_notice();
                }
                _ => self.send_back(response)?,
            }
        }
        Ok(())
    }

    #[inline(always)]
    fn poll_next_message(&mut self, cx: &mut Context<'_>) -> Poll<Result<ReceivedMessage>> {
        if self.state == WorkerState::Closed {
            // We're still responsing to the last messages, only after clearing the backlog we
            // should stop reading.
            return Poll::Pending;
        }

        self.socket.poll_try_read(cx, |buf| {
            // all packets in postgres start with a 5-byte header
            // this header contains the message type and the total length of the message
            let Some(mut header) = buf.get(..5) else {
                return Ok(ControlFlow::Continue(5));
            };

            let format = BackendMessageFormat::try_from_u8(header.get_u8())?;

            let message_len = header.get_u32() as usize;

            let expected_len = message_len
                .checked_add(1)
                // this shouldn't really happen but is mostly a sanity check
                .ok_or_else(|| err_protocol!("message_len + 1 overflows usize: {message_len}"))?;

            if buf.len() < expected_len {
                return Ok(ControlFlow::Continue(expected_len));
            }

            // `buf` SHOULD NOT be modified ABOVE this line

            // pop off the format code since it's not counted in `message_len`
            buf.advance(1);

            // consume the message, including the length prefix
            let mut contents = buf.split_to(message_len).freeze();

            // cut off the length prefix
            contents.advance(4);

            Ok(ControlFlow::Break(ReceivedMessage { format, contents }))
        })
    }

    #[inline(always)]
    fn poll_shutdown(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        match self.state {
            // After responding to the last messages we can issue a [Terminate] request and
            // close the connection.
            WorkerState::Closing if self.back_log.is_empty() => {
                let terminate = [Terminate::FORMAT as u8, 0, 0, 0, 4];
                self.socket.write_buffer_mut().put_slice(&terminate);
                self.state = WorkerState::Closed;

                // Closing the socket also flushes the buffer.
                self.socket.poll_close_unpin(cx)
            }
            // The channel is closed, all requests are flushed and a [Terminate] message has been
            // sent, now try and close the socket
            WorkerState::Closed => self.socket.poll_close_unpin(cx),
            WorkerState::Open | WorkerState::Closing => Poll::Pending,
        }
    }

    fn poll_worker(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        // Try to receive responses from the database and handle them.
        self.poll_backlog(cx)?;

        // Push as many new requests in the write buffer as we can.
        if let Poll::Ready(Err(e)) = self.poll_receiver(cx) {
            return Poll::Ready(Err(e));
        };

        // Flush the write buffer if needed.
        self.handle_poll_flush(cx)?;

        // Close this socket if we're done.
        self.poll_shutdown(cx)
    }
}

impl Future for Worker {
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.poll_worker(cx).map_err(|e| {
            tracing::error!("Background worker stopped with error: {e:?}");
            e
        })
    }
}

#[derive(Clone)]
pub struct Shared(Arc<Mutex<SharedInner>>);

pub struct SharedInner {
    pub parameter_statuses: BTreeMap<String, String>,
    pub transaction_status: TransactionStatus,
}

impl Shared {
    pub fn new() -> Shared {
        Shared(Arc::new(Mutex::new(SharedInner {
            parameter_statuses: BTreeMap::new(),
            transaction_status: TransactionStatus::Idle,
        })))
    }

    fn lock(&self) -> MutexGuard<'_, SharedInner> {
        self.0
            .lock()
            .expect("BUG: failed to get lock on shared state in worker")
    }

    pub fn get_transaction_status(&self) -> TransactionStatus {
        self.lock().transaction_status
    }

    fn set_transaction_status(&self, status: TransactionStatus) {
        self.lock().transaction_status = status
    }

    fn insert_parameter_status(&self, name: String, value: String) {
        self.lock().parameter_statuses.insert(name, value);
    }

    pub fn remove_parameter_status(&self, name: &str) -> Option<String> {
        self.lock().parameter_statuses.remove(name)
    }

    #[cfg(feature = "offline")]
    pub fn with_lock<T>(&self, f: impl Fn(&mut SharedInner) -> T) -> T {
        let mut lock = self.lock();
        f(&mut lock)
    }
}
