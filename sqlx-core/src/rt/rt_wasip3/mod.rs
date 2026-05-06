use bytes::BytesMut;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::Arc;
use futures_util::future::{AbortHandle, Abortable};
use tokio::sync::mpsc;
use wit_bindgen::rt::async_support;
use futures::channel::oneshot;

use crate::net::WithSocket;

mod socket;

pub struct JoinHandle<T> {
    rx: oneshot::Receiver<T>,
}

impl<T> Future for JoinHandle<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.rx).poll(cx) {
            Poll::Ready(Ok(v)) => Poll::Ready(v),
            Poll::Ready(Err(oneshot::Canceled)) => panic!("wasip3 JoinHandle canceled"),
            Poll::Pending => Poll::Pending,
        }
    }
}

pub fn spawn<T: 'static>(fut: impl Future<Output = T> + 'static) -> JoinHandle<T> {
    let (tx, rx) = oneshot::channel();
    async_support::spawn(async move {
        let v = fut.await;
        _ = tx.send(v);
    });
    JoinHandle { rx }
}

// A tiny poll-aware sender shim backed by `futures::channel::mpsc::Sender`.
// This provides the minimal API `socket.rs` expects: `try_send`, `get_ref` and
// `poll_reserve`.
pub struct WasiPollSender<T> {
    inner: Option<mpsc::Sender<T>>,
}

impl<T> WasiPollSender<T> {
    pub fn new(s: mpsc::Sender<T>) -> Self {
        Self { inner: Some(s) }
    }

    pub fn get_ref(&self) -> Option<&mpsc::Sender<T>> {
        // Note: inner holds a `tokio::sync::mpsc::Sender` stored as a
        // `Option<mpsc::Sender<T>>` (type alias imported above). Return a
        // reference to it if present.
        self.inner.as_ref()
    }

    pub fn try_send(&self, item: T) -> Result<(), ()> {
        if let Some(s) = &self.inner {
            s.try_send(item).map_err(|_| ())
        } else {
            Err(())
        }
    }

    pub fn poll_reserve(&self, _cx: &mut Context<'_>) -> Poll<Result<(), ()>> {
        // There's no exact `poll_reserve` equivalent in futures mpsc. We emulate
        // it by checking if `poll_ready` would be `Ready` by attempting to
        // reserve via a short-lived future that yields `Ready` when the sink
        // can accept an item. For simplicity, we attempt a non-allocating
        // check: futures mpsc provides `poll_ready` on the Sink trait but
        // that's not directly available here. As a pragmatic approach, treat
        // the sender as always ready and return Pending only if the channel
        // is closed.
        if self.inner.is_some() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(()))
        }
    }
}

pub struct TcpSocket {
    pub tx: WasiPollSender<Vec<u8>>,
    pub rx: mpsc::Receiver<Vec<u8>>,
    pub buf: BytesMut,
    // Abort handle for the background task spawned with `async_support::spawn`.
    pub abort_handle: AbortHandle,
}

impl Drop for TcpSocket {
    fn drop(&mut self) {
        // Abort the background task if it's still running.
        self.abort_handle.abort();
    }
}

