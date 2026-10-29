use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const AUTHENTICATION_OK: &[u8] = b"R\0\0\0\x08\0\0\0\0";
const BACKEND_KEY_DATA: &[u8] = b"K\0\0\0\x0c\0\0\0\x01\0\0\0\x02";
const READY_FOR_QUERY: &[u8] = b"Z\0\0\0\x05I";

#[tokio::test]
async fn return_to_pool_ping_timeout_recovers_pool_capacity() -> anyhow::Result<()> {
    let server = FakePostgresServer::bind().await?;
    let options = PgConnectOptions::new()
        .host("127.0.0.1")
        .port(server.port())
        .username("postgres")
        .database("postgres")
        .ssl_mode(PgSslMode::Disable);

    let pool = PgPoolOptions::new()
        .min_connections(0)
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(8))
        .test_before_acquire(false)
        .connect_with(options)
        .await?;

    let conn = pool.acquire().await?;
    assert_eq!(server.connection_count(), 1);

    drop(conn);

    let conn = pool.acquire().await?;
    assert_eq!(server.connection_count(), 2);

    conn.close().await?;
    pool.close().await;

    Ok(())
}

struct FakePostgresServer {
    port: u16,
    connection_count: Arc<AtomicUsize>,
}

impl FakePostgresServer {
    async fn bind() -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let connection_count = Arc::new(AtomicUsize::new(0));

        tokio::spawn(accept_connections(listener, Arc::clone(&connection_count)));

        Ok(Self {
            port,
            connection_count,
        })
    }

    fn port(&self) -> u16 {
        self.port
    }

    fn connection_count(&self) -> usize {
        self.connection_count.load(Ordering::SeqCst)
    }
}

async fn accept_connections(listener: TcpListener, connection_count: Arc<AtomicUsize>) {
    while let Ok((socket, _)) = listener.accept().await {
        connection_count.fetch_add(1, Ordering::SeqCst);

        tokio::spawn(async move {
            let _ = handle_connection(socket).await;
        });
    }
}

async fn handle_connection(mut socket: TcpStream) -> std::io::Result<()> {
    read_startup_message(&mut socket).await?;

    socket.write_all(AUTHENTICATION_OK).await?;
    socket.write_all(BACKEND_KEY_DATA).await?;
    socket.write_all(READY_FOR_QUERY).await?;
    socket.flush().await?;

    let mut buf = [0_u8; 1024];

    loop {
        if socket.read(&mut buf).await? == 0 {
            return Ok(());
        }
    }
}

async fn read_startup_message(socket: &mut TcpStream) -> std::io::Result<()> {
    let mut len = [0_u8; 4];
    socket.read_exact(&mut len).await?;

    let len = u32::from_be_bytes(len) as usize;
    let mut body = vec![0_u8; len.saturating_sub(4)];
    socket.read_exact(&mut body).await?;

    Ok(())
}
