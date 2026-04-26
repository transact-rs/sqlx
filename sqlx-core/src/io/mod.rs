mod buf;
mod buf_mut;
// mod buf_stream;
mod decode;
mod encode;
mod read_buf;
// mod write_and_flush;

pub use buf::BufExt;
pub use buf_mut::BufMutExt;
//pub use buf_stream::BufStream;
pub use decode::ProtocolDecode;
pub use encode::ProtocolEncode;
pub use read_buf::ReadBuf;

#[cfg(not(feature = "_rt-tokio"))]
pub use futures_io::AsyncRead;

#[cfg(feature = "_rt-tokio")]
pub use tokio::io::AsyncRead;

#[cfg(not(feature = "_rt-tokio"))]
pub use futures_util::io::AsyncReadExt;

#[cfg(feature = "_rt-tokio")]
pub use tokio::io::AsyncReadExt;

pub async fn read_from(
    mut source: impl AsyncRead + Unpin,
    data: &mut Vec<u8>,
) -> std::io::Result<usize> {
    match () {
        // Tokio lets us read into the buffer without zeroing first
        #[cfg(feature = "_rt-tokio")]
        _ => source.read_buf(data).await,
        #[cfg(not(feature = "_rt-tokio"))]
        _ => {
            data.resize(data.capacity(), 0);
            source.read(data).await
        }
    }
}
