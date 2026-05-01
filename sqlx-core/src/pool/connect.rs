use crate::connection::{ConnectOptions, Connection};
use crate::database::Database;
use crate::pool::connection::ConnectionInner;
use crate::pool::{Pool, PoolConnection};
use crate::rt::JoinHandle;
use crate::{rt, Error};
use event_listener::{listener, Event};
use std::fmt::{Display, Formatter};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::pool::connection_set::DisconnectedSlot;
#[cfg(doc)]
use crate::pool::PoolOptions;
use ease_off::core::EaseOffCore;
use std::ops::ControlFlow;
use std::pin::{pin, Pin};
use std::task::{ready, Context, Poll};

const EASE_OFF: EaseOffCore = ease_off::Options::new().into_core();

/// Custom connect callback for [`Pool`][crate::pool::Pool].
///
/// Implemented for closures with the signature
/// `Fn(PoolConnectMetadata) -> impl Future<Output = sqlx::Result<impl Connection>>`.
///
/// See [`Self::connect()`] for details and implementation advice.
///
/// # Example: `after_connect` Replacement
/// The `after_connect` callback was removed in 0.9.0 as it was redundant to this API.
///
/// This example uses Postgres but may be adapted to any driver.
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use sqlx::PgConnection;
/// use sqlx::postgres::PgPoolOptions;
/// use sqlx::Connection;
/// use sqlx::pool::PoolConnectMetadata;
///
/// async fn _example() -> sqlx::Result<()> {
/// // `PoolConnector` is implemented for closures but this has restrictions on returning borrows
/// // due to current language limitations. Custom implementations are not subject to this.
/// //
/// // This example shows how to get around this using `Arc`.
/// let database_url: Arc<str> = "postgres://...".into();
///
/// let pool = PgPoolOptions::new()
///     .min_connections(5)
///     .max_connections(30)
///     // Type annotation on the argument is required for the trait impl to reseolve.
///     .connect_with_connector(move |meta: PoolConnectMetadata| {
///         let database_url = database_url.clone();
///         async move {
///             println!(
///                 "opening connection {}, attempt {}; elapsed time: {:?}",
///                 meta.pool_size,
///                 meta.num_attempts + 1,
///                 meta.start.elapsed()
///             );
///
///             let mut conn = PgConnection::connect(&database_url).await?;
///
///             // Override the time zone of the connection.
///             sqlx::raw_sql("SET TIME ZONE 'Europe/Berlin'")
///                 .execute(&mut conn)
///                 .await?;
///
///             Ok(conn)
///         }
///     })
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// # Example: `set_connect_options` Replacement
/// `set_connect_options` and `get_connect_options` were removed in 0.9.0 because they complicated
/// the pool internals. They can be reimplemented by capturing a mutex, or similar, in the callback.
///
/// This example uses Postgres and [`tokio::sync::RwLock`] but may be adapted to any driver
/// or `async-std`, respectively.
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use tokio::sync::RwLock;
/// use sqlx::PgConnection;
/// use sqlx::postgres::PgConnectOptions;
/// use sqlx::postgres::PgPoolOptions;
/// use sqlx::ConnectOptions;
/// use sqlx::pool::PoolConnectMetadata;
///
/// async fn _example() -> sqlx::Result<()> {
/// // If you do not wish to hold the lock during the connection attempt,
/// // you could use `Arc<PgConnectOptions>` instead.
/// let connect_opts: Arc<RwLock<PgConnectOptions>> = Arc::new(RwLock::new("postgres://...".parse()?));
/// // We need a copy that will be captured by the closure.
/// let connect_opts_ = connect_opts.clone();
///
/// let pool = PgPoolOptions::new()
///     .connect_with_connector(move |meta: PoolConnectMetadata| {
///         let connect_opts = connect_opts_.clone();
///         async move {
///             println!(
///                 "opening connection {}, attempt {}; elapsed time: {:?}",
///                 meta.pool_size,
///                 meta.num_attempts + 1,
///                 meta.start.elapsed()
///             );
///
///             connect_opts.read().await.connect().await
///         }
///     })
///     .await?;
///
/// // Close the connection that was previously opened by `connect_with_connector()`.
/// pool.acquire().await?.close().await?;
///
/// // Simulating a credential rotation
/// let mut write_connect_opts = connect_opts.write().await;
/// write_connect_opts
///     .set_username("new_username")
///     .set_password("new password");
///
/// // Should use the new credentials.
/// let mut conn = pool.acquire().await?;
///
/// # Ok(())
/// # }
/// ```
///
/// # Example: Custom Implementation
///
/// Custom implementations of `PoolConnector` trade a little bit of boilerplate for much
/// more flexibility. Thanks to the signature of `connect()`, they can return a `Future`
/// type that borrows from `self`.
///
/// This example uses Postgres but may be adapted to any driver.
///
/// ```rust,no_run
/// use sqlx::{PgConnection, Postgres};
/// use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
/// use sqlx_core::connection::ConnectOptions;
/// use sqlx_core::pool::{PoolConnectMetadata, PoolConnector};
///
/// struct MyConnector {
///     // A list of servers to connect to in a high-availability configuration.
///     host_ports: Vec<(String, u16)>,
///     username: String,
///     password: String,
/// }
///
/// impl PoolConnector<Postgres> for MyConnector {
///     // The desugaring of `async fn` is compatible with the signature of `connect()`.
///     async fn connect(&self, meta: PoolConnectMetadata) -> sqlx::Result<PgConnection> {
///         self.get_connect_options(meta.num_attempts)
///             .connect()
///             .await
///     }
/// }
///
/// impl MyConnector {
///     fn get_connect_options(&self, attempt: u32) -> PgConnectOptions {
///         // Select servers in a round-robin.
///         let (ref host, port) = self.host_ports[attempt as usize % self.host_ports.len()];
///
///         PgConnectOptions::new()
///             .host(host)
///             .port(port)
///             .username(&self.username)
///             .password(&self.password)
///     }
/// }
///
/// # async fn _example() -> sqlx::Result<()> {
/// let pool = PgPoolOptions::new()
///     .max_connections(25)
///     .connect_with_connector(MyConnector {
///         host_ports: vec![
///             ("db1.postgres.cluster.local".into(), 5432),
///             ("db2.postgres.cluster.local".into(), 5432),
///             ("db3.postgres.cluster.local".into(), 5432),
///             ("db4.postgres.cluster.local".into(), 5432),
///         ],
///         username: "my_username".into(),
///         password: "my password".into(),
///     })
///     .await?;
///
/// let conn = pool.acquire().await?;
///
/// # Ok(())
/// # }
/// ```
pub trait PoolConnector<DB: Database>: Send + Sync + 'static {
    /// Open a connection for the pool.
    ///
    /// Any setup that must be done on the connection should be performed before it is returned.
    ///
    /// If this method returns an error that is known to be retryable, it is called again
    /// in an exponential backoff loop. Retryable errors include, but are not limited to:
    ///
    /// * [`io::Error`]
    /// * Database errors for which
    ///   [`is_retryable_connect_error`][crate::error::DatabaseError::is_retryable_connect_error]
    ///   returns `true`.
    /// * [`Error::PoolConnector`] with `retryable: true`.
    ///   This error kind is not returned internally and is designed to allow this method to return
    ///   arbitrary error types not otherwise supported.
    ///
    /// This behavior may be customized by overriding [`Self::connect_with_control_flow()`].
    ///
    /// Manual implementations of this method may also use the signature:
    /// ```rust,ignore
    /// async fn connect(
    ///     &self,
    ///     meta: PoolConnectMetadata
    /// ) -> sqlx::Result<{PgConnection, MySqlConnection, SqliteConnection, etc.}>
    /// ```
    ///
    /// Note: the returned future must be `Send`.
    fn connect(
        &self,
        meta: PoolConnectMetadata,
    ) -> impl Future<Output = crate::Result<DB::Connection>> + Send + '_;

    /// Open a connection for the pool, or indicate what to do on an error.
    ///
    /// This method may return one of the following:
    ///
    /// * `ControlFlow::Break(Ok(_))` with a successfully established connection.
    /// * `ControlFlow::Break(Err(_))` with an error to immediately return to the caller.
    /// * `ControlFlow::Continue(_)` with a retryable error.
    ///   The pool will call this method again in an exponential backoff loop until it succeeds,
    ///   or the [connect timeout][PoolOptions::connect_timeout]
    ///   or [acquire timeout][PoolOptions::acquire_timeout] is reached.
    ///
    /// # Default Implementation
    /// This method has a provided implementation by default which calls [`Self::connect()`]
    /// and then returns `ControlFlow::Continue` if the error is any of the following:
    ///
    /// * [`io::Error`]
    /// * Database errors for which
    ///   [`is_retryable_connect_error`][crate::error::DatabaseError::is_retryable_connect_error]
    ///   returns `true`.
    /// * [`Error::PoolConnector`] with `retryable: true`.
    ///   This error kind is not returned internally and is designed to allow this method to return
    ///   arbitrary error types not otherwise supported.
    ///
    /// A custom backoff loop may be implemented by overriding this method and retrying internally,
    /// only returning `ControlFlow::Break` if/when an error should be propagated out to the caller.
    ///
    /// If this method is overridden and does not call [`Self::connect()`], then the implementation
    /// of the latter can be a stub. It is not called internally.
    fn connect_with_control_flow(
        &self,
        meta: PoolConnectMetadata,
    ) -> impl Future<Output = ControlFlow<crate::Result<DB::Connection>, Error>> + Send + '_ {
        async {
            match self.connect(meta).await {
                Err(err @ Error::Io(_)) => ControlFlow::Continue(err),
                Err(Error::Database(dbe)) if dbe.is_retryable_connect_error() => {
                    ControlFlow::Continue(Error::Database(dbe))
                }
                Err(
                    err @ Error::PoolConnector {
                        retryable: true, ..
                    },
                ) => ControlFlow::Continue(err),
                res => ControlFlow::Break(res),
            }
        }
    }
}