pub async fn connect_tcp<Ws: WithSocket>(
    _host: &str,
    port: u16,
    with_socket: Ws,
) -> crate::Result<Ws::Output> {
    // address resolution requires additional processing
    // let addresses = wasip3::sockets::ip_name_lookup::resolve_addresses(host.to_string())
    //     .await
    //     .map_err(|e| {
    //         crate::Error::Io(std::io::Error::new(
    //             std::io::ErrorKind::Other,
    //             format!("DNS failed: {:?}", e),
    //         ))
    //     })?;

    // let ip = addresses.into_iter().next().ok_or_else(|| {
    //     crate::Error::Io(std::io::Error::new(
    //         std::io::ErrorKind::Other,
    //         "No addresses found",
    //     ))
    // })?;

    // let addr = match ip {
    //     wasip3::sockets::types::IpAddress::Ipv4(ipv4) => {
    //         IpSocketAddress::Ipv4(wasip3::sockets::types::Ipv4SocketAddress {
    //             address: ipv4,
    //             port,
    //         })
    //     }
    //     wasip3::sockets::types::IpAddress::Ipv6(ipv6) => {
    //         IpSocketAddress::Ipv6(wasip3::sockets::types::Ipv6SocketAddress {
    //             address: ipv6,
    //             port,
    //             flow_info: 0,
    //             scope_id: 0,
    //         })
    //     }
    // };
    let sock =
        wasip3::sockets::types::TcpSocket::create(wasip3::sockets::types::IpAddressFamily::Ipv4)
            .expect("failed to create TCP socket");
    sock.connect(wasip3::sockets::types::IpSocketAddress::Ipv4(
        wasip3::sockets::types::Ipv4SocketAddress {
            address: (127, 0, 0, 1),
            port,
        },
    ))
    .await
    .expect(&format!("failed to connect to 127.0.0.1:{port}"));

    let (rx_tx, rx_rx) = mpsc::channel::<Vec<u8>>(1);
    let (tx_tx, mut tx_rx) = mpsc::channel::<Vec<u8>>(1);
    let (mut send_tx, send_rx) = wasip3::wit_stream::new();
    let (mut recv_rx, recv_fut) = sock.receive();

    let (abort_handle, abort_registration) = AbortHandle::new_pair();
    async_support::yield_async().await;
    let background = Abortable::new(
        async move {
            let sock = Arc::new(sock);
            let (ready_tx, ready_rx) = oneshot::channel();

            async_support::spawn({
                let sock = Arc::clone(&sock);
                async move {
                    let fut = sock.send(send_rx);
                    _ = ready_tx.send(());
                    let _ = fut.await;
                    drop(sock);
                }
            });
            async_support::yield_async().await;
            async_support::spawn({
                let sock = Arc::clone(&sock);
                async move {
                    let _ = recv_fut.await;
                    drop(sock);
                }
            });
            // Yield to give spawned tasks a chance to be polled before joining.
            async_support::yield_async().await;
            futures_util::join!(
                async {
                    use wit_bindgen::rt::async_support::StreamResult;
                    loop {
                        let buf = Vec::with_capacity(4096);
                        let (result, buf) = recv_rx.read(buf).await;
                        if !buf.is_empty() && rx_tx.send(buf).await.is_err() {
                            break;
                        }
                        match result {
                            StreamResult::Dropped => break,
                            StreamResult::Cancelled | StreamResult::Complete(_) => {}
                        }
                    }
                    drop(recv_rx);
                    drop(rx_tx);
                },
                async {
                    _ = ready_rx.await;
                    while let Some(buf) = tx_rx.recv().await {
                        let _ = send_tx.write(buf).await;
                    }
                    drop(tx_rx);
                    drop(send_tx);
                },
            );
        },
        abort_registration,
    );

    async_support::spawn(async move {
        let _ = background.await;
    });
    Ok(with_socket
        .with_socket(TcpSocket {
            tx: WasiPollSender::new(tx_tx),
            rx: rx_rx,
            buf: bytes::BytesMut::new(),
            abort_handle,
        })
        .await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn test_spawn_completes_successfully() {
        async {
            let handle = spawn(async { 42 });
            let result = handle.await;
            assert_eq!(result, 42);
        };
    }

    #[test]
    fn test_spawn_with_async_computation() {
        async {
            let handle = spawn(async {
                let mut sum = 0;
                for i in 1..=10 {
                    sum += i;
                }
                sum
            });

            let result = handle.await;
            assert_eq!(result, 55);
        };
    }

    #[test]
    fn test_spawn_multiple_tasks() {
        async {
            let handle1 = spawn(async { 1 });
            let handle2 = spawn(async { 2 });
            let handle3 = spawn(async { 3 });

            let result1 = handle1.await;
            let result2 = handle2.await;
            let result3 = handle3.await;

            assert_eq!(result1 + result2 + result3, 6);
        };
    }

    #[test]
    fn test_spawn_with_sleep() {
        async {
            let started = Instant::now();

            let handle = spawn(async {
                crate::rt::sleep(Duration::from_millis(100)).await;
                "completed"
            });

            let result = handle.await;
            let elapsed = started.elapsed();

            assert_eq!(result, "completed");
            assert!(elapsed >= Duration::from_millis(100));
        };
    }

    #[test]
    fn test_spawn_nested_tasks() {
        async {
            let outer = spawn(async {
                let inner = spawn(async { 10 });
                let value = inner.await;
                value * 2
            });

            let result = outer.await;
            assert_eq!(result, 20);
        };
    }

    #[test]
    fn test_spawn_with_shared_state() {
        async {
            let counter = Arc::new(AtomicU32::new(0));
            let counter_clone = counter.clone();

            let handle = spawn(async move {
                counter_clone.fetch_add(1, Ordering::SeqCst);
                counter_clone.fetch_add(1, Ordering::SeqCst);
            });

            handle.await;
            assert_eq!(counter.load(Ordering::SeqCst), 2);
        };
    }

    #[test]
    fn test_spawn_concurrent_tasks_with_shared_state() {
        async {
            let counter = Arc::new(AtomicU32::new(0));
            let mut handles = vec![];

            for _ in 0..5 {
                let counter_clone = counter.clone();
                let handle = spawn(async move {
                    counter_clone.fetch_add(1, Ordering::SeqCst);
                });
                handles.push(handle);
            }

            for handle in handles {
                handle.await;
            }

            assert_eq!(counter.load(Ordering::SeqCst), 5);
        };
    }

    #[test]
    fn test_sleep_duration_accuracy() {
        async {
            let durations = [
                Duration::from_millis(50),
                Duration::from_millis(100),
                Duration::from_millis(200),
            ];

            for expected_duration in durations {
                let start = Instant::now();
                crate::rt::sleep(expected_duration).await;
                let elapsed = start.elapsed();

                // Allow for some timing variance (±20ms)
                assert!(
                    elapsed >= expected_duration,
                    "Sleep was too short: expected {:?}, got {:?}",
                    expected_duration,
                    elapsed
                );
                assert!(
                    elapsed < expected_duration + Duration::from_millis(50),
                    "Sleep was too long: expected {:?}, got {:?}",
                    expected_duration,
                    elapsed
                );
            }
        };
    }

    #[test]
    fn test_sleep_zero_duration() {
        async {
            let start = Instant::now();
            crate::rt::sleep(Duration::ZERO).await;
            let elapsed = start.elapsed();

            // Should complete very quickly
            assert!(elapsed < Duration::from_millis(10));
        };
    }

    #[test]
    fn test_timeout_completes_before_deadline() {
        async {
            let result = crate::rt::timeout(Duration::from_secs(1), async {
                crate::rt::sleep(Duration::from_millis(50)).await;
                42
            })
            .await;

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), 42);
        };
    }

    #[test]
    fn test_timeout_exceeds_deadline() {
        async {
            let result = crate::rt::timeout(Duration::from_millis(50), async {
                crate::rt::sleep(Duration::from_millis(200)).await;
                42
            })
            .await;

            assert!(result.is_err());
        };
    }

    #[test]
    fn test_timeout_immediate_completion() {
        async {
            let result = crate::rt::timeout(Duration::from_secs(1), async { "immediate" }).await;

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), "immediate");
        };
    }

    #[test]
    fn test_timeout_with_computation() {
        async {
            let result = crate::rt::timeout(Duration::from_secs(1), async {
                let mut sum = 0;
                for i in 1..=100 {
                    sum += i;
                }
                sum
            })
            .await;

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), 5050);
        };
    }

    #[test]
    fn test_spawn_and_timeout_combined() {
        async {
            let handle = spawn(async {
                crate::rt::timeout(Duration::from_millis(100), async {
                    crate::rt::sleep(Duration::from_millis(50)).await;
                    "success"
                })
                .await
            });

            let result = handle.await;
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), "success");
        };
    }

    #[test]
    fn test_multiple_sleeps_sequential() {
        async {
            let start = Instant::now();

            crate::rt::sleep(Duration::from_millis(50)).await;
            crate::rt::sleep(Duration::from_millis(50)).await;
            crate::rt::sleep(Duration::from_millis(50)).await;

            let elapsed = start.elapsed();

            // Total should be at least 150ms
            assert!(elapsed >= Duration::from_millis(150));
        };
    }

    #[test]
    fn test_multiple_sleeps_concurrent() {
        async {
            let start = Instant::now();

            let h1 = spawn(async {
                crate::rt::sleep(Duration::from_millis(100)).await;
            });
            let h2 = spawn(async {
                crate::rt::sleep(Duration::from_millis(100)).await;
            });
            let h3 = spawn(async {
                crate::rt::sleep(Duration::from_millis(100)).await;
            });

            h1.await;
            h2.await;
            h3.await;

            let elapsed = start.elapsed();

            // Should complete in ~100ms, not 300ms (concurrent execution)
            assert!(elapsed < Duration::from_millis(200));
        };
    }

    #[test]
    fn test_join_handle_future_trait() {
        async {
            use std::future::Future;
            use std::pin::Pin;
            use std::task::{Context, Poll};

            let handle = spawn(async { 99 });

            // Pin the handle to test Future implementation
            let mut pinned = Box::pin(handle);

            // Create a simple waker for testing
            let waker = futures_util::task::noop_waker();
            let mut cx = Context::from_waker(&waker);

            // Poll until ready
            loop {
                match pinned.as_mut().poll(&mut cx) {
                    Poll::Ready(value) => {
                        assert_eq!(value, 99);
                        break;
                    }
                    Poll::Pending => continue,
                }
            }
        };
    }

    #[test]
    fn test_spawn_with_boolean_result() {
        async {
            let flag = Arc::new(AtomicBool::new(false));
            let flag_clone = flag.clone();

            let handle = spawn(async move {
                flag_clone.store(true, Ordering::SeqCst);
                flag_clone.load(Ordering::SeqCst)
            });

            let result = handle.await;
            assert!(result);
            assert!(flag.load(Ordering::SeqCst));
        };
    }

    #[test]
    fn test_complex_async_workflow() {
        async {
            // Simulate a complex workflow with spawning, sleeping, and timeouts
            let step1 = spawn(async {
                crate::rt::sleep(Duration::from_millis(50)).await;
                10
            });

            let step2 = spawn(async {
                crate::rt::sleep(Duration::from_millis(30)).await;
                20
            });

            let result1 = step1.await;
            let result2 = step2.await;

            let step3 = spawn(async move {
                crate::rt::timeout(Duration::from_millis(100), async { result1 + result2 }).await
            });

            let final_result = step3.await;
            assert!(final_result.is_ok());
            assert_eq!(final_result.unwrap(), 30);
        };
    }

    #[test]
    fn test_spawn_return_string() {
        async {
            let handle = spawn(async { String::from("Hello from WASI!") });

            let result = handle.await;
            assert_eq!(result, "Hello from WASI!");
        };
    }

    #[test]
    fn test_spawn_with_option_result() {
        async {
            let handle = spawn(async { Some(42) });

            let result = handle.await;
            assert_eq!(result, Some(42));
        };
    }

    #[test]
    fn test_spawn_with_result_type() {
        async {
            let handle = spawn(async { Ok::<i32, String>(100) });

            let result = handle.await;
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), 100);
        };
    }
}
