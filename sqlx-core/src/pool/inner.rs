use super::connection::ConnectionInner;
use crate::database::Database;
use crate::error::Error;
use crate::pool::{connection, CloseEvent, Pool, PoolConnection, PoolConnector, PoolOptions};

use std::cmp;
use std::future::Future;
use std::ops::ControlFlow;
use std::pin::{pin, Pin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};

use crate::connection::Connection;
use crate::ext::future::race;
use crate::logger::private_level_filter_to_trace_level;
use crate::pool::connect::{ConnectTaskShared, ConnectionId, DynConnector};
use crate::pool::connection_set::{ConnectedSlot, ConnectionSet, DisconnectedSlot};
use crate::{private_tracing_dynamic_event, rt};
use event_listener::listener;
use futures_util::future::{self};
use std::time::{Duration, Instant};
use tracing::{Instrument, Level};

const TEST_BEFORE_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct PoolInner<DB: Database> {
    pub(super) connector: DynConnector<DB>,
    pub(super) connections: ConnectionSet<ConnectionInner<DB>>,
    is_closed: AtomicBool,
    pub(super) on_closed: event_listener::Event,
    pub(super) options: PoolOptions<DB>,
    pub(crate) acquire_time_level: Option<Level>,
    pub(crate) acquire_slow_level: Option<Level>,
}

impl<DB: Database> PoolInner<DB> {
    pub(super) fn new_arc(
        options: PoolOptions<DB>,
        connector: impl PoolConnector<DB>,
    ) -> Arc<Self> {
        let pool = Arc::new(Self {
            connector: DynConnector::new(connector),
            connections: ConnectionSet::new(options.min_connections..=options.max_connections),
            is_closed: AtomicBool::new(false),
            on_closed: event_listener::Event::new(),
            acquire_time_level: private_level_filter_to_trace_level(options.acquire_time_level),
            acquire_slow_level: private_level_filter_to_trace_level(options.acquire_slow_level),
            options,
        });

        spawn_maintenance_tasks(&pool);

        pool
    }

    pub(super) fn size(&self) -> usize {
        self.connections.num_connected()
    }

    pub(super) fn num_idle(&self) -> usize {
        self.connections.count_idle()
    }

    pub(super) fn is_closed(&self) -> bool {
        self.is_closed.load(Ordering::Acquire)
    }

    fn mark_closed(&self) {
        self.is_closed.store(true, Ordering::Release);
        self.on_closed.notify(usize::MAX);
    }

    pub(super) fn close(self: &Arc<Self>) -> impl Future<Output = ()> + '_ {
        self.mark_closed();

