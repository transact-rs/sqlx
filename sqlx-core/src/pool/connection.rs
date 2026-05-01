use std::fmt::{self, Debug, Formatter};
use std::future::{self, Future};
use std::io;
use std::ops::{Deref, DerefMut};
use std::sync::Weak;
use std::time::{Duration, Instant};

use crate::connection::Connection;
use crate::database::Database;
use crate::error::Error;

use super::inner::PoolInner;
use crate::pool::connect::ConnectionId;
use crate::pool::connection_set::{ConnectedSlot, DisconnectedSlot};
use crate::pool::options::PoolConnectionMetadata;
use crate::pool::PoolOptions;
use crate::rt;

const RETURN_TO_POOL_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// A connection managed by a [`Pool`][crate::pool::Pool].
///
/// Will be returned to the pool on-drop.
pub struct PoolConnection<DB: Database> {
    conn: Option<ConnectedSlot<ConnectionInner<DB>>>,
    close_on_drop: bool,
}

pub(super) struct ConnectionInner<DB: Database> {
    // Note: must be `Weak` to prevent a reference cycle
    pub(crate) pool: Weak<PoolInner<DB>>,
    pub(super) raw: DB::Connection,
    pub(super) id: ConnectionId,
    pub(super) created_at: Instant,
    pub(super) last_released_at: Instant,
}

const EXPECT_MSG: &str = "BUG: inner connection already taken!";

impl<DB: Database> Debug for PoolConnection<DB> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolConnection")
            .field("database", &DB::NAME)
            .field("id", &self.conn.as_ref().map(|live| live.id))
            .finish()
    }
}

impl<DB: Database> Deref for PoolConnection<DB> {
    type Target = DB::Connection;

    fn deref(&self) -> &Self::Target {
        &self.conn.as_ref().expect(EXPECT_MSG).raw
    }
}

impl<DB: Database> DerefMut for PoolConnection<DB> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.conn.as_mut().expect(EXPECT_MSG).raw
    }
}

impl<DB: Database> AsRef<DB::Connection> for PoolConnection<DB> {
    fn as_ref(&self) -> &DB::Connection {
        self
    }
}

impl<DB: Database> AsMut<DB::Connection> for PoolConnection<DB> {
    fn as_mut(&mut self) -> &mut DB::Connection {
        self
    }
}

impl<DB: Database> PoolConnection<DB> {
    pub(super) fn new(live: ConnectedSlot<ConnectionInner<DB>>) -> Self {
        Self {
            conn: Some(live),
            close_on_drop: false,
        }
    }

    /// Close this connection, allowing the pool to open a replacement.
    ///
    /// Equivalent to calling [`.detach()`] then [`.close()`], but the connection permit is retained
    /// for the duration so that the pool may not exceed `max_connections`.
    ///
    /// [`.detach()`]: PoolConnection::detach
    /// [`.close()`]: Connection::close
    pub async fn close(mut self) -> Result<(), Error> {
        let (res, _slot) = close(self.take_conn()).await;
        res
    }

    /// Close this connection on-drop, instead of returning it to the pool.
    ///
    /// May be used in cases where waiting for the [`.close()`][Self::close] call
    /// to complete is unacceptable, but you still want the connection to be closed gracefully
    /// so that the server can clean up resources.
    #[inline(always)]
    pub fn close_on_drop(&mut self) {
        self.close_on_drop = true;
    }

    /// Detach this connection from the pool, allowing it to open a replacement.
    ///
    /// Note that if your application uses a single shared pool, this
    /// effectively lets the application exceed the [`max_connections`] setting.
    ///
    /// If [`min_connections`] is nonzero, a task will be spawned to replace this connection.
    ///
    /// If you want the pool to treat this connection as permanently checked-out,
    /// use [`.leak()`][Self::leak] instead.
    ///
    /// [`max_connections`]: crate::pool::PoolOptions::max_connections
    /// [`min_connections`]: crate::pool::PoolOptions::min_connections
    pub fn detach(mut self) -> DB::Connection {
        let (conn, _slot) = ConnectedSlot::take(self.take_conn());
        conn.raw
    }

