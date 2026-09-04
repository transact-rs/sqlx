mod socket;
pub mod tls;

pub use socket::{
    connect_tcp, connect_uds, BufferedSocket, Socket, SocketIntoBox, WithSocket, WriteBuffer,
};

#[cfg(feature = "_rt-tokio")]
pub use socket::async_rw_adapter::TokioStream;

#[cfg(feature = "_rt-async-io")]
pub use socket::async_rw_adapter::FuturesStream;
