use thiserror::Error;

use crate::compress::{
    CompressError, ZlibLevel, ZlibStreamCompressor, ZstdLevel, ZstdStreamCompressor,
};
use crate::crypto::Crypt;
use crate::protocol::{
    AppendMode, BlockMagic, CompressionKind, Hour, LogHeader, SeqGenerator, HEADER_LEN, MAGIC_END,
};

#[derive(Debug, Error)]
/// Errors raised while sealing an xlog frame.
pub enum FrameError {
    /// The compressor rejected the chunk or could not be constructed.
    #[error(transparent)]
    Compress(#[from] CompressError),
    /// A sync writer was asked to continue an async stream.
    #[error("sync frame writer has no open stream")]
    NotAsync,
    /// `finish` or `push_line` ran before [`FrameWriter::open`].
    #[error("async frame is not open")]
    NotOpen,
    /// The plaintext length does not fit in the header's `u32` field.
    #[error("payload length does not fit in u32")]
    LengthOverflow,
}

enum AsyncEncoder {
    Zlib(ZlibStreamCompressor),
    Zstd(ZstdStreamCompressor),
}

impl AsyncEncoder {
    fn write_plain(&mut self, input: &[u8], output: &mut Vec<u8>) -> Result<(), CompressError> {
        match self {
            Self::Zlib(encoder) => encoder.write_plain(input, output),
            Self::Zstd(encoder) => encoder.write_plain(input, output),
        }
    }

    fn finish_into(&mut self, output: &mut Vec<u8>) -> Result<(), CompressError> {
        match self {
            Self::Zlib(encoder) => encoder.finish_into(output),
            Self::Zstd(encoder) => encoder.finish_into(output),
        }
    }
}

struct OpenBlock {
    header: LogHeader,
    payload_len: usize,
    crypt_tail: Vec<u8>,
}

struct AsyncState {
    family: CompressionKind,
    zlib_level: Option<ZlibLevel>,
    zstd_level: Option<ZstdLevel>,
    crypt: Crypt,
    encoder: Option<AsyncEncoder>,
    scratch: Vec<u8>,
    open: Option<OpenBlock>,
    line_count: u64,
    raw_input_bytes: u64,
    flush_epoch: u64,
}

enum FrameKind {
    Sync {
        family: CompressionKind,
        crypt: Crypt,
    },
    Async(Box<AsyncState>),
}

/// Turns formatted log lines into xlog blocks.
///
/// Sync frames copy the line into the payload. Async frames own the compressor
/// and the TEA tail and emit incremental payload bytes for the mmap buffer.
pub struct FrameWriter {
    kind: FrameKind,
}

impl FrameWriter {
    /// Sync writer. Payload bytes are the original line.
    pub fn sync(family: CompressionKind, crypt: Crypt) -> Self {
        Self {
            kind: FrameKind::Sync { family, crypt },
        }
    }

    /// Async zlib stream.
    pub fn async_zlib(level: ZlibLevel, crypt: Crypt) -> Self {
        Self {
            kind: FrameKind::Async(Box::new(empty_async(
                CompressionKind::Zlib,
                Some(level),
                None,
                crypt,
            ))),
        }
    }

    /// Async zstd stream. Encoder construction is checked when [`Self::open`] runs.
    pub fn async_zstd(level: ZstdLevel, crypt: Crypt) -> Self {
        Self {
            kind: FrameKind::Async(Box::new(empty_async(
                CompressionKind::Zstd,
                None,
                Some(level),
                crypt,
            ))),
        }
    }

    /// Writes one complete sync block into `out`.
    pub fn seal_sync_line(
        &self,
        line: &[u8],
        hour: Hour,
        out: &mut Vec<u8>,
    ) -> Result<(), FrameError> {
        let FrameKind::Sync { family, crypt } = &self.kind else {
            return Err(FrameError::NotAsync);
        };
        let len = u32::try_from(line.len()).map_err(|_| FrameError::LengthOverflow)?;
        let header = LogHeader {
            magic: BlockMagic::new(*family, AppendMode::Sync, crypt.tea_key().is_some()),
            seq: SeqGenerator::sync_seq(),
            begin_hour: hour,
            end_hour: hour,
            len,
            client_pubkey: crypt.client_pubkey(),
        };
        out.clear();
        out.reserve(HEADER_LEN + line.len() + 1);
        out.extend_from_slice(&header.encode());
        out.extend_from_slice(line);
        out.push(MAGIC_END);
        Ok(())
    }

    /// Whether an async block is currently open.
    pub fn is_open(&self) -> bool {
        matches!(&self.kind, FrameKind::Async(state) if state.open.is_some())
    }

