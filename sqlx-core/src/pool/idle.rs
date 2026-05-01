use crate::connection::Connection;
use crate::database::Database;
use crate::pool::connection::{Floating, Idle, ConnectionInner};
use crate::pool::inner::PoolInner;
use crossbeam_queue::ArrayQueue;
use event_listener::Event;
use futures_util::FutureExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use event_listener::listener;

pub struct IdleQueue<DB: Database> {
    queue: ArrayQueue<Idle<DB>>,
    // Keep a separate count because `ArrayQueue::len()` loops until the head and tail pointers
    // stop changing, which may never happen at high contention.
    len: AtomicUsize,
    release_event: Event,
    fair: bool,
}

impl<DB: Database> IdleQueue<DB> {
    pub fn new(fair: bool, cap: usize) -> Self {
        Self {
            queue: ArrayQueue::new(cap),
            len: AtomicUsize::new(0),
            release_event: Event::new(),
            fair,
        }
    }

    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    pub async fn acquire(&self, pool: &Arc<PoolInner<DB>>) -> Floating<DB, Idle<DB>> {
        let mut should_wait = self.fair && self.release_event.total_listeners() > 0;

        for attempt in 1usize.. {
            if should_wait {
                listener!(self.release_event => release_event);
                release_event.await;
            }

            if let Some(conn) = self.try_acquire(pool) {
                return conn;
            }

            should_wait = true;

            if attempt == 2 {
                tracing::warn!(
                    "unable to acquire a connection after sleeping; this may indicate a bug"
                );
            }
        }

        panic!("BUG: was never able to acquire a connection despite waking many times")
    }

    pub fn try_acquire(&self, pool: &Arc<PoolInner<DB>>) -> Option<Floating<DB, Idle<DB>>> {
        self.len
            .fetch_update(Ordering::Release, Ordering::Acquire, |len| {
                len.checked_sub(1)
            })
            .ok()
            .and_then(|_| {
                let conn = self.queue.pop()?;

                Some(Floating::from_idle(conn, Arc::clone(pool)))
            })
    }

    pub fn release(&self, conn: Floating<DB, ConnectionInner<DB>>) {
        let Floating {
            inner: conn,
            permit,
        } = conn.into_idle();

        self.queue
            .push(conn)
            .unwrap_or_else(|_| panic!("BUG: idle queue capacity exceeded"));

        self.len.fetch_add(1, Ordering::Release);

        self.release_event.notify(1usize);

        // Don't decrease the size.
        permit.consume();
    }

    pub fn drain(&self, pool: &PoolInner<DB>) {
        while let Some(conn) = self.queue.pop() {
            // Hopefully will send at least a TCP FIN packet.
            conn.live.raw.close_hard().now_or_never();

            pool.counter.release_permit(pool);
        }
    }
}
