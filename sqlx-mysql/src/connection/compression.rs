use crate::protocol::Capabilities;
use crate::CompressionConfig;
use sqlx_core::io::{ProtocolDecode, ProtocolEncode};
use sqlx_core::net::{BufferedSocket, Socket};
use sqlx_core::Error;
#[cfg(any(feature = "zlib-compression", feature = "zstd-compression"))]
use {crate::Compression, compressed_stream::CompressedStream};

pub(crate) struct CompressionMySqlStream<S = Box<dyn Socket>> {
    mode: CompressionMode,
    pub(crate) socket: BufferedSocket<S>,
}

impl<S: Socket> CompressionMySqlStream<S> {
    pub(crate) fn not_compressed(socket: BufferedSocket<S>) -> Self {
        let mode = CompressionMode::NotCompressed;
        Self { mode, socket }
    }

    #[cfg(any(feature = "zstd-compression", feature = "zlib-compression"))]
    fn compressed(socket: BufferedSocket<S>, compression: CompressionConfig) -> Self {
        let mode = CompressionMode::Compressed(CompressedStream::new(compression));
        Self { mode, socket }
    }

    pub(crate) fn create(
        socket: BufferedSocket<S>,
        #[cfg_attr(
            not(all(feature = "zstd-compression", feature = "zlib-compression")),
            allow(unused_variables)
        )]
        capabilities: &Capabilities,
        compression_configs: &[CompressionConfig],
    ) -> Self {
        let supported_compression = compression_configs.iter().find(|c| {
            let is_supported = match c.0 {
                #[cfg(feature = "zlib-compression")]
                Compression::Zlib => capabilities.contains(Capabilities::COMPRESS),
                #[cfg(feature = "zstd-compression")]
                Compression::Zstd => {
                    capabilities.contains(Capabilities::ZSTD_COMPRESSION_ALGORITHM)
                }
                #[cfg(not(any(feature = "zstd-compression", feature = "zlib-compression")))]
                _ => false,
            };
            if !is_supported {
                tracing::warn!("server doesn't support '{:?}' compression", c.0);
            }
            is_supported
        });
        match supported_compression {
            #[cfg(any(feature = "zstd-compression", feature = "zlib-compression"))]
            Some(c) => CompressionMySqlStream::compressed(socket, *c),
            _ => CompressionMySqlStream::not_compressed(socket),
        }
    }

    pub(crate) fn boxed(self) -> CompressionMySqlStream<Box<dyn Socket>> {
        CompressionMySqlStream {
            socket: self.socket.boxed(),
            mode: self.mode,
        }
    }

    pub(crate) async fn read_with<'de, T, C>(
        &mut self,
        byte_len: usize,
        context: C,
    ) -> Result<T, Error>
    where
        T: ProtocolDecode<'de, C>,
    {
        match self.mode {
            CompressionMode::NotCompressed => self.socket.read_with(byte_len, context).await,
            #[cfg(any(feature = "zstd-compression", feature = "zlib-compression"))]
            CompressionMode::Compressed(ref mut s) => {
                s.read_with(byte_len, context, &mut self.socket).await
            }
        }
    }

    pub(crate) async fn write_with<'en, 'stream, T>(
        &mut self,
        value: T,
        context: (Capabilities, &'stream mut u8),
    ) -> Result<(), Error>
    where
        T: ProtocolEncode<'en, (Capabilities, &'stream mut u8)>,
    {
        match self.mode {
            CompressionMode::NotCompressed => self.socket.write_with(value, context),
            #[cfg(any(feature = "zstd-compression", feature = "zlib-compression"))]
            CompressionMode::Compressed(ref mut s) => {
                s.write_with(value, context, &mut self.socket).await
            }
        }
    }

    pub(crate) fn uncompressed_write_with<'en, 'stream, T>(
        &mut self,
        value: T,
        context: (Capabilities, &'stream mut u8),
    ) -> Result<(), Error>
    where
        T: ProtocolEncode<'en, (Capabilities, &'stream mut u8)>,
    {
        match self.mode {
            CompressionMode::NotCompressed => self.socket.write_with(value, context),
            #[cfg(any(feature = "zstd-compression", feature = "zlib-compression"))]
            CompressionMode::Compressed(ref mut s) => {
                s.uncompressed_write_with(value, context, &mut self.socket)
            }
        }
    }
}