/// # Note: Future Changes (FIXME)
/// This could theoretically be replaced with an impl over `AsyncFn` to allow lending closures,
/// except we have no way to put the `Send` bound on the returned future.
///
/// We need Return Type Notation for that: https://github.com/rust-lang/rust/pull/138424
impl<DB, F, Fut> PoolConnector<DB> for F
where
    DB: Database,
    F: Fn(PoolConnectMetadata) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = crate::Result<DB::Connection>> + Send + 'static,
{
    fn connect(
        &self,
        meta: PoolConnectMetadata,
    ) -> impl Future<Output = crate::Result<DB::Connection>> + Send + '_ {
        self(meta)
    }
}

pub(crate) struct DefaultConnector<DB: Database>(
    pub <<DB as Database>::Connection as Connection>::Options,
);

impl<DB: Database> PoolConnector<DB> for DefaultConnector<DB> {
    fn connect(
        &self,
        _meta: PoolConnectMetadata,
    ) -> impl Future<Output = crate::Result<DB::Connection>> + Send + '_ {
        self.0.connect()
    }
}

/// Metadata passed to [`PoolConnector::connect()`] for every connection attempt.
#[derive(Debug)]
#[non_exhaustive]
pub struct PoolConnectMetadata {
    /// The instant at which the current connection task was started, including all attempts.
    ///
    /// May be used for reporting purposes, or to implement a custom backoff.
    pub start: Instant,

