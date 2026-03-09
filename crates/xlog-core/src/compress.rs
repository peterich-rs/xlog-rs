use std::io::{Read, Write};

use flate2::Compression;
use thiserror::Error;

#[derive(Debug, Error)]
/// Errors raised by the compression and decompression helpers.
pub enum CompressError {
    /// Raw-deflate compression failed.
    #[error("zlib compress failed: {0}")]
    Zlib(String),
    /// zstd compression failed.
    #[error("zstd compress failed: {0}")]
    Zstd(String),
    /// Raw-deflate decompression failed.
    #[error("zlib decompress failed: {0}")]
    ZlibDecompress(String),
    /// zstd frame decompression failed.
    #[error("zstd decompress failed: {0}")]
    ZstdDecompress(String),
}

/// Stateful compressor used by the async writer to emit one logical stream.
pub trait StreamCompressor {
    /// Compresses one input chunk and appends newly emitted bytes to `output`.
    fn compress_chunk(&mut self, input: &[u8], output: &mut Vec<u8>) -> Result<(), CompressError>;
    /// Finalizes the stream and appends any remaining encoded bytes to `output`.
    fn flush(&mut self, output: &mut Vec<u8>) -> Result<(), CompressError>;
}

/// Raw-deflate stream compressor compatible with Mars zlib settings.
pub struct ZlibStreamCompressor {
    inner: flate2::write::DeflateEncoder<Vec<u8>>,
    emitted: usize,
}

impl ZlibStreamCompressor {
    /// Creates a raw-deflate stream compressor with the requested compression level.
    pub fn new(level: i32) -> Self {
        let level = level.clamp(0, 9) as u32;
        Self {
            inner: flate2::write::DeflateEncoder::new(Vec::new(), Compression::new(level)),
            emitted: 0,
        }
    }
}

impl Default for ZlibStreamCompressor {
    fn default() -> Self {
        Self::new(9)
    }
}

impl StreamCompressor for ZlibStreamCompressor {
    fn compress_chunk(&mut self, input: &[u8], output: &mut Vec<u8>) -> Result<(), CompressError> {
        self.inner
            .write_all(input)
            .map_err(|e| CompressError::Zlib(e.to_string()))?;
        self.inner
            .flush()
            .map_err(|e| CompressError::Zlib(e.to_string()))?;
        let encoded = self.inner.get_ref();
        if encoded.len() > self.emitted {
            output.extend_from_slice(&encoded[self.emitted..]);
            self.emitted = encoded.len();
        }
        Ok(())
    }

    fn flush(&mut self, output: &mut Vec<u8>) -> Result<(), CompressError> {
        self.inner
            .try_finish()
            .map_err(|e| CompressError::Zlib(e.to_string()))?;
        let encoded = self.inner.get_ref();
        if encoded.len() > self.emitted {
            output.extend_from_slice(&encoded[self.emitted..]);
            self.emitted = encoded.len();
        }
        Ok(())
    }
}

/// zstd chunk compressor.
///
/// Each chunk is encoded as a zstd frame and can be decoded from concatenated frames.
pub struct ZstdChunkCompressor {
    level: i32,
}

impl ZstdChunkCompressor {
    /// Creates a compressor that encodes each input chunk as a separate zstd frame.
    pub fn new(level: i32) -> Self {
        Self { level }
    }
}

impl StreamCompressor for ZstdChunkCompressor {
    fn compress_chunk(&mut self, input: &[u8], output: &mut Vec<u8>) -> Result<(), CompressError> {
        let compressed = zstd::bulk::compress(input, self.level)
            .map_err(|e| CompressError::Zstd(e.to_string()))?;
        output.extend_from_slice(&compressed);
        Ok(())
    }

    fn flush(&mut self, _output: &mut Vec<u8>) -> Result<(), CompressError> {
        Ok(())
    }
}

/// Streaming zstd compressor compatible with Mars async path.
///
/// Uses one compression stream with `windowLog=16`, flushing each chunk and
/// emitting the final frame epilogue in `flush()`.
pub struct ZstdStreamCompressor {
    inner: Option<zstd::stream::write::Encoder<'static, Vec<u8>>>,
    emitted: usize,
}