enum CompressionMode {
    NotCompressed,
    #[cfg(any(feature = "zstd-compression", feature = "zlib-compression"))]
    Compressed(CompressedStream),
}

#[cfg(any(feature = "zstd-compression", feature = "zlib-compression"))]
mod compressed_stream {
    use crate::{Compression, CompressionConfig};
    use bytes::{Buf, BufMut, Bytes, BytesMut};
    #[cfg(feature = "zlib-compression")]
    use flate2::{
        write::ZlibEncoder, Compression as ZlibCompression, Decompress as ZlibDecompressor,
        FlushDecompress, Status,
    };
    use sqlx_core::io::{ProtocolDecode, ProtocolEncode};
    use sqlx_core::net::{BufferedSocket, Socket};
    use sqlx_core::rt::yield_now;
    use sqlx_core::Error;
    use std::cmp::min;
    use std::io::{Cursor, Write};
    #[cfg(feature = "zstd-compression")]
    use zstd::stream::{
        raw::{Decoder as ZstdDecoder, InBuffer, Operation, OutBuffer},
        Encoder as ZstdEncoder,
    };

    pub(crate) struct CompressedStream {
        compression_config: CompressionConfig,
        sequence_id: u8,
        packet_reader: Option<CompressedPacketReader>,
    }

    impl CompressedStream {
        pub(crate) fn new(compression_config: CompressionConfig) -> Self {
            Self {
                sequence_id: 0,
                packet_reader: None,
                compression_config,
            }
        }

        pub(crate) async fn read_with<'de, T, C, S: Socket>(
            &mut self,
            byte_len: usize,
            context: C,
            buffered_socket: &mut BufferedSocket<S>,
        ) -> Result<T, Error>
        where
            T: ProtocolDecode<'de, C>,
        {
            let mut result_buffer = BytesMut::with_capacity(byte_len);
            while result_buffer.len() != byte_len {
                let compressed_packet_reader = match self.packet_reader.as_mut() {
                    None => {
                        let packet_reader =
                            CompressedPacketReader::new(buffered_socket, &self.compression_config)
                                .await?;
                        self.sequence_id = packet_reader.sequence_id.wrapping_add(1);
                        self.packet_reader = Some(packet_reader);
                        self.packet_reader.as_mut().unwrap()
                    }
                    Some(p) => p,
                };

                let required_bytes_count = byte_len.saturating_sub(result_buffer.len());
                let chunk = compressed_packet_reader
                    .read(buffered_socket, required_bytes_count)
                    .await?;
                result_buffer.put_slice(&chunk);

                if !compressed_packet_reader.is_available() {
                    self.packet_reader = None
                }
            }

            T::decode_with(result_buffer.freeze(), context)
        }