    /// The deadline (`start` plus the [connect timeout][PoolOptions::connect_timeout], if set).
    pub deadline: Option<Instant>,

    /// The number of attempts that have occurred so far.
    pub num_attempts: u32,
    /// The current size of the pool.
    pub pool_size: usize,
    /// The ID of the connection, unique for the pool.
    pub connection_id: ConnectionId,
}

pub struct DynConnector<DB: Database> {
    // We want to spawn the connection attempt as a task anyway
    connect: Box<
        dyn Fn(
                Pool<DB>,
                ConnectionId,
                DisconnectedSlot<ConnectionInner<DB>>,
                Arc<ConnectTaskShared>,
            ) -> ConnectTask<DB>
            + Send
            + Sync
            + 'static,
    >,
}

impl<DB: Database> DynConnector<DB> {
    pub fn new(connector: impl PoolConnector<DB>) -> Self {
        let connector = Arc::new(connector);

        Self {
            connect: Box::new(move |pool, id, guard, shared| {
                ConnectTask::spawn(pool, id, guard, connector.clone(), shared)
            }),
        }
    }

    pub fn connect(
        &self,
        pool: Pool<DB>,
        id: ConnectionId,
        slot: DisconnectedSlot<ConnectionInner<DB>>,
        shared: Arc<ConnectTaskShared>,
    ) -> ConnectTask<DB> {
        (self.connect)(pool, id, slot, shared)
    }
}

pub struct ConnectTask<DB: Database> {
    handle: JoinHandle<crate::Result<PoolConnection<DB>>>,
}

pub struct ConnectTaskShared {
    cancel_event: Event,
    // Using the normal `std::sync::Mutex` because the critical sections are very short;
    // we only hold the lock long enough to insert or take the value.
    last_error: Mutex<Option<Error>>,
}

impl<DB: Database> Future for ConnectTask<DB> {
    type Output = crate::Result<PoolConnection<DB>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.handle).poll(cx)
    }
}

