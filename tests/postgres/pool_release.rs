//! Tests for how `PgPool` tests a connection when it is returned, against an in-process server that counts the client's `Sync` messages. No `DATABASE_URL` is needed.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The server answers this simple query only after `SLOW_REPLY`, so a client can cancel it while the reply is still owed.
const SLOW_QUERY: &str = "SELECT pg_sleep(0.3)";
const SLOW_REPLY: Duration = Duration::from_millis(300);

#[derive(Default)]
struct Observed {
    connections: AtomicUsize,
    syncs: AtomicUsize,
}

/// A server that completes the startup handshake, answers every simple query with an empty result, and answers every `Sync` with `ReadyForQuery`. The release test is a bare `Sync`, so `syncs` counts it.
async fn spawn_counting_postgres() -> (SocketAddr, Arc<Observed>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let observed = Arc::new(Observed::default());
    let shared = observed.clone();

    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            shared.connections.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(serve(socket, shared.clone()));
        }
    });

    (addr, observed)
}

async fn serve(mut socket: TcpStream, observed: Arc<Observed>) -> std::io::Result<()> {
    // `sslmode=disable`, so the first message is the untagged StartupMessage.
    let len = socket.read_u32().await? as usize;
    let mut startup = vec![0; len.saturating_sub(4)];
    socket.read_exact(&mut startup).await?;

    let mut reply = Vec::new();
    message(&mut reply, b'R', &0u32.to_be_bytes()); // AuthenticationOk
    for parameter in [
        &b"server_version\x0017.0\x00"[..],
        b"client_encoding\x00UTF8\x00",
        b"DateStyle\x00ISO, MDY\x00",
    ] {
        message(&mut reply, b'S', parameter); // ParameterStatus
    }
    message(&mut reply, b'K', &[0, 0, 0, 1, 0, 0, 0, 2]); // BackendKeyData
    message(&mut reply, b'Z', b"I"); // ReadyForQuery
    socket.write_all(&reply).await?;

    loop {
        let tag = socket.read_u8().await?;
        let len = socket.read_u32().await? as usize;
        let mut body = vec![0; len.saturating_sub(4)];
        socket.read_exact(&mut body).await?;

        let mut reply = Vec::new();
        match tag {
            b'Q' => {
                if body.starts_with(SLOW_QUERY.as_bytes()) {
                    tokio::time::sleep(SLOW_REPLY).await;
                }
                message(&mut reply, b'C', b"SELECT 0\x00"); // CommandComplete
                message(&mut reply, b'Z', b"I");
            }
            b'S' => {
                observed.syncs.fetch_add(1, Ordering::SeqCst);
                message(&mut reply, b'Z', b"I");
            }
            b'X' => return Ok(()),
            _ => {}
        }
        socket.write_all(&reply).await?;
    }
}

fn message(buf: &mut Vec<u8>, tag: u8, body: &[u8]) {
    buf.push(tag);
    buf.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
    buf.extend_from_slice(body);
}

fn single_connection_pool(addr: SocketAddr) -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        // The acquire test is a `Sync` too; only the release path is under test here.
        .test_before_acquire(false)
        .connect_lazy(&format!(
            "postgres://postgres@{}:{}/sqlx?sslmode=disable",
            addr.ip(),
            addr.port()
        ))
        .expect("build pool")
}

/// A connection is returned to the pool by a spawned task, so wait until it is idle again.
async fn until_returned(pool: &PgPool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while pool.num_idle() < 1 {
        assert!(
            Instant::now() < deadline,
            "the connection never returned to the pool"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test]
async fn a_connection_with_nothing_in_flight_returns_without_a_round_trip() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (addr, observed) = spawn_counting_postgres().await;
        let pool = single_connection_pool(addr);

        for _ in 0..3 {
            sqlx::raw_sql("SELECT 1")
                .execute(&pool)
                .await
                .expect("query");
            until_returned(&pool).await;
        }

        assert_eq!(
            observed.syncs.load(Ordering::SeqCst),
            0,
            "a connection with no reply outstanding was tested on release"
        );
        assert_eq!(observed.connections.load(Ordering::SeqCst), 1);
    })
    .await
    .expect("test hung");
}

#[tokio::test]
async fn a_connection_cancelled_mid_query_is_flushed_before_reuse() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (addr, observed) = spawn_counting_postgres().await;
        let pool = single_connection_pool(addr);

        sqlx::raw_sql("SELECT 1")
            .execute(&pool)
            .await
            .expect("query");
        until_returned(&pool).await;

        let cancelled = tokio::time::timeout(
            Duration::from_millis(50),
            sqlx::raw_sql(SLOW_QUERY).execute(&pool),
        )
        .await;
        assert!(
            cancelled.is_err(),
            "the slow query should not have completed"
        );
        until_returned(&pool).await;

        assert_eq!(
            observed.syncs.load(Ordering::SeqCst),
            1,
            "a connection still owed a reply was returned without being tested"
        );

        sqlx::raw_sql("SELECT 1")
            .execute(&pool)
            .await
            .expect("query after the flush");
        until_returned(&pool).await;

        assert_eq!(observed.syncs.load(Ordering::SeqCst), 1);
        assert_eq!(
            observed.connections.load(Ordering::SeqCst),
            1,
            "the flushed connection should have been reused"
        );
    })
    .await
    .expect("test hung");
}
