//! Pool tests against an in-process fake server; no `DATABASE_URL` needed.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Fake Postgres that completes the startup handshake, then keeps reading but never
/// replies again: the observable state of a peer that vanished without RST/FIN.
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
                let mut len_buf = [0u8; 4];
                if socket.read_exact(&mut len_buf).await.is_err() {
                    return;
                }
                let len = (u32::from_be_bytes(len_buf) as usize).saturating_sub(4);
                let mut body = vec![0u8; len];
                if socket.read_exact(&mut body).await.is_err() {
                    return;
                }

                // AuthenticationOk, ParameterStatus*, BackendKeyData, ReadyForQuery(idle)
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

                // Play dead: drain so client writes succeed, never respond.
                let mut buf = [0u8; 4096];
                while socket.read(&mut buf).await.map(|n| n > 0).unwrap_or(false) {}
            });
        }
    });

    (addr, connections)
}

/// Dropping a `PoolConnection` whose peer went silent must not strand its permit:
/// with `max_connections(1)` the next `acquire()` must open a fresh connection
/// instead of failing with `PoolTimedOut`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_recovers_permit_when_connection_unresponsive_on_release() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (addr, connections) = spawn_silent_postgres().await;
        let url = format!(
            "postgres://user@{}:{}/db?sslmode=disable",
            addr.ip(),
            addr.port()
        );

        // acquire_timeout must exceed the return-to-pool bound (5s).
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(20))
            .connect_lazy(&url)
            .expect("build pool");

        let conn = pool
            .acquire()
            .await
            .expect("first acquire opens a connection");
        drop(conn);

        let conn2 = pool
            .acquire()
            .await
            .expect("permit must be released once return-to-pool times out");
        drop(conn2);

        assert_eq!(
            connections.load(Ordering::SeqCst),
            2,
            "the unresponsive connection must be discarded and a fresh one opened"
        );
    })
    .await
    .expect("test hung: return-to-pool is not bounded");
}