        // Keep clearing the idle queue as connections are released until the count reaches zero.
        self.connections.drain(async |slot| {
            let (_res, slot) = connection::close(slot).await;
            slot
        })
    }

    pub(crate) fn close_event(&self) -> CloseEvent {
        CloseEvent {
            listener: (!self.is_closed()).then(|| self.on_closed.listen()),
        }
    }

    #[inline]
    pub(super) fn try_acquire(self: &Arc<Self>) -> Option<ConnectedSlot<ConnectionInner<DB>>> {
        if self.is_closed() {
            return None;
        }

        self.connections.try_acquire_connected()
    }

    pub(super) async fn acquire(self: &Arc<Self>) -> Result<PoolConnection<DB>, Error> {
        if self.is_closed() {
            return Err(Error::PoolClosed);
        }

        let acquire_started_at = Instant::now();

        // Lazily allocated `Arc<ConnectTaskShared>`
        let mut connect_shared = None;

        let res = {
            // Pinned to the stack without allocating
            listener!(self.on_closed => close_listener);
            let mut deadline = pin!(rt::sleep(self.options.acquire_timeout));
            let mut acquire_inner = pin!(self.acquire_inner(&mut connect_shared));

            std::future::poll_fn(|cx| {
                if self.is_closed() {
                    return Poll::Ready(Err(Error::PoolClosed));
                }

                // The result doesn't matter so much as the wakeup
                let _ = Pin::new(&mut close_listener).poll(cx);

                if let Poll::Ready(()) = deadline.as_mut().poll(cx) {
                    return Poll::Ready(Err(Error::PoolTimedOut {
                        last_connect_error: None,
                    }));
                }

                acquire_inner.as_mut().poll(cx)
            })
            .await
        };

        let acquired = res.map_err(|e| match e {
            Error::PoolTimedOut {
                last_connect_error: None,
            } => Error::PoolTimedOut {
                last_connect_error: connect_shared
                    .and_then(|shared| Some(shared.take_error()?.into())),
            },
            e => e,
        })?;

        let acquired_after = acquire_started_at.elapsed();

        let acquire_slow_level = self
            .acquire_slow_level
            .filter(|_| acquired_after > self.options.acquire_slow_threshold);

        if let Some(level) = acquire_slow_level {
            private_tracing_dynamic_event!(
                target: "sqlx::pool::acquire",
                level,
                acquired_after_secs = acquired_after.as_secs_f64(),
                slow_acquire_threshold_secs = self.options.acquire_slow_threshold.as_secs_f64(),
                "acquired connection, but time to acquire exceeded slow threshold"
            );
        } else if let Some(level) = self.acquire_time_level {
            private_tracing_dynamic_event!(
                target: "sqlx::pool::acquire",
                level,
                acquired_after_secs = acquired_after.as_secs_f64(),
                "acquired connection"
            );
        }

        Ok(acquired)
    }

    async fn acquire_inner(
        self: &Arc<Self>,
        connect_shared: &mut Option<Arc<ConnectTaskShared>>,
    ) -> Result<PoolConnection<DB>, Error> {
        tracing::trace!("waiting for any connection");

        let disconnected = match self.connections.acquire_any().await {
            Ok(conn) => match self.finish_acquire(conn).await {
                Ok(conn) => return Ok(conn),
                Err(slot) => slot,
            },
            Err(slot) => slot,
        };

        let mut connect_task = self.connector.connect(
            Pool(self.clone()),
            ConnectionId::next(),
            disconnected,
            connect_shared.insert(ConnectTaskShared::new_arc()).clone(),
        );

        loop {
            match race(&mut connect_task, self.connections.acquire_connected()).await {
                Ok(Ok(conn)) => return Ok(conn),
                Ok(Err(e)) => return Err(e),
                Err(conn) => match self.finish_acquire(conn).await {
                    Ok(conn) => return Ok(conn),
                    Err(_) => continue,
                },
            }
        }
    }

    #[inline(always)]
    async fn finish_acquire(
        self: &Arc<Self>,
        conn: ConnectedSlot<ConnectionInner<DB>>,
    ) -> Result<PoolConnection<DB>, DisconnectedSlot<ConnectionInner<DB>>> {
        let span =
            tracing::trace_span!(target: "sqlx::pool", "finish_acquire", connection_id=?conn.id);

        finish_acquire(self, conn).instrument(span).await
    }

    pub(crate) async fn try_min_connections(
        self: &Arc<Self>,
        deadline: Option<Instant>,
    ) -> Result<(), Error> {
        let shared = ConnectTaskShared::new_arc();

        let connect_min_connections = future::try_join_all(
            (self.connections.num_connected()..self.options.min_connections)
                .filter_map(|_| self.connections.try_acquire_disconnected())
                .map(|slot| {
                    self.connector.connect(
                        Pool(self.clone()),
                        ConnectionId::next(),
                        slot,
                        shared.clone(),
                    )
                }),
        );

        let conns = if let Some(deadline) = deadline {
            match rt::timeout_at(deadline, connect_min_connections).await {
                Ok(Ok(conns)) => conns,
                Err(_) | Ok(Err(Error::PoolTimedOut { .. })) => {
                    return Err(Error::PoolTimedOut {
                        last_connect_error: shared.take_error().map(Box::new),
                    });
                }
                Ok(Err(e)) => return Err(e),
            }
        } else {
            connect_min_connections.await?
        };

        for mut conn in conns {
            // Bypass `after_release`
            drop(conn.return_to_pool());
        }

        Ok(())
    }
}

impl<DB: Database> Drop for PoolInner<DB> {
    fn drop(&mut self) {
        self.mark_closed();
    }
}

