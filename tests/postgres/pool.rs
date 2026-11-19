//! Pool regression tests using an in-process server; no DATABASE_URL is needed.

use std::future;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sqlx::pool::PoolConnection;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgPool, Postgres};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{sleep, timeout};

// Allow ample time for the pool's five-second return timeout on busy CI workers.
const TEST_TIMEOUT: Duration = Duration::from_secs(20);

struct TestServer {
    addr: SocketAddr,
    accepted: Arc<AtomicUsize>,
    disconnected: mpsc::UnboundedReceiver<()>,
    task: JoinHandle<()>,
}

impl TestServer {
    async fn start(respond_to_ping: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let (disconnect_tx, disconnected) = mpsc::unbounded_channel();

        let task = tokio::spawn({
            let accepted = accepted.clone();
            async move {
                let mut connections = JoinSet::new();
                loop {
                    tokio::select! {
                        result = listener.accept() => {
                            let (socket, _) = result.unwrap();
                            accepted.fetch_add(1, Ordering::SeqCst);
                            let disconnect_tx = disconnect_tx.clone();
                            connections.spawn(async move {
                                serve(socket, respond_to_ping).await.unwrap();
                                let _ = disconnect_tx.send(());
                            });
                        }
                        result = connections.join_next(), if !connections.is_empty() => {
                            result.unwrap().unwrap();
                        }
                    }
                }
            }
        });

        Self {
            addr,
            accepted,
            disconnected,
            task,
        }
    }

    fn connect_options(&self) -> PgConnectOptions {
        PgConnectOptions::new()
            .host("127.0.0.1")
            .port(self.addr.port())
            .username("test")
            .database("test")
            .ssl_mode(PgSslMode::Disable)
    }

    async fn wait_for_disconnect(&mut self) {
        timeout(TEST_TIMEOUT, self.disconnected.recv())
            .await
            .expect("timed-out return must close the socket")
            .expect("server stopped before the socket closed");
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // Dropping the server's JoinSet also aborts any remaining connections.
        self.task.abort();
    }
}

async fn serve(mut socket: TcpStream, respond_to_ping: bool) -> io::Result<()> {
    // StartupMessage has a length prefix but no message-type byte.
    let length = socket.read_u32().await?;
    let mut body = vec![0; usize::try_from(length.checked_sub(4).unwrap()).unwrap()];
    socket.read_exact(&mut body).await?;

    // AuthenticationOk followed by ReadyForQuery(idle).
    socket.write_all(b"R\0\0\0\x08\0\0\0\0Z\0\0\0\x05I").await?;

    if !respond_to_ping {
        // Keep accepting client bytes, but never answer the release ping.
        // Unlike a reset or EOF, this leaves the client's read pending forever.
        tokio::io::copy(&mut socket, &mut tokio::io::sink()).await?;
        return Ok(());
    }

    loop {
        let mut tag = [0];
        if socket.read(&mut tag).await? == 0 {
            return Ok(());
        }
        let length = socket.read_u32().await?;
        body.resize(usize::try_from(length.checked_sub(4).unwrap()).unwrap(), 0);
        socket.read_exact(&mut body).await?;

        match tag[0] {
            b'S' => socket.write_all(b"Z\0\0\0\x05I").await?, // Sync -> ReadyForQuery
            b'X' => return Ok(()),                            // Terminate
            tag => panic!("unexpected client message: {tag}"),
        }
    }
}

fn pool_options() -> PgPoolOptions {
    PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(TEST_TIMEOUT)
        .max_lifetime(None)
        .idle_timeout(None)
}

async fn assert_capacity_recovers(
    server: &mut TestServer,
    pool: &PgPool,
    conn: PoolConnection<Postgres>,
) {
    drop(conn);
    server.wait_for_disconnect().await;
    assert_eq!(
        pool.size(),
        0,
        "the discarded connection must leave the pool"
    );
    assert_eq!(
        pool.num_idle(),
        0,
        "the stalled connection must not be reused"
    );

    let replacement = pool.acquire().await.expect("pool permit must be restored");
    assert_eq!(server.accepted.load(Ordering::SeqCst), 2);
    assert_eq!(pool.size(), 1);

    drop(replacement.detach());
    pool.close().await;
}

#[tokio::test]
async fn drop_recovers_capacity_after_unresponsive_release_ping() {
    let mut server = TestServer::start(false).await;
    let pool = pool_options().connect_lazy_with(server.connect_options());
    let conn = pool.acquire().await.unwrap();

    assert_capacity_recovers(&mut server, &pool, conn).await;
}

#[tokio::test]
async fn drop_recovers_capacity_after_hanging_after_release() {
    let mut server = TestServer::start(true).await;
    let pool = pool_options()
        .after_release(|_, _| Box::pin(future::pending()))
        .connect_lazy_with(server.connect_options());
    let conn = pool.acquire().await.unwrap();

    // A timeout around only ping() cannot recover from a stalled callback.
    assert_capacity_recovers(&mut server, &pool, conn).await;
}

#[tokio::test]
async fn return_timeout_replenishes_min_connections() {
    let mut server = TestServer::start(true).await;
    let pool = pool_options()
        .min_connections(1)
        .after_release(|_, _| Box::pin(future::pending()))
        .connect_lazy_with(server.connect_options());
    let conn = pool.acquire().await.unwrap();
    drop(conn);

    server.wait_for_disconnect().await;
    timeout(TEST_TIMEOUT, async {
        while pool.num_idle() != 1 {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("return timeout must still run min_connections maintenance");
    assert_eq!(server.accepted.load(Ordering::SeqCst), 2);
    assert_eq!(pool.size(), 1);

    pool.close().await;
}

#[tokio::test]
async fn healthy_connection_returns_to_idle_queue() {
    let server = TestServer::start(true).await;
    let pool = pool_options()
        .after_release(|_, _| Box::pin(async { Ok(true) }))
        .connect_lazy_with(server.connect_options());
    let mut conn = pool.acquire().await.unwrap();

    timeout(TEST_TIMEOUT, conn.return_to_pool()).await.unwrap();
    assert_eq!(pool.num_idle(), 1);
    assert_eq!(pool.size(), 1);

    let replacement = pool.acquire().await.unwrap();
    assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
    drop(replacement.detach());
    pool.close().await;
}