        pub(crate) async fn write_with<'en, T, C, S: Socket>(
            &mut self,
            packet: T,
            context: C,
            buffered_socket: &mut BufferedSocket<S>,
        ) -> Result<(), Error>
        where
            T: ProtocolEncode<'en, C>,
        {
            self.sequence_id = 0;
            let mut uncompressed_payload = Vec::with_capacity(0xFF_FF_FF);
            packet.encode_with(&mut uncompressed_payload, context)?;

            let mut uncompressed_chunks = uncompressed_payload.chunks(0xFF_FF_FF);
            for uncompressed_chunk in uncompressed_chunks.by_ref() {
                let mut compressed_payload = Vec::with_capacity(uncompressed_chunk.len() + 7);
                Self::add_compressed_packet(
                    self.sequence_id,
                    &self.compression_config,
                    &mut compressed_payload,
                    uncompressed_chunk,
                )
                .await?;

                buffered_socket.write_with(compressed_payload.as_slice(), ())?;

                self.sequence_id = self.sequence_id.wrapping_add(1);
            }

            Ok(())
        }

        pub(crate) fn uncompressed_write_with<'en, T, C, S: Socket>(
            &mut self,
            packet: T,
            context: C,
            buffered_socket: &mut BufferedSocket<S>,
        ) -> Result<(), Error>
        where
            T: ProtocolEncode<'en, C>,
        {
            self.sequence_id = 0;
            let mut uncompressed_payload = Vec::with_capacity(0xFF_FF_FF);
            packet.encode_with(&mut uncompressed_payload, context)?;

            let mut uncompressed_chunks = uncompressed_payload.chunks(0xFF_FF_FF);
            for uncompressed_chunk in uncompressed_chunks.by_ref() {
                let mut header = Vec::with_capacity(7);
                header.put_uint_le(uncompressed_chunk.len() as u64, 3);
                header.put_u8(self.sequence_id);
                header.put_uint_le(0, 3);

                buffered_socket.write_with(header.as_slice(), ())?;
                buffered_socket.write_with(uncompressed_chunk, ())?;

                self.sequence_id = self.sequence_id.wrapping_add(1);
            }

            Ok(())
        }

        async fn add_compressed_packet(
            sequence_id: u8,
            compression: &CompressionConfig,
            compressed_chunk: &mut Vec<u8>,
            uncompressed_chunk: &[u8],
        ) -> Result<(), Error> {
            compressed_chunk.extend_from_slice(&[0; 7]);

            let compressed_payload_length =
                Self::compress_chunk(compression, compressed_chunk, uncompressed_chunk).await?;

            let mut header = &mut compressed_chunk[0..7];
            header.put_uint_le(compressed_payload_length as u64, 3);
            header.put_u8(sequence_id);
            header.put_uint_le(uncompressed_chunk.len() as u64, 3);

            Ok(())
        }

        async fn compress_chunk(
            compression: &CompressionConfig,
            output: &mut Vec<u8>,
            uncompressed_chunk: &[u8],
        ) -> Result<usize, Error> {
            let offset = output.len();
            let mut cursor = Cursor::new(output);
            cursor.set_position(offset as u64);

            let mut encoder = Encoder::new(compression, cursor)?;

            for chunk in uncompressed_chunk.chunks(encoder.get_chunk_size()) {
                encoder.write_all(chunk)?;
                yield_now().await;
            }
            let cursor = encoder.finish()?;
            Ok(cursor.get_ref().len().saturating_sub(offset))
        }
    }

    enum Encoder<'en> {
        #[cfg(feature = "zlib-compression")]
        Zlib(ZlibEncoder<Cursor<&'en mut Vec<u8>>>, u8),
        #[cfg(feature = "zstd-compression")]
        Zstd(ZstdEncoder<'en, Cursor<&'en mut Vec<u8>>>, u8),
    }

    impl<'en> Encoder<'en> {
        fn new(
            compression_config: &CompressionConfig,
            cursor: Cursor<&'en mut Vec<u8>>,
        ) -> Result<Encoder<'en>, Error> {
            let encoder = match compression_config {
                #[cfg(feature = "zlib-compression")]
                CompressionConfig(Compression::Zlib, level) => Encoder::Zlib(
                    ZlibEncoder::new(cursor, ZlibCompression::new(*level as u32)),
                    *level,
                ),
                #[cfg(feature = "zstd-compression")]
                CompressionConfig(Compression::Zstd, level) => {
                    Encoder::Zstd(ZstdEncoder::new(cursor, *level as i32)?, *level)
                }
            };
            Ok(encoder)
        }

        fn write_all(&mut self, buf: &'en [u8]) -> Result<(), Error> {
            match self {
                #[cfg(feature = "zlib-compression")]
                Encoder::Zlib(encoder, _) => encoder.write_all(buf)?,
                #[cfg(feature = "zstd-compression")]
                Encoder::Zstd(encoder, _) => encoder.write_all(buf)?,
            }
            Ok(())
        }

        fn finish(self) -> Result<Cursor<&'en mut Vec<u8>>, Error> {
            let cursor = match self {
                #[cfg(feature = "zlib-compression")]
                Encoder::Zlib(encoder, _) => encoder.finish()?,
                #[cfg(feature = "zstd-compression")]
                Encoder::Zstd(encoder, _) => encoder.finish()?,
            };
            Ok(cursor)
        }

        // Chunk size is chosen based on lzbench benchmarks:
        // https://github.com/inikep/lzbench?tab=readme-ov-file#benchmarks
        // The target is to keep runtime under 50 ms.
        fn get_chunk_size(&self) -> usize {
            match self {
                #[cfg(feature = "zlib-compression")]
                Encoder::Zlib(_, level) => match level {
                    1 => 4 * 1024,
                    2..=4 => 2 * 1024,
                    5..=6 => 1024,
                    _ => 512,
                },
                #[cfg(feature = "zstd-compression")]
                Encoder::Zstd(_, level) => match level {
                    1..=2 => 16 * 1024,
                    3..=4 => 8 * 1024,
                    5..=6 => 4 * 1024,
                    7..=10 => 2 * 1024,
                    11..=12 => 1024,
                    13..=14 => 512,
                    15..=16 => 256,
                    17..=20 => 128,
                    _ => 64,
                },
            }
        }
    }

    struct CompressedPacketReader {
        sequence_id: u8,
        remaining_bytes: usize,
        is_compressed: bool,

        decoder: Decoder,
        input_buffer: Bytes,
        input_buffer_pos: usize,
        output_buffer: BytesMut,
    }

    impl CompressedPacketReader {
        async fn new<S: Socket>(
            buffered_socket: &mut BufferedSocket<S>,
            compression_config: &CompressionConfig,
        ) -> Result<CompressedPacketReader, Error> {
            let mut header: Bytes = buffered_socket.read(7).await?;
            #[allow(clippy::cast_possible_truncation)]
            let compressed_payload_length = header.get_uint_le(3) as usize;
            let sequence_id = header.get_u8();
            #[allow(clippy::cast_possible_truncation)]
            let uncompressed_payload_length = header.get_uint_le(3) as usize;
            let decoder = Decoder::new(compression_config)?;

            Ok(CompressedPacketReader {
                sequence_id,
                remaining_bytes: compressed_payload_length,
                is_compressed: uncompressed_payload_length > 0,
                decoder,

                input_buffer: Bytes::new(),
                input_buffer_pos: 0,
                output_buffer: BytesMut::with_capacity(uncompressed_payload_length),
            })
        }

        fn is_available(&self) -> bool {
            !self.output_buffer.is_empty()
                || self.input_buffer_pos < self.input_buffer.len()
                || self.remaining_bytes > 0
        }

        async fn read<S: Socket>(
            &mut self,
            buffered_socket: &mut BufferedSocket<S>,
            bytes_count: usize,
        ) -> Result<Bytes, Error> {
            let chunk = if self.is_compressed {
                self.decompress(buffered_socket, bytes_count).await?
            } else {
                let available_bytes_count = min(self.remaining_bytes, bytes_count);
                let result: Bytes = buffered_socket.read(available_bytes_count).await?;
                self.remaining_bytes = self.remaining_bytes.saturating_sub(result.len());
                result
            };

            Ok(chunk)
        }

        async fn decompress<S: Socket>(
            &mut self,
            buffered_socket: &mut BufferedSocket<S>,
            output_bytes_count: usize,
        ) -> Result<Bytes, Error> {
            if self.output_buffer.len() >= output_bytes_count {
                return Ok(self.output_buffer.split_to(output_bytes_count).freeze());
            }

            while self.output_buffer.len() < output_bytes_count {
                let mut is_refill_required = self.input_buffer_pos >= self.input_buffer.len();

                if !is_refill_required {
                    let input = &self.input_buffer[self.input_buffer_pos..];
                    let (consumed_bytes_total_count, produced_bytes_total_count) =
                        self.decoder.decompress(input, &mut self.output_buffer)?;

                    self.input_buffer_pos += consumed_bytes_total_count;

                    if produced_bytes_total_count == 0 {
                        is_refill_required = true;
                    }
                }

                if is_refill_required {
                    if self.remaining_bytes == 0 {
                        break;
                    }
                    let available_bytes = min(self.remaining_bytes, self.decoder.get_chunk_size());

                    self.input_buffer = buffered_socket.read(available_bytes).await?;
                    self.input_buffer_pos = 0;
                    self.remaining_bytes =
                        self.remaining_bytes.saturating_sub(self.input_buffer.len());

                    if self.input_buffer.is_empty() {
                        return Err(err_protocol!("Compressed input ended unexpectedly"));
                    }
                }
            }

            let available_bytes = min(self.output_buffer.len(), output_bytes_count);
            Ok(self.output_buffer.split_to(available_bytes).freeze())
        }
    }

    enum Decoder {
        #[cfg(feature = "zlib-compression")]
        Zlib(ZlibDecompressor),
        #[cfg(feature = "zstd-compression")]
        Zstd(ZstdDecoder<'static>),
    }
    impl Decoder {
        // Chunk size is chosen based on lzbench benchmarks:
        // https://github.com/inikep/lzbench?tab=readme-ov-file#benchmarks
        // The target is to keep runtime under 50 ms.
        fn get_chunk_size(&self) -> usize {
            match self {
                #[cfg(feature = "zlib-compression")]
                Decoder::Zlib(_) => 16 * 1024,
                #[cfg(feature = "zstd-compression")]
                Decoder::Zstd(_) => 32 * 1024,
            }
        }

        fn new(compression_config: &CompressionConfig) -> Result<Self, Error> {
            let decoder = match compression_config.0 {
                #[cfg(feature = "zlib-compression")]
                Compression::Zlib => Decoder::Zlib(ZlibDecompressor::new(true)),
                #[cfg(feature = "zstd-compression")]
                Compression::Zstd => Decoder::Zstd(ZstdDecoder::new()?),
            };
            Ok(decoder)
        }

        fn decompress(
            &mut self,
            input: &[u8],
            output: &mut BytesMut,
        ) -> Result<(usize, usize), Error> {
            let mut produced_bytes_total_count = 0;
            let mut consumed_bytes_total_count = 0;

            match self {
                #[cfg(feature = "zlib-compression")]
                Decoder::Zlib(decoder) => {
                    let mut output_buffer = [0u8; 16 * 1024];
                    while consumed_bytes_total_count < input.len() {
                        let consumed_bytes_count_before = decoder.total_in();
                        let produced_bytes_count_before = decoder.total_out();

                        let status = decoder
                            .decompress(
                                &input[consumed_bytes_total_count..],
                                &mut output_buffer,
                                FlushDecompress::None,
                            )
                            .map_err(|e| err_protocol!("Decompression error: {}", e))?;

                        #[allow(clippy::cast_possible_truncation)]
                        let consumed_bytes_count =
                            (decoder.total_in() - consumed_bytes_count_before) as usize;
                        #[allow(clippy::cast_possible_truncation)]
                        let produced_bytes_count =
                            (decoder.total_out() - produced_bytes_count_before) as usize;

                        if produced_bytes_count > 0 {
                            output.extend_from_slice(&output_buffer[..produced_bytes_count]);
                        }

                        consumed_bytes_total_count += consumed_bytes_count;
                        produced_bytes_total_count += produced_bytes_count;

                        match status {
                            // Not enough input data to continue decompression
                            Status::BufError => break,
                            Status::StreamEnd => {
                                if consumed_bytes_total_count < input.len() {
                                    return Err(err_protocol!("Unexpected stream end"));
                                } else {
                                    break;
                                }
                            }
                            Status::Ok => {}
                        }
                    }
                }
                #[cfg(feature = "zstd-compression")]
                Decoder::Zstd(decoder) => {
                    let mut input_chunk = input;
                    let mut output_buffer = [0u8; 16 * 1024];

                    while !input_chunk.is_empty() {
                        let mut in_buf = InBuffer::around(input_chunk);
                        let mut out_buf = OutBuffer::around(&mut output_buffer[..]);

                        let result = decoder.run(&mut in_buf, &mut out_buf)?;

                        let consumed_bytes_count = in_buf.pos();
                        let produced_bytes_count = out_buf.pos();

                        input_chunk = &input_chunk[consumed_bytes_count..];

                        if produced_bytes_count > 0 {
                            output.extend_from_slice(&output_buffer[..produced_bytes_count]);
                        }

                        consumed_bytes_total_count += consumed_bytes_count;
                        produced_bytes_total_count += produced_bytes_count;

                        // No progress made; waiting for the next input chunk
                        if consumed_bytes_count == 0 && produced_bytes_count == 0 {
                            break;
                        }

                        if result == 0 && !input_chunk.is_empty() {
                            return Err(err_protocol!("Unexpected stream end"));
                        }
                    }
                }
            };

            Ok((consumed_bytes_total_count, produced_bytes_total_count))
        }
    }
}
