use crate::net::Socket;

use std::future;
use std::io::{self, Read, Write};
use std::task::{Context, Poll};

pub struct StdSocket<S> {
    pub socket: S,
    wants_read: bool,
    wants_write: bool,
}

impl<S: Socket> StdSocket<S> {
    pub fn new(socket: S) -> Self {
        Self {
            socket,
            wants_read: false,
            wants_write: false,
        }
    }

    /// Returns `Ready` if a previously blocked read _or_ write may now proceed.
    ///
    /// If both a read and a write were attempted, to avoid deadlocks this returns `Ready`
    /// when _either_ direction is ready, not necessarily both.
    pub fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Return `Ready` without waiting if the caller hasn't tried to do I/O in either direction.
        let mut ready = !(self.wants_read || self.wants_write);

        if self.wants_write && self.socket.poll_write_ready(cx)?.is_ready() {
            self.wants_write = false;
            ready |= true;
        }

        if self.wants_read && self.socket.poll_read_ready(cx)?.is_ready() {
            self.wants_read = false;
            ready |= true;
        }

        if ready {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    /// Returns successfully if a previously blocked read _or_ write may now proceed.
    ///
    /// If both a read and a write were attempted, to avoid deadlocks this returns when _either_
    /// direction is ready, not necessarily both.
    pub async fn ready(&mut self) -> io::Result<()> {
        future::poll_fn(|cx| self.poll_ready(cx)).await
    }
}

impl<S: Socket> Read for StdSocket<S> {
    fn read(&mut self, mut buf: &mut [u8]) -> io::Result<usize> {
        self.wants_read = true;
        let read = self.socket.try_read(&mut buf)?;
        self.wants_read = false;

        Ok(read)
    }
}

impl<S: Socket> Write for StdSocket<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.wants_write = true;
        let written = self.socket.try_write(buf)?;
        self.wants_write = false;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        // NOTE: TCP sockets and unix sockets are both no-ops for flushes
        Ok(())
    }
}
