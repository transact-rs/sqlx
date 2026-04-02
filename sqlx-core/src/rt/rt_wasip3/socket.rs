use core::task::{Context, Poll};

use bytes::BufMut as _;
use std::io;
use tokio::sync::mpsc::error::TryRecvError;

use crate::io::ReadBuf;
use crate::net::Socket;

impl Socket for super::TcpSocket {
    fn try_read(&mut self, buf: &mut dyn ReadBuf) -> io::Result<usize> {
        let n = buf.remaining_mut();

        // First, drain any buffered data
        if !self.buf.is_empty() {
            let to_copy = n.min(self.buf.len());
            buf.put_slice(&self.buf.split_to(to_copy));
            return Ok(to_copy);
        }

        // Try to receive new data
        match self.rx.try_recv() {
            Ok(rx_vec) => {
                if rx_vec.is_empty() {
                    return Err(io::ErrorKind::WouldBlock.into());
                }

                if rx_vec.len() <= n {
                    // All data fits in the buffer
                    buf.put_slice(&rx_vec);
                    Ok(rx_vec.len())
                } else {
                    // Data is larger than buffer, store remainder
                    buf.put_slice(&rx_vec[..n]);
                    self.buf.extend_from_slice(&rx_vec[n..]);
                    Ok(n)
                }
            }
            Err(TryRecvError::Empty) => Err(io::ErrorKind::WouldBlock.into()),
            Err(TryRecvError::Disconnected) => Ok(0),
        }
    }

    fn try_write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let n = buf.len();
        match self.tx.try_send(buf.to_vec()) {
            Ok(()) => Ok(n),
            Err(_) => Err(io::ErrorKind::WouldBlock.into()),
        }
    }

    fn poll_read_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // If we have buffered data, we're ready to read
        if !self.buf.is_empty() {
            return Poll::Ready(Ok(()));
        }

        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(v)) => {
                if !v.is_empty() {
                    self.buf.extend(v);
                    Poll::Ready(Ok(()))
                } else {
                    // Empty vec received, wait for more
                    Poll::Pending
                }
            }
            Poll::Ready(None) => {
                // Channel closed
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_write_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.tx.poll_reserve(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(())) => Poll::Ready(Err(io::ErrorKind::ConnectionReset.into())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_shutdown(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Drop the sender to signal shutdown
        // The abort_handle will be dropped when TcpSocket is dropped
        Poll::Ready(Ok(()))
    }
}
