//! Pool behavior tests that do not require a live database: they drive a real
//! `PgPool` against an in-process fake server, so they run anywhere `cargo test`
//! runs without a `DATABASE_URL`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Spawn a fake PostgreSQL server that completes the startup handshake and then
/// goes silent: it keeps reading (and thus ACKing) whatever the client sends but
/// never sends another byte back. This is the observable state of a connection
/// whose peer has silently gone away — e.g. a NAT/firewall that dropped the flow
/// without sending RST/FIN. Any request the client makes (including the pool's
/// on-release `ping`) will be written successfully and then awaited forever.
///
/// Returns the bound address and a counter of accepted physical connections.
async fn spawn_silent_postgres() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let connections = Arc::new(AtomicUsize::new(0));
    let accepted = connections.clone();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            accepted.fetch_add(1, Ordering::SeqCst);

            tokio::spawn(async move {
                // Read the client's StartupMessage (length-prefixed, no type byte).
                let mut len_buf = [0u8; 4];
                if socket.read_exact(&mut len_buf).await.is_err() {
                    return;
                }
                let len = (u32::from_be_bytes(len_buf) as usize).saturating_sub(4);
                let mut body = vec![0u8; len];
                if socket.read_exact(&mut body).await.is_err() {
                    return;
                }

                // Minimal successful handshake: AuthenticationOk, a few
                // ParameterStatus messages the client reads during connect,
                // BackendKeyData, then ReadyForQuery(idle).
                let mut reply: Vec<u8> = Vec::new();
                reply.extend([b'R', 0, 0, 0, 8, 0, 0, 0, 0]);
                for (key, value) in [
                    ("server_version", "14.0"),
                    ("client_encoding", "UTF8"),
                    ("DateStyle", "ISO, MDY"),
                ] {
                    let payload_len = 4 + key.len() + 1 + value.len() + 1;
                    reply.push(b'S');
                    reply.extend((payload_len as u32).to_be_bytes());
                    reply.extend(key.as_bytes());
                    reply.push(0);
                    reply.extend(value.as_bytes());
                    reply.push(0);
                }
                reply.extend([b'K', 0, 0, 0, 12]);
                reply.extend(1234u32.to_be_bytes());
                reply.extend(5678u32.to_be_bytes());
                reply.extend([b'Z', 0, 0, 0, 5, b'I']);
                if socket.write_all(&reply).await.is_err() {
                    return;
                }

                // Handshake complete. Play dead: drain client bytes forever
                // (so writes keep succeeding) but never respond.
                let mut buf = [0u8; 4096];
                while socket.read(&mut buf).await.map(|n| n > 0).unwrap_or(false) {}
            });
        }
    });

    (addr, connections)
}

/// Regression test: a `PoolConnection` whose peer has silently gone away must not
/// strand its permit when dropped.
///
/// On drop the pool spawns a task that `ping`s the connection before returning it.
/// Against the silent server that ping is sent but never answered. If the ping is
/// unbounded, the spawned task parks forever holding the connection's permit, so a
/// `max_connections(1)` pool can never hand out another connection and every later
/// `acquire()` fails with `PoolTimedOut`. With the on-release ping bounded, the
/// permit is released and a fresh connection can be opened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_recovers_permit_when_connection_unresponsive_on_release() {
    // Overall guard so a regression hangs this one test instead of wedging CI.
    tokio::time::timeout(Duration::from_secs(60), async {
        let (addr, connections) = spawn_silent_postgres().await;
        let url = format!(
            "postgres://user@{}:{}/db?sslmode=disable",
            addr.ip(),
            addr.port()
        );

        // acquire_timeout must exceed the on-release ping bound (5s) so that,
        // with the fix, the freed permit is observable before this deadline;
        // without the fix, acquire genuinely times out here.
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(20))
            .connect_lazy(&url)
            .expect("build pool");

        // Opens physical connection #1 (fresh connections are not ping-tested).
        let conn = pool
            .acquire()
            .await
            .expect("first acquire opens a connection");
        // Drop triggers the spawned on-release ping, which the silent server
        // never answers.
        drop(conn);

        // With the bounded ping the permit is released after the connection is
        // found unresponsive, so this opens a fresh physical connection.
        let conn2 = pool
            .acquire()
            .await
            .expect("permit must be released after the bounded on-release ping");
        drop(conn2);

        assert_eq!(
            connections.load(Ordering::SeqCst),
            2,
            "the unresponsive connection must be discarded and a fresh one opened"
        );
    })
    .await
    .expect("test hung: on-release connection test is not bounded");
}