    /// Opens a new async block and returns the header that must be written first.
    pub fn open(
        &mut self,
        hour: Hour,
        seq: u16,
        flush_epoch: u64,
    ) -> Result<LogHeader, FrameError> {
        let FrameKind::Async(state) = &mut self.kind else {
            return Err(FrameError::NotAsync);
        };
        state.encoder = Some(new_encoder(state)?);
        state.line_count = 0;
        state.raw_input_bytes = 0;
        state.flush_epoch = flush_epoch;
        let header = LogHeader {
            magic: BlockMagic::new(
                state.family,
                AppendMode::Async,
                state.crypt.tea_key().is_some(),
            ),
            seq,
            begin_hour: hour,
            end_hour: hour,
            len: 0,
            client_pubkey: state.crypt.client_pubkey(),
        };
        state.open = Some(OpenBlock {
            header,
            payload_len: 0,
            crypt_tail: Vec::with_capacity(8),
        });
        Ok(header)
    }

    /// Compresses and optionally encrypts one line.
    ///
    /// `encoded` receives the bytes to append. The returned value is how many
    /// previous payload bytes the mmap writer must truncate first, which is the
    /// unaligned TEA tail from the previous chunk.
    pub fn push_line(
        &mut self,
        line: &[u8],
        hour: Hour,
        encoded: &mut Vec<u8>,
    ) -> Result<usize, FrameError> {
        self.with_compressed(encoded, |encoder, scratch| {
            encoder.write_plain(line, scratch)
        })?;
        let truncate = self.apply_compressed(hour, encoded)?;
        if let FrameKind::Async(state) = &mut self.kind {
            state.line_count = state.line_count.saturating_add(1);
            state.raw_input_bytes = state.raw_input_bytes.saturating_add(line.len() as u64);
        }
        Ok(truncate)
    }

    /// Finishes the compression stream.
    ///
    /// `encoded` receives any final payload bytes. An empty buffer means the
    /// caller only needs to append the end marker. The returned truncate length
    /// is zero in that case.
    pub fn finish(&mut self, hour: Hour, encoded: &mut Vec<u8>) -> Result<usize, FrameError> {
        self.with_compressed(encoded, |encoder, scratch| encoder.finish_into(scratch))?;
        if encoded.is_empty() {
            if let FrameKind::Async(state) = &mut self.kind {
                if let Some(open) = state.open.as_mut() {
                    open.header.end_hour = hour;
                }
            }
            return Ok(0);
        }
        self.apply_compressed(hour, encoded)
    }

    /// Drops the open async block after it has been sealed or discarded.
    pub fn close(&mut self) {
        if let FrameKind::Async(state) = &mut self.kind {
            state.open = None;
            state.encoder = None;
        }
    }

    /// Bytes currently accounted in the open payload, including a stored TEA tail.
    pub fn payload_len(&self) -> usize {
        match &self.kind {
            FrameKind::Async(state) => state
                .open
                .as_ref()
                .map(|open| open.payload_len)
                .unwrap_or(0),
            FrameKind::Sync { .. } => 0,
        }
    }

    /// Overrides the accounted payload length.
    ///
    /// The high-watermark path uses this when the in-memory length, not a fresh
    /// compression, decides which line is written.
    pub fn set_payload_len(&mut self, len: usize) {
        if let FrameKind::Async(state) = &mut self.kind {
            if let Some(open) = state.open.as_mut() {
                open.payload_len = len;
            }
        }
    }

    /// Lines accepted by [`Self::push_line`] since [`Self::open`].
    pub fn line_count(&self) -> u64 {
        match &self.kind {
            FrameKind::Async(state) => state.line_count,
            FrameKind::Sync { .. } => 0,
        }
    }

    /// Plaintext bytes accepted since [`Self::open`].
    pub fn raw_input_bytes(&self) -> u64 {
        match &self.kind {
            FrameKind::Async(state) => state.raw_input_bytes,
            FrameKind::Sync { .. } => 0,
        }
    }

    /// Flush epoch captured when the current block was opened.
    pub fn flush_epoch(&self) -> u64 {
        match &self.kind {
            FrameKind::Async(state) => state.flush_epoch,
            FrameKind::Sync { .. } => 0,
        }
    }