/// Execute `test_before_acquire` and/or `before_acquire` in a background task, if applicable.
///
/// Otherwise, immediately returns the connection.
async fn finish_acquire<DB: Database>(
    pool: &Arc<PoolInner<DB>>,
    mut conn: ConnectedSlot<ConnectionInner<DB>>,
) -> Result<PoolConnection<DB>, DisconnectedSlot<ConnectionInner<DB>>> {
    struct SpawnOnDrop<F: Future + Send + 'static>(Option<Pin<Box<F>>>)
    where
        F::Output: Send + 'static;

    impl<F: Future + Send + 'static> Future for SpawnOnDrop<F>
    where
        F::Output: Send + 'static,
    {
        type Output = F::Output;

        #[inline(always)]
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            // Essentially an inlined `Fuse`
            let Some(mut fut) = self.0.take() else {
                return Poll::Pending;
            };

            if let Poll::Ready(res) = fut.as_mut().poll(cx) {
                return Poll::Ready(res);
            }

            self.0 = Some(fut);
            Poll::Pending
        }
    }

    impl<F: Future + Send + 'static> Drop for SpawnOnDrop<F>
    where
        F::Output: Send + 'static,
    {
        fn drop(&mut self) {
            if let Some(fut) = self.0.take() {
                rt::try_spawn(fut);
            }
        }
    }

    async fn finish_inner<DB: Database>(
        conn: &mut ConnectedSlot<ConnectionInner<DB>>,
        pool: &PoolInner<DB>,
    ) -> ControlFlow<()> {
        // Check that the connection is still live
        if pool.options.test_before_acquire {
            if let Err(error) = conn.raw.ping().await {
                // an error here means the other end has hung up or we lost connectivity
                // either way we're fine to just discard the connection
                // the error itself here isn't necessarily unexpected so WARN is too strong
                tracing::info!(%error, connection_id=%conn.id, "ping on idle connection returned error");
                return ControlFlow::Break(());
            }
        }

        if let Some(test) = &pool.options.before_acquire {
            let meta = conn.idle_metadata();
            match test(&mut conn.raw, meta).await {
                Ok(false) => {
                    // connection was rejected by user-defined hook, close nicely
                    tracing::debug!(connection_id=%conn.id, "connection rejected by `before_acquire`");
                    return ControlFlow::Break(());
                }

                Err(error) => {
                    tracing::warn!(%error, "error from `before_acquire`");
                    return ControlFlow::Break(());
                }

                Ok(true) => (),
            }
        }

        // Checks passed
        ControlFlow::Continue(())
    }

    if pool.options.test_before_acquire || pool.options.before_acquire.is_some() {
        let pool = pool.clone();

        // Spawn a task on-drop so the call may complete even if `acquire()` is cancelled.
        conn = SpawnOnDrop(Some(Box::pin(async move {
            match rt::timeout(TEST_BEFORE_ACQUIRE_TIMEOUT, finish_inner(&mut conn, &pool)).await {
                Ok(ControlFlow::Continue(())) => {
                    Ok(conn)
                }
                Ok(ControlFlow::Break(())) => {
                    // Connection rejected by user-defined hook, attempt to close nicely
                    let (_res, slot) = connection::close(conn).await;
                    Err(slot)
                }
                Err(_) => {
                    tracing::info!(connection_id=%conn.id, "`before_acquire` checks timed out, closing connection");
                    let (_res, slot) = connection::close_hard(conn).await;
                    Err(slot)
                }
            }
        }))).await?;
    }

    tracing::debug!(
        target: "sqlx::pool",
        connection_id=%conn.id,
        "acquired idle connection"
    );

    Ok(PoolConnection::new(conn))
}

fn spawn_maintenance_tasks<DB: Database>(pool: &Arc<PoolInner<DB>>) {
    if pool.options.min_connections > 0 {
        // NOTE: use `pool_weak` for the maintenance tasks
        // so they don't keep `PoolInner` from being dropped.
        let pool_weak = Arc::downgrade(pool);
        let mut close_event = pool.close_event();

        rt::spawn(async move {
            close_event
                .do_until(check_min_connections(pool_weak))
                .await
                .ok();
        });
    }

    let check_interval = match (pool.options.max_lifetime, pool.options.idle_timeout) {
        (Some(it), None) | (None, Some(it)) => it,
        (Some(a), Some(b)) => cmp::min(a, b),
        (None, None) => return,
    };

    let pool_weak = Arc::downgrade(pool);
    let mut close_event = pool.close_event();

    rt::spawn(async move {
        let _ = close_event
            .do_until(check_idle_conns(pool_weak, check_interval))
            .await;
    });
}

async fn check_idle_conns<DB: Database>(pool_weak: Weak<PoolInner<DB>>, check_interval: Duration) {
    let mut interval = pin!(rt::interval_after(check_interval));

    while let Some(pool) = pool_weak.upgrade() {
        if pool.is_closed() {
            return;
        }

        // Go over all idle connections, check for idleness and lifetime,
        // and if we have fewer than min_connections after reaping a connection,
        // open a new one immediately.
        for conn in pool.connections.iter_idle() {
            if conn.is_beyond_idle_timeout(&pool.options)
                || conn.is_beyond_max_lifetime(&pool.options)
            {
                // Dropping the slot will check if the connection needs to be re-made.
                let _ = connection::close(conn).await;
            }
        }

        // Don't hold a reference to the pool while sleeping.
        drop(pool);

        interval.as_mut().tick().await;
    }
}

async fn check_min_connections<DB: Database>(pool_weak: Weak<PoolInner<DB>>) {
    while let Some(pool) = pool_weak.upgrade() {
        if pool.is_closed() {
            return;
        }

        match pool.try_min_connections(None).await {
            Ok(()) => {
                let listener = pool.connections.min_connections_listener();

                // Important: don't hold a strong ref while sleeping
                drop(pool);

                listener.await;
            }
            Err(e) => {
                tracing::warn!(
                    target: "sqlx::pool::maintenance",
                    min_connections=pool.options.min_connections,
                    num_connected=pool.connections.num_connected(),
                    "unable to maintain `min_connections`: {e:?}",
                );
            }
        }
    }
}