impl ZstdStreamCompressor {
    /// Creates a streaming zstd compressor configured to match Mars async settings.
    pub fn new(level: i32) -> Result<Self, CompressError> {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), level)
            .map_err(|e| CompressError::Zstd(e.to_string()))?;
        encoder
            .window_log(16)
            .map_err(|e| CompressError::Zstd(e.to_string()))?;
        Ok(Self {
            inner: Some(encoder),
            emitted: 0,
        })
    }
}

impl StreamCompressor for ZstdStreamCompressor {
    fn compress_chunk(&mut self, input: &[u8], output: &mut Vec<u8>) -> Result<(), CompressError> {
        let Some(encoder) = self.inner.as_mut() else {
            return Err(CompressError::Zstd(
                "zstd stream compressor already finished".to_string(),
            ));
        };
        encoder
            .write_all(input)
            .map_err(|e| CompressError::Zstd(e.to_string()))?;
        encoder
            .flush()
            .map_err(|e| CompressError::Zstd(e.to_string()))?;
        let encoded = encoder.get_ref();
        if encoded.len() > self.emitted {
            output.extend_from_slice(&encoded[self.emitted..]);
            self.emitted = encoded.len();
        }
        Ok(())
    }

    fn flush(&mut self, output: &mut Vec<u8>) -> Result<(), CompressError> {
        let Some(encoder) = self.inner.take() else {
            return Ok(());
        };
        let encoded = encoder
            .finish()
            .map_err(|e| CompressError::Zstd(e.to_string()))?;
        if encoded.len() > self.emitted {
            output.extend_from_slice(&encoded[self.emitted..]);
            self.emitted = encoded.len();
        }
        Ok(())
    }
}

/// Decompresses raw-deflate data produced by the zlib stream compressor.
pub fn decompress_raw_zlib(input: &[u8]) -> Result<Vec<u8>, CompressError> {
    let mut decoder = flate2::read::DeflateDecoder::new(input);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| CompressError::ZlibDecompress(e.to_string()))?;
    Ok(out)
}

/// Decompresses one or more concatenated zstd frames into a single byte buffer.
pub fn decompress_zstd_frames(input: &[u8]) -> Result<Vec<u8>, CompressError> {
    let mut reader = std::io::Cursor::new(input);
    let mut decoder = zstd::stream::Decoder::new(&mut reader)
        .map_err(|e| CompressError::ZstdDecompress(e.to_string()))?;
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| CompressError::ZstdDecompress(e.to_string()))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{
        decompress_raw_zlib, decompress_zstd_frames, CompressError, StreamCompressor,
        ZstdChunkCompressor, ZstdStreamCompressor,
    };

    #[test]
    fn zstd_chunk_roundtrips_concatenated_frames() {
        let mut compressor = ZstdChunkCompressor::new(3);
        let mut encoded = Vec::new();

        compressor.compress_chunk(b"mars", &mut encoded).unwrap();
        compressor.compress_chunk(b" xlog", &mut encoded).unwrap();
        compressor.flush(&mut encoded).unwrap();

        assert_eq!(decompress_zstd_frames(&encoded).unwrap(), b"mars xlog");
    }

    #[test]
    fn zstd_stream_rejects_compress_after_finish_and_double_flush_is_noop() {
        let mut compressor = ZstdStreamCompressor::new(3).unwrap();
        let mut encoded = Vec::new();

        compressor.compress_chunk(b"hello", &mut encoded).unwrap();
        compressor.flush(&mut encoded).unwrap();
        let finished = encoded.clone();

        let err = compressor
            .compress_chunk(b"world", &mut encoded)
            .unwrap_err();
        assert!(
            matches!(err, CompressError::Zstd(message) if message.contains("already finished"))
        );

        compressor.flush(&mut encoded).unwrap();
        assert_eq!(encoded, finished);
    }

    #[test]
    fn invalid_decompression_maps_to_specific_error_variants() {
        assert!(matches!(
            decompress_raw_zlib(b"not-a-deflate-stream"),
            Err(CompressError::ZlibDecompress(_))
        ));
        assert!(matches!(
            decompress_zstd_frames(b"not-a-zstd-frame"),
            Err(CompressError::ZstdDecompress(_))
        ));
    }
}
