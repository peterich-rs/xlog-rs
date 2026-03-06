use std::io::{Read, Write};

use flate2::Compression;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CompressError {
    #[error("zlib compress failed: {0}")]
    Zlib(String),
    #[error("zstd compress failed: {0}")]
    Zstd(String),
    #[error("zlib decompress failed: {0}")]
    ZlibDecompress(String),
    #[error("zstd decompress failed: {0}")]
    ZstdDecompress(String),
}

pub trait StreamCompressor {
    fn compress_chunk(&mut self, input: &[u8], output: &mut Vec<u8>) -> Result<(), CompressError>;
    fn flush(&mut self, output: &mut Vec<u8>) -> Result<(), CompressError>;
}

/// Raw-deflate stream compressor compatible with Mars zlib settings.
pub struct ZlibStreamCompressor {
    inner: flate2::write::DeflateEncoder<Vec<u8>>,
    emitted: usize,
}

impl ZlibStreamCompressor {
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

pub fn decompress_raw_zlib(input: &[u8]) -> Result<Vec<u8>, CompressError> {
    let mut decoder = flate2::read::DeflateDecoder::new(input);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| CompressError::ZlibDecompress(e.to_string()))?;
    Ok(out)
}

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