    fn with_compressed(
        &mut self,
        encoded: &mut Vec<u8>,
        op: impl FnOnce(&mut AsyncEncoder, &mut Vec<u8>) -> Result<(), CompressError>,
    ) -> Result<(), FrameError> {
        let FrameKind::Async(state) = &mut self.kind else {
            return Err(FrameError::NotAsync);
        };
        let Some(mut encoder) = state.encoder.take() else {
            return Err(FrameError::NotOpen);
        };
        let mut scratch = std::mem::take(&mut state.scratch);
        scratch.clear();
        let result = op(&mut encoder, &mut scratch);
        state.encoder = Some(encoder);
        state.scratch = scratch;
        result?;
        encoded.clear();
        encoded.extend_from_slice(&state.scratch);
        Ok(())
    }

    fn apply_compressed(&mut self, hour: Hour, encoded: &mut Vec<u8>) -> Result<usize, FrameError> {
        let FrameKind::Async(state) = &mut self.kind else {
            return Err(FrameError::NotAsync);
        };
        let Some(open) = state.open.as_mut() else {
            return Err(FrameError::NotOpen);
        };
        open.header.end_hour = hour;
        if state.crypt.tea_key().is_none() {
            open.payload_len = open.payload_len.saturating_add(encoded.len());
            return Ok(0);
        }

        let previous_tail_len = open.crypt_tail.len();
        let mut combined = std::mem::take(&mut state.scratch);
        combined.clear();
        combined.extend_from_slice(&open.crypt_tail);
        combined.extend_from_slice(encoded);
        let full_len = combined.len() / 8 * 8;
        if full_len > 0 {
            state
                .crypt
                .encrypt_async_in_place(&mut combined[..full_len]);
        }
        open.payload_len = open
            .payload_len
            .saturating_sub(previous_tail_len)
            .saturating_add(combined.len());
        open.crypt_tail.clear();
        open.crypt_tail.extend_from_slice(&combined[full_len..]);
        encoded.clear();
        encoded.extend_from_slice(&combined);
        state.scratch = combined;
        Ok(previous_tail_len)
    }
}

fn empty_async(
    family: CompressionKind,
    zlib_level: Option<ZlibLevel>,
    zstd_level: Option<ZstdLevel>,
    crypt: Crypt,
) -> AsyncState {
    AsyncState {
        family,
        zlib_level,
        zstd_level,
        crypt,
        encoder: None,
        scratch: Vec::with_capacity(16 * 1024),
        open: None,
        line_count: 0,
        raw_input_bytes: 0,
        flush_epoch: 0,
    }
}

fn new_encoder(state: &AsyncState) -> Result<AsyncEncoder, FrameError> {
    match state.family {
        CompressionKind::Zlib => {
            let level = state.zlib_level.ok_or(FrameError::NotAsync)?;
            Ok(AsyncEncoder::Zlib(ZlibStreamCompressor::new(level)))
        }
        CompressionKind::Zstd => {
            let level = state.zstd_level.ok_or(FrameError::NotAsync)?;
            Ok(AsyncEncoder::Zstd(ZstdStreamCompressor::new(level)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FrameWriter;
    use crate::compress::{decompress_raw_zlib, ZlibLevel};
    use crate::crypto::Crypt;
    use crate::protocol::{BlockMagic, CompressionKind, Hour, HEADER_LEN, MAGIC_END};

    #[test]
    fn sync_block_keeps_plaintext_payload() {
        let writer = FrameWriter::sync(CompressionKind::Zlib, Crypt::off());
        let mut out = Vec::new();
        writer
            .seal_sync_line(b"hello\n", Hour::try_from(3).unwrap(), &mut out)
            .unwrap();
        assert_eq!(out[0], u8::from(BlockMagic::SyncZlib { encrypted: false }));
        assert_eq!(&out[HEADER_LEN..out.len() - 1], b"hello\n");
        assert_eq!(*out.last().unwrap(), MAGIC_END);
    }

    #[test]
    fn async_zlib_roundtrips_two_lines() {
        let mut writer = FrameWriter::async_zlib(ZlibLevel::try_from(6).unwrap(), Crypt::off());
        let header = writer.open(Hour::try_from(1).unwrap(), 1, 0).unwrap();
        let mut encoded = Vec::new();
        assert_eq!(
            writer
                .push_line(b"mars", Hour::try_from(1).unwrap(), &mut encoded)
                .unwrap(),
            0
        );
        let mut payload = encoded.clone();
        writer
            .push_line(b" xlog", Hour::try_from(1).unwrap(), &mut encoded)
            .unwrap();
        payload.extend_from_slice(&encoded);
        assert_eq!(
            writer
                .finish(Hour::try_from(2).unwrap(), &mut encoded)
                .unwrap(),
            0
        );
        payload.extend_from_slice(&encoded);
        assert_eq!(decompress_raw_zlib(&payload).unwrap(), b"mars xlog");
        assert!(!header.magic.encrypted());
        assert_eq!(writer.line_count(), 2);
    }
}