    /// Detach this connection from the pool, treating it as permanently checked-out.
    ///
    /// This effectively will reduce the maximum capacity of the pool by 1 every time it is used.
    ///
    /// If you don't want to impact the pool's capacity, use [`.detach()`][Self::detach] instead.
    pub fn leak(mut self) -> DB::Connection {
        let (conn, slot) = ConnectedSlot::take(self.take_conn());
        DisconnectedSlot::leak(slot);
        conn.raw
    }

    fn take_conn(&mut self) -> ConnectedSlot<ConnectionInner<DB>> {
        self.conn.take().expect(EXPECT_MSG)
    }

    /// Test the connection to make sure it is still live before returning it to the pool.
    ///
    /// This effectively runs the drop handler eagerly instead of spawning a task to do it.
    #[doc(hidden)]
    pub fn return_to_pool(&mut self) -> impl Future<Output = ()> + Send + 'static {
        let conn = self.conn.take();

        async move {
            let Some(conn) = conn else {
                return;
            };

            let Some(pool) = Weak::upgrade(&conn.pool) else {
                return;
            };

            rt::timeout(RETURN_TO_POOL_TIMEOUT, return_to_pool(conn, &pool))
                .await
                // Dropping of the `slot` will check if the connection must be re-established
                // but only after trying to pass it to a task that needs it.
                .ok();
        }
    }

    fn take_and_close(&mut self) -> impl Future<Output = ()> + Send + 'static {
        let conn = self.conn.take();

        async move {
            if let Some(conn) = conn {
                // Don't hold the connection forever if it hangs while trying to close
                rt::timeout(CLOSE_TIMEOUT, close(conn)).await.ok();
            }
        }
    }
}

impl<'c, DB: Database> crate::acquire::Acquire<'c> for &'c mut PoolConnection<DB> {
    type Database = DB;

    type Connection = &'c mut <DB as Database>::Connection;

    #[inline]
    fn acquire(self) -> futures_core::future::BoxFuture<'c, Result<Self::Connection, Error>> {
        Box::pin(future::ready(Ok(&mut **self)))
    }

    #[inline]
    fn begin(
        self,
    ) -> futures_core::future::BoxFuture<'c, Result<crate::transaction::Transaction<'c, DB>, Error>>
    {
        crate::transaction::Transaction::begin(&mut **self, None)
    }
}

/// Returns the connection to the [`Pool`][crate::pool::Pool] it was checked-out from.
impl<DB: Database> Drop for PoolConnection<DB> {
    fn drop(&mut self) {
        if self.close_on_drop {
            crate::rt::spawn(self.take_and_close());
            return;
        }

        // We still need to spawn a task to maintain `min_connections`.
        if self.conn.is_some() {
            crate::rt::spawn(self.return_to_pool());
        }
    }
}

impl<DB: Database> ConnectionInner<DB> {
    pub fn metadata(&self) -> PoolConnectionMetadata {
        PoolConnectionMetadata {
            age: self.created_at.elapsed(),
            idle_for: Duration::ZERO,
        }
    }

    pub fn idle_metadata(&self) -> PoolConnectionMetadata {
        // Use a single `now` value for consistency.
        let now = Instant::now();

        PoolConnectionMetadata {
            // NOTE: the receiver is the later `Instant` and the arg is the earlier
            // https://github.com/launchbadge/sqlx/issues/1912
            age: now.saturating_duration_since(self.created_at),
            idle_for: now.saturating_duration_since(self.last_released_at),
        }
    }

    pub fn is_beyond_max_lifetime(&self, options: &PoolOptions<DB>) -> bool {
        if let Some(max_lifetime) = options.max_lifetime {
            let age = self.created_at.elapsed();

            if age > max_lifetime {
                tracing::info!(
                    target: "sqlx::pool",
                    connection_id=%self.id,
                    ?age,
                    "connection is beyond `max_lifetime`, closing"
                );

                return true;
            }
        }

        false
    }

