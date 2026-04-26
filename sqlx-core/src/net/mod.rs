mod socket;
pub mod tls;

pub use socket::{
    connect_tcp, connect_uds, BufferedSocket, Socket, SocketExt, SocketIntoBox, WithSocket,
    WriteBuffer,
};
