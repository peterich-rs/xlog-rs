use std::io::{Read, Write};

use flate2::Compression;
use thiserror::Error;

#[derive(Debug, Error)]
/// Errors raised by the compression and decompression helpers.
pub enum CompressError {
    /// Raw-deflate compression failed.
    #[error("zlib compress failed")]
    Zlib(#[source] std::io::Error),
    /// zstd compression failed.
    #[error("zstd compress failed")]
    Zstd(#[source] std::io::Error),
    /// Raw-deflate decompression failed.
    #[error("zlib decompress failed")]
    ZlibDecompress(#[source] std::io::Error),
    /// zstd frame decompression failed.
    #[error("zstd decompress failed")]
    ZstdDecompress(#[source] std::io::Error),
    /// The requested level is outside the algorithm's accepted range.
    #[error("compression level {level} is outside the accepted range")]
    InvalidLevel {
        /// The rejected level.
        level: i32,
    },
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
/// zlib level in `0..=9`.
pub struct ZlibLevel(u8);

impl ZlibLevel {
    /// Return the numeric level.
    pub const fn get(self) -> i32 {
        self.0 as i32
    }
}

impl TryFrom<i32> for ZlibLevel {
    type Error = CompressError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        u8::try_from(value)
            .ok()
            .filter(|level| *level <= 9)
            .map(Self)
            .ok_or(CompressError::InvalidLevel { level: value })
    }
}

impl From<ZlibLevel> for i32 {
    fn from(value: ZlibLevel) -> Self {
        value.get()
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
/// zstd level accepted by the linked zstd library.
pub struct ZstdLevel(i32);

impl ZstdLevel {
    /// Return the numeric level.
    pub const fn get(self) -> i32 {
        self.0
    }
}

impl TryFrom<i32> for ZstdLevel {
    type Error = CompressError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        if zstd::compression_level_range().contains(&value) {
            Ok(Self(value))
        } else {
            Err(CompressError::InvalidLevel { level: value })
        }
    }
}

impl From<ZstdLevel> for i32 {
    fn from(value: ZstdLevel) -> Self {
        value.0
    }
}

/// Raw-deflate stream compressor compatible with Mars zlib settings.
///
/// `write_plain` uses [`Write`] on the underlying encoder. `Write::flush` only
/// emits bytes already produced. [`Self::finish_into`] ends the deflate stream.
pub struct ZlibStreamCompressor {
    inner: flate2::write::DeflateEncoder<Vec<u8>>,
}

impl ZlibStreamCompressor {
    /// Creates a raw-deflate stream compressor at a validated level.
    pub fn new(level: ZlibLevel) -> Self {
        Self {
            inner: flate2::write::DeflateEncoder::new(
                Vec::new(),
                Compression::new(level.get() as u32),
            ),
        }
    }

    /// Compresses one plaintext chunk and appends newly emitted bytes to `output`.
    pub fn write_plain(
        &mut self,
        input: &[u8],
        output: &mut impl Write,
    ) -> Result<(), CompressError> {
        self.inner.write_all(input).map_err(CompressError::Zlib)?;
        self.inner.flush().map_err(CompressError::Zlib)?;
        drain_encoded_bytes(self.inner.get_mut(), output).map_err(CompressError::Zlib)
    }

    /// Ends the deflate stream and appends any remaining encoded bytes.
    pub fn finish_into(&mut self, output: &mut impl Write) -> Result<(), CompressError> {
        self.inner.try_finish().map_err(CompressError::Zlib)?;
        drain_encoded_bytes(self.inner.get_mut(), output).map_err(CompressError::Zlib)
    }
}

impl Default for ZlibStreamCompressor {
    fn default() -> Self {
        Self::new(ZlibLevel::try_from(9).expect("9 is a valid zlib level"))
    }
}

/// zstd chunk compressor.
///
/// Each chunk is encoded as its own zstd frame.
pub struct ZstdChunkCompressor {
    level: ZstdLevel,
}

impl ZstdChunkCompressor {
    /// Creates a compressor that encodes each input chunk as a separate zstd frame.
    pub fn new(level: ZstdLevel) -> Self {
        Self { level }
    }

    /// Encodes `input` as one zstd frame and appends it to `output`.
    pub fn write_plain(
        &mut self,
        input: &[u8],
        output: &mut impl Write,
    ) -> Result<(), CompressError> {
        let compressed = zstd::bulk::compress(input, self.level.get()).map_err(|err| {
            CompressError::Zstd(std::io::Error::new(std::io::ErrorKind::InvalidData, err))
        })?;
        output.write_all(&compressed).map_err(CompressError::Zstd)?;
        Ok(())
    }

    /// Chunk frames are complete after [`Self::write_plain`].
    pub fn finish_into(&mut self, _output: &mut impl Write) -> Result<(), CompressError> {
        Ok(())
    }
}

/// Streaming zstd compressor compatible with Mars async path.
///
/// Uses one compression stream with `windowLog=16`. [`Self::write_plain`] flushes
/// each chunk. [`Self::finish_into`] writes the frame epilogue and consumes the
/// encoder.
pub struct ZstdStreamCompressor {
    inner: Option<zstd::stream::write::Encoder<'static, Vec<u8>>>,
}

impl ZstdStreamCompressor {
    /// Creates a streaming zstd compressor configured to match Mars async settings.
    pub fn new(level: ZstdLevel) -> Result<Self, CompressError> {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), level.get())
            .map_err(CompressError::Zstd)?;
        encoder.window_log(16).map_err(CompressError::Zstd)?;
        Ok(Self {
            inner: Some(encoder),
        })
    }

    /// Compresses one plaintext chunk and appends newly emitted bytes to `output`.
    pub fn write_plain(
        &mut self,
        input: &[u8],
        output: &mut impl Write,
    ) -> Result<(), CompressError> {
        let Some(encoder) = self.inner.as_mut() else {
            return Err(CompressError::Zstd(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "zstd stream compressor already finished",
            )));
        };
        encoder.write_all(input).map_err(CompressError::Zstd)?;
        encoder.flush().map_err(CompressError::Zstd)?;
        drain_encoded_bytes(encoder.get_mut(), output).map_err(CompressError::Zstd)
    }

    /// Ends the zstd frame and appends any remaining encoded bytes.
    pub fn finish_into(&mut self, output: &mut impl Write) -> Result<(), CompressError> {
        let Some(encoder) = self.inner.take() else {
            return Ok(());
        };
        let mut encoded = encoder.finish().map_err(CompressError::Zstd)?;
        drain_encoded_bytes(&mut encoded, output).map_err(CompressError::Zstd)
    }
}

fn drain_encoded_bytes(encoded: &mut Vec<u8>, output: &mut impl Write) -> std::io::Result<()> {
    if encoded.is_empty() {
        return Ok(());
    }
    output.write_all(encoded)?;
    encoded.clear();
    Ok(())
}

/// Decompresses raw-deflate data produced by the zlib stream compressor.
pub fn decompress_raw_zlib(input: &[u8]) -> Result<Vec<u8>, CompressError> {
    let mut decoder = flate2::read::DeflateDecoder::new(input);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(CompressError::ZlibDecompress)?;
    Ok(out)
}

/// Decompresses one or more concatenated zstd frames into a single byte buffer.
pub fn decompress_zstd_frames(input: &[u8]) -> Result<Vec<u8>, CompressError> {
    let mut reader = std::io::Cursor::new(input);
    let mut decoder =
        zstd::stream::Decoder::new(&mut reader).map_err(CompressError::ZstdDecompress)?;
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(CompressError::ZstdDecompress)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::{
        decompress_raw_zlib, decompress_zstd_frames, CompressError, ZlibLevel,
        ZlibStreamCompressor, ZstdChunkCompressor, ZstdLevel, ZstdStreamCompressor,
    };

    #[test]
    fn zlib_stream_roundtrips_incremental_chunks() {
        let mut compressor = ZlibStreamCompressor::new(ZlibLevel::try_from(6).unwrap());
        let mut encoded = Vec::new();

        compressor.write_plain(b"mars", &mut encoded).unwrap();
        compressor.write_plain(b" xlog", &mut encoded).unwrap();
        compressor.finish_into(&mut encoded).unwrap();

        assert_eq!(decompress_raw_zlib(&encoded).unwrap(), b"mars xlog");
    }

    #[test]
    fn stream_compressors_release_emitted_bytes_after_each_chunk() {
        let mut zlib = ZlibStreamCompressor::new(ZlibLevel::try_from(6).unwrap());
        let mut zlib_out = Vec::new();
        zlib.write_plain(b"mars", &mut zlib_out).unwrap();
        assert!(zlib.inner.get_ref().is_empty());
        zlib.write_plain(b" xlog", &mut zlib_out).unwrap();
        assert!(zlib.inner.get_ref().is_empty());

        let mut zstd = ZstdStreamCompressor::new(ZstdLevel::try_from(3).unwrap()).unwrap();
        let mut zstd_out = Vec::new();
        zstd.write_plain(b"mars", &mut zstd_out).unwrap();
        assert!(zstd.inner.as_ref().unwrap().get_ref().is_empty());
        zstd.write_plain(b" xlog", &mut zstd_out).unwrap();
        assert!(zstd.inner.as_ref().unwrap().get_ref().is_empty());
    }

    #[test]
    fn zstd_chunk_roundtrips_concatenated_frames() {
        let mut compressor = ZstdChunkCompressor::new(ZstdLevel::try_from(3).unwrap());
        let mut encoded = Vec::new();

        compressor.write_plain(b"mars", &mut encoded).unwrap();
        compressor.write_plain(b" xlog", &mut encoded).unwrap();
        compressor.finish_into(&mut encoded).unwrap();

        assert_eq!(decompress_zstd_frames(&encoded).unwrap(), b"mars xlog");
    }

    #[test]
    fn zstd_stream_rejects_compress_after_finish_and_double_flush_is_noop() {
        let mut compressor = ZstdStreamCompressor::new(ZstdLevel::try_from(3).unwrap()).unwrap();
        let mut encoded = Vec::new();

        compressor.write_plain(b"hello", &mut encoded).unwrap();
        compressor.finish_into(&mut encoded).unwrap();
        let finished = encoded.clone();

        let err = compressor.write_plain(b"world", &mut encoded).unwrap_err();
        assert!(
            matches!(err, CompressError::Zstd(source) if source.to_string().contains("already finished"))
        );

        compressor.finish_into(&mut encoded).unwrap();
        assert_eq!(encoded, finished);
    }

    #[test]
    fn invalid_decompression_maps_to_specific_error_variants() {
        let zlib_err = decompress_raw_zlib(b"not-a-deflate-stream").unwrap_err();
        assert!(matches!(zlib_err, CompressError::ZlibDecompress(_)));
        assert!(zlib_err.source().is_some());
        let zstd_err = decompress_zstd_frames(b"not-a-zstd-frame").unwrap_err();
        assert!(matches!(zstd_err, CompressError::ZstdDecompress(_)));
        assert!(zstd_err.source().is_some());
    }

    #[test]
    fn levels_reject_values_outside_the_algorithm_range() {
        assert!(matches!(
            ZlibLevel::try_from(10),
            Err(CompressError::InvalidLevel { level: 10 })
        ));
        assert!(ZlibLevel::try_from(6).is_ok());
        assert!(ZstdLevel::try_from(3).is_ok());
        assert!(ZstdLevel::try_from(i32::MAX).is_err());
    }
}