    pub fn is_beyond_idle_timeout(&self, options: &PoolOptions<DB>) -> bool {
        if let Some(idle_timeout) = options.idle_timeout {
            let now = Instant::now();

            let age = now.duration_since(self.created_at);
            let idle_duration = now.duration_since(self.last_released_at);

            if idle_duration > idle_timeout {
                tracing::info!(
                    target: "sqlx::pool",
                    connection_id=%self.id,
                    ?age,
                    ?idle_duration,
                    "connection is beyond `idle_timeout`, closing"
                );

                return true;
            }
        }

        false
    }
}

pub(crate) async fn close<DB: Database>(
    conn: ConnectedSlot<ConnectionInner<DB>>,
) -> (Result<(), Error>, DisconnectedSlot<ConnectionInner<DB>>) {
    let connection_id = conn.id;

    tracing::debug!(target: "sqlx::pool", %connection_id, "closing connection (gracefully)");

    let (conn, slot) = ConnectedSlot::take(conn);

    let res = rt::timeout(CLOSE_TIMEOUT, conn.raw.close())
        .await
        .unwrap_or_else(|_| {
            Err(io::Error::new(io::ErrorKind::TimedOut, "timed out sending close packet").into())
        })
        .inspect_err(|error| {
            tracing::debug!(
                target: "sqlx::pool",
                %connection_id,
                %error,
                "error occurred while closing the pool connection"
            );
        });

    (res, slot)
}
pub(crate) async fn close_hard<DB: Database>(
    conn: ConnectedSlot<ConnectionInner<DB>>,
) -> (Result<(), Error>, DisconnectedSlot<ConnectionInner<DB>>) {
    let connection_id = conn.id;

    tracing::debug!(
        target: "sqlx::pool",
        %connection_id,
        "closing connection (forcefully)"
    );

    let (conn, slot) = ConnectedSlot::take(conn);

    let res = rt::timeout(CLOSE_TIMEOUT, conn.raw.close_hard())
        .await
        .unwrap_or_else(|_| {
            Err(io::Error::new(io::ErrorKind::TimedOut, "timed out sending close packet").into())
        })
        .inspect_err(|error| {
            tracing::debug!(
                target: "sqlx::pool",
                %connection_id,
                %error,
                "error occurred while closing the pool connection"
            );
        });

    (res, slot)
}

/// Return the connection to the pool.
///
/// Returns `true` if the connection was successfully returned, `false` if it was closed.
async fn return_to_pool<DB: Database>(
    mut conn: ConnectedSlot<ConnectionInner<DB>>,
    pool: &PoolInner<DB>,
) -> Result<(), DisconnectedSlot<ConnectionInner<DB>>> {
    // Immediately close the connection.
    if pool.is_closed() {
        let (_res, slot) = close(conn).await;
        return Err(slot);
    }

    // If the connection is beyond max lifetime, close the connection and
    // immediately create a new connection
    if conn.is_beyond_max_lifetime(&pool.options) {
        let (_res, slot) = close(conn).await;
        return Err(slot);
    }

    if let Some(test) = &pool.options.after_release {
        let meta = conn.metadata();
        match (test)(&mut conn.raw, meta).await {
            Ok(true) => (),
            Ok(false) => {
                let (_res, slot) = close(conn).await;
                return Err(slot);
            }
            Err(error) => {
                tracing::warn!(%error, "error from `after_release`");
                // Connection is broken, don't try to gracefully close as
                // something weird might happen.
                let (_res, slot) = close_hard(conn).await;
                return Err(slot);
            }
        }
    }

    // test the connection on-release to ensure it is still viable,
    // and flush anything time-sensitive like transaction rollbacks
    // if an Executor future/stream is dropped during an `.await` call, the connection
    // is likely to be left in an inconsistent state, in which case it should not be
    // returned to the pool; also of course, if it was dropped due to an error
    // this is simply a band-aid as SQLx-next connections should be able
    // to recover from cancellations
    if let Err(error) = conn.raw.ping().await {
        tracing::warn!(
            target: "sqlx::pool",
            %error,
            "error occurred while testing the connection on-release",
        );

        // Connection is broken, don't try to gracefully close.
        let (_res, slot) = close_hard(conn).await;
        Err(slot)
    } else {
        // if the connection is still viable, release it to the pool
        drop(conn);
        Ok(())
    }
}
