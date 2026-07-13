//! Generic Response Packets
//!
//! <https://dev.mysql.com/doc/internals/en/generic-response-packets.html>
//! <https://mariadb.com/kb/en/4-server-response-packets/>

mod eof;
mod err;
mod local_infile;
mod ok;
mod status;

pub use eof::EofPacket;
pub use err::ErrPacket;
pub use local_infile::LocalInfilePacket;
pub use ok::OkPacket;
pub use status::Status;