impl<DB: Database> ConnectTask<DB> {
    fn spawn(
        pool: Pool<DB>,
        id: ConnectionId,
        guard: DisconnectedSlot<ConnectionInner<DB>>,
        connector: Arc<impl PoolConnector<DB>>,
        shared: Arc<ConnectTaskShared>,
    ) -> Self {
        let handle = crate::rt::spawn(connect_with_backoff(
            pool,
            id,
            connector,
            guard,
            shared.clone(),
        ));

        Self { handle }
    }
}

impl ConnectTaskShared {
    pub fn new_arc() -> Arc<Self> {
        Arc::new(Self {
            cancel_event: Event::new(),
            last_error: Mutex::new(None),
        })
    }

    pub fn take_error(&self) -> Option<Error> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    fn put_error(&self, error: Error) {
        *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
    }
}

/// An opaque connection ID, unique for every connection attempt with the same pool.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ConnectionId(usize);

impl ConnectionId {
    pub(super) fn next() -> ConnectionId {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

        ConnectionId(NEXT_ID.fetch_add(1, Ordering::AcqRel))
    }
}

impl Display for ConnectionId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0, f)
    }
}

#[tracing::instrument(
    target = "sqlx::pool::connect", 
    skip_all,
    fields(%connection_id),
    err
)]
async fn connect_with_backoff<DB: Database>(
    pool: Pool<DB>,
    connection_id: ConnectionId,
    connector: Arc<impl PoolConnector<DB>>,
    slot: DisconnectedSlot<ConnectionInner<DB>>,
    shared: Arc<ConnectTaskShared>,
) -> crate::Result<PoolConnection<DB>> {
    listener!(pool.0.on_closed => closed);
    listener!(shared.cancel_event => cancelled);

    let start = Instant::now();
    let deadline = pool
        .0
        .options
        .connect_timeout
        .and_then(|timeout| start.checked_add(timeout));

    for attempt in 1u32.. {
        let meta = PoolConnectMetadata {
            start,
            deadline,
            num_attempts: attempt,
            pool_size: pool.size(),
            connection_id,
        };

        tracing::trace!(
            target: "sqlx::pool::connect",
            %connection_id,
            attempt,
            elapsed_seconds=start.elapsed().as_secs_f64(),
            "beginning connection attempt"
        );

        let res = connector.connect_with_control_flow(meta).await;

        let now = Instant::now();
        let elapsed = now.duration_since(start);
        let elapsed_seconds = elapsed.as_secs_f64();

        match res {
            ControlFlow::Break(Ok(conn)) => {
                tracing::debug!(
                    target: "sqlx::pool::connect",
                    %connection_id,
                    attempt,
                    elapsed_seconds,
                    "connection established",
                );

                return Ok(PoolConnection::new(slot.put(ConnectionInner {
                    pool: Arc::downgrade(&pool.0),
                    raw: conn,
                    id: connection_id,
                    created_at: now,
                    last_released_at: now,
                })));
            }
            ControlFlow::Break(Err(e)) => {
                tracing::error!(
                    target: "sqlx::pool::connect",
                    %connection_id,
                    attempt,
                    elapsed_seconds,
                    error=?e,
                    "error connecting to database",
                );

                return Err(e);
            }
            ControlFlow::Continue(e) => {
                tracing::warn!(
                    target: "sqlx::pool::connect",
                    %connection_id,
                    attempt,
                    elapsed_seconds,
                    error=?e,
                    "error connecting to database; retrying",
                );

                shared.put_error(e);
            }
        }

        let wait = EASE_OFF
            .nth_retry_at(attempt, now, deadline, &mut rand::thread_rng())
            .map_err(|_| {
                Error::PoolTimedOut {
                    // This should be populated by the caller
                    last_connect_error: None,
                }
            })?;

        if let Some(wait) = wait {
            tracing::trace!(
                target: "sqlx::pool::connect",
                %connection_id,
                attempt,
                elapsed_seconds,
                "waiting for {:?}",
                wait.duration_since(now),
            );

            let mut sleep = pin!(rt::sleep_until(wait));

            std::future::poll_fn(|cx| {
                if let Poll::Ready(()) = Pin::new(&mut closed).poll(cx) {
                    return Poll::Ready(Err(Error::PoolClosed));
                }

                if let Poll::Ready(()) = Pin::new(&mut cancelled).poll(cx) {
                    return Poll::Ready(Err(Error::PoolTimedOut {
                        last_connect_error: None,
                    }));
                }

                ready!(sleep.as_mut().poll(cx));
                Poll::Ready(Ok(()))
            })
            .await?;
        }
    }

    Err(Error::PoolTimedOut {
        last_connect_error: None,
    })
}
