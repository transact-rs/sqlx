mod socket;
pub mod tls;

pub use socket::{
    connect_tcp, connect_uds, BufferStats, BufferedSocket, Socket, SocketIntoBox, WithSocket,
    WriteBuffer,
};
