use std::sync::atomic::{AtomicU16, Ordering};

use thiserror::Error;

/// Byte length of one encoded xlog header.
pub const HEADER_LEN: usize = 1 + 2 + 1 + 1 + 4 + 64;
/// Byte offset of the encoded end-hour field.
pub const HEADER_END_HOUR_OFFSET: usize = 4;
/// Byte offset of the encoded payload-length field.
pub const HEADER_LEN_OFFSET: usize = 5;
/// Byte offset of the encoded client public key field.
pub const HEADER_CLIENT_PUBKEY_OFFSET: usize = 9;
/// Byte length of the trailing end marker.
pub const TAILER_LEN: usize = 1;
/// Tail marker terminating a complete xlog block.
pub const MAGIC_END: u8 = 0x00;

/// Magic byte for sync + zlib + encrypted blocks.
pub const MAGIC_SYNC_ZLIB_START: u8 = 0x06;
/// Magic byte for sync + zlib + plaintext blocks.
pub const MAGIC_SYNC_NO_CRYPT_ZLIB_START: u8 = 0x08;
/// Magic byte for async + zlib + encrypted blocks.
pub const MAGIC_ASYNC_ZLIB_START: u8 = 0x07;
/// Magic byte for async + zlib + plaintext blocks.
pub const MAGIC_ASYNC_NO_CRYPT_ZLIB_START: u8 = 0x09;

/// Magic byte for sync + zstd + encrypted blocks.
pub const MAGIC_SYNC_ZSTD_START: u8 = 0x0A;
/// Magic byte for sync + zstd + plaintext blocks.
pub const MAGIC_SYNC_NO_CRYPT_ZSTD_START: u8 = 0x0B;
/// Magic byte for async + zstd + encrypted blocks.
pub const MAGIC_ASYNC_ZSTD_START: u8 = 0x0C;
/// Magic byte for async + zstd + plaintext blocks.
pub const MAGIC_ASYNC_NO_CRYPT_ZSTD_START: u8 = 0x0D;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
/// Compression family encoded into the xlog magic byte.
pub enum CompressionKind {
    /// zlib-framed payloads.
    Zlib,
    /// zstd-framed payloads.
    Zstd,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
/// Append path encoded into the xlog magic byte.
pub enum AppendMode {
    /// Synchronous write path.
    Sync,
    /// Asynchronous buffered write path.
    Async,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
/// Local hour stored in an xlog header.
pub struct Hour(u8);

impl Hour {
    /// Return the wire byte for this hour.
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for Hour {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value <= 23 {
            Ok(Self(value))
        } else {
            Err(ProtocolError::InvalidHour(value))
        }
    }
}

impl From<Hour> for u8 {
    fn from(value: Hour) -> Self {
        value.0
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
/// Closed set of modern xlog start magic values.
///
/// Sync variants record the configured compressor and key slot. Their payload
/// bytes stay plaintext. Async variants are the ones that compress, and only
/// the encrypted async variants run TEA.
pub enum BlockMagic {
    /// Sync zlib magic. Payload is plaintext.
    SyncZlib { encrypted: bool },
    /// Sync zstd magic. Payload is plaintext.
    SyncZstd { encrypted: bool },
    /// Async zlib magic. Payload is a raw deflate stream, optionally TEA-encrypted.
    AsyncZlib { encrypted: bool },
    /// Async zstd magic. Payload is a zstd stream, optionally TEA-encrypted.
    AsyncZstd { encrypted: bool },
}

impl BlockMagic {
    /// Build the magic for one `(compression, append mode, crypto slot)` tuple.
    pub const fn new(compress: CompressionKind, mode: AppendMode, encrypted: bool) -> Self {
        match (compress, mode, encrypted) {
            (CompressionKind::Zlib, AppendMode::Sync, encrypted) => Self::SyncZlib { encrypted },
            (CompressionKind::Zstd, AppendMode::Sync, encrypted) => Self::SyncZstd { encrypted },
            (CompressionKind::Zlib, AppendMode::Async, encrypted) => Self::AsyncZlib { encrypted },
            (CompressionKind::Zstd, AppendMode::Async, encrypted) => Self::AsyncZstd { encrypted },
        }
    }

    /// Compression family recorded by this magic byte.
    pub const fn family(self) -> CompressionKind {
        match self {
            Self::SyncZlib { .. } | Self::AsyncZlib { .. } => CompressionKind::Zlib,
            Self::SyncZstd { .. } | Self::AsyncZstd { .. } => CompressionKind::Zstd,
        }
    }

    /// Append path recorded by this magic byte.
    pub const fn mode(self) -> AppendMode {
        match self {
            Self::SyncZlib { .. } | Self::SyncZstd { .. } => AppendMode::Sync,
            Self::AsyncZlib { .. } | Self::AsyncZstd { .. } => AppendMode::Async,
        }
    }

    /// Whether the header carries a client public key slot marked as encrypted.
    pub const fn encrypted(self) -> bool {
        match self {
            Self::SyncZlib { encrypted }
            | Self::SyncZstd { encrypted }
            | Self::AsyncZlib { encrypted }
            | Self::AsyncZstd { encrypted } => encrypted,
        }
    }
}

impl From<BlockMagic> for u8 {
    fn from(value: BlockMagic) -> Self {
        match value {
            BlockMagic::SyncZlib { encrypted: true } => MAGIC_SYNC_ZLIB_START,
            BlockMagic::SyncZlib { encrypted: false } => MAGIC_SYNC_NO_CRYPT_ZLIB_START,
            BlockMagic::AsyncZlib { encrypted: true } => MAGIC_ASYNC_ZLIB_START,
            BlockMagic::AsyncZlib { encrypted: false } => MAGIC_ASYNC_NO_CRYPT_ZLIB_START,
            BlockMagic::SyncZstd { encrypted: true } => MAGIC_SYNC_ZSTD_START,
            BlockMagic::SyncZstd { encrypted: false } => MAGIC_SYNC_NO_CRYPT_ZSTD_START,
            BlockMagic::AsyncZstd { encrypted: true } => MAGIC_ASYNC_ZSTD_START,
            BlockMagic::AsyncZstd { encrypted: false } => MAGIC_ASYNC_NO_CRYPT_ZSTD_START,
        }
    }
}

impl TryFrom<u8> for BlockMagic {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            MAGIC_SYNC_ZLIB_START => Ok(Self::SyncZlib { encrypted: true }),
            MAGIC_SYNC_NO_CRYPT_ZLIB_START => Ok(Self::SyncZlib { encrypted: false }),
            MAGIC_ASYNC_ZLIB_START => Ok(Self::AsyncZlib { encrypted: true }),
            MAGIC_ASYNC_NO_CRYPT_ZLIB_START => Ok(Self::AsyncZlib { encrypted: false }),
            MAGIC_SYNC_ZSTD_START => Ok(Self::SyncZstd { encrypted: true }),
            MAGIC_SYNC_NO_CRYPT_ZSTD_START => Ok(Self::SyncZstd { encrypted: false }),
            MAGIC_ASYNC_ZSTD_START => Ok(Self::AsyncZstd { encrypted: true }),
            MAGIC_ASYNC_NO_CRYPT_ZSTD_START => Ok(Self::AsyncZstd { encrypted: false }),
            other => Err(ProtocolError::InvalidMagic(other)),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
/// Decoded header for one modern xlog block.
pub struct LogHeader {
    /// Magic describing compression, append mode, and the crypto slot.
    pub magic: BlockMagic,
    /// Sequence number. Async blocks increment and skip zero; sync blocks use zero.
    pub seq: u16,
    /// Begin hour in local time.
    pub begin_hour: Hour,
    /// End hour in local time.
    pub end_hour: Hour,
    /// Payload byte length, excluding header and tail marker.
    pub len: u32,
    /// Client public key bytes. Zero when the block has no crypto slot.
    pub client_pubkey: [u8; 64],
}

#[derive(Debug, Error)]
/// Errors returned by header decode/mutation helpers.
pub enum ProtocolError {
    #[error("invalid header length")]
    /// The provided buffer is shorter than [`HEADER_LEN`].
    InvalidHeaderLen,
    #[error("invalid magic byte: {0:#x}")]
    /// The header used a start magic byte outside the supported xlog set.
    InvalidMagic(u8),
    #[error("hour {0} is outside 0..=23")]
    /// A header hour was outside the local-time range stored by xlog.
    InvalidHour(u8),
}

impl LogHeader {
    /// Encode this header into the on-disk xlog header layout.
    pub fn encode(self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0] = u8::from(self.magic);
        out[1..3].copy_from_slice(&self.seq.to_le_bytes());
        out[3] = u8::from(self.begin_hour);
        out[HEADER_END_HOUR_OFFSET] = u8::from(self.end_hour);
        out[HEADER_LEN_OFFSET..HEADER_LEN_OFFSET + 4].copy_from_slice(&self.len.to_le_bytes());
        out[HEADER_CLIENT_PUBKEY_OFFSET..HEADER_CLIENT_PUBKEY_OFFSET + 64]
            .copy_from_slice(&self.client_pubkey);
        out
    }

    /// Decode one xlog header from the start of `buf`.
    pub fn decode(buf: &[u8]) -> Result<Self, ProtocolError> {
        if buf.len() < HEADER_LEN {
            return Err(ProtocolError::InvalidHeaderLen);
        }
        let magic = BlockMagic::try_from(buf[0])?;
        let begin_hour = Hour::try_from(buf[3])?;
        let end_hour = Hour::try_from(buf[HEADER_END_HOUR_OFFSET])?;

        let mut key = [0u8; 64];
        key.copy_from_slice(&buf[HEADER_CLIENT_PUBKEY_OFFSET..HEADER_CLIENT_PUBKEY_OFFSET + 64]);
        Ok(Self {
            magic,
            seq: u16::from_le_bytes([buf[1], buf[2]]),
            begin_hour,
            end_hour,
            len: u32::from_le_bytes([
                buf[HEADER_LEN_OFFSET],
                buf[HEADER_LEN_OFFSET + 1],
                buf[HEADER_LEN_OFFSET + 2],
                buf[HEADER_LEN_OFFSET + 3],
            ]),
            client_pubkey: key,
        })
    }
}

/// Return whether `magic` is one of the supported modern xlog start markers.
pub fn magic_start_is_valid(magic: u8) -> bool {
    BlockMagic::try_from(magic).is_ok()
}

/// Add `add_len` to the payload length stored in an encoded header.
///
/// Returns the new payload length.
pub fn update_log_len_in_place(buf: &mut [u8], add_len: u32) -> Result<u32, ProtocolError> {
    if buf.len() < HEADER_LEN {
        return Err(ProtocolError::InvalidHeaderLen);
    }
    let current = u32::from_le_bytes([
        buf[HEADER_LEN_OFFSET],
        buf[HEADER_LEN_OFFSET + 1],
        buf[HEADER_LEN_OFFSET + 2],
        buf[HEADER_LEN_OFFSET + 3],
    ]);
    let next = current.saturating_add(add_len);
    buf[HEADER_LEN_OFFSET..HEADER_LEN_OFFSET + 4].copy_from_slice(&next.to_le_bytes());
    Ok(next)
}

/// Update the encoded end-hour field inside an existing header.
pub fn update_end_hour_in_place(buf: &mut [u8], hour: u8) -> Result<(), ProtocolError> {
    if buf.len() < HEADER_LEN {
        return Err(ProtocolError::InvalidHeaderLen);
    }
    buf[HEADER_END_HOUR_OFFSET] = hour;
    Ok(())
}

/// Async sequence generator matching Mars xlog semantics.
pub struct SeqGenerator {
    seq: AtomicU16,
}

impl Default for SeqGenerator {
    fn default() -> Self {
        Self {
            seq: AtomicU16::new(0),
        }
    }
}

impl SeqGenerator {
    /// Create a generator with a fixed initial sequence state.
    pub fn with_seed(seed: u16) -> Self {
        Self {
            seq: AtomicU16::new(seed),
        }
    }

    /// Generate the next async sequence number.
    ///
    /// Matches the historical C++ behavior: increment first, then skip `0`.
    /// Under wraparound races concurrent callers may observe gaps, but `0` is
    /// never returned and values are not reused.
    pub fn next_async(&self) -> u16 {
        let mut next = self.seq.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        if next == 0 {
            next = self.seq.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            if next == 0 {
                next = 1;
                self.seq.store(1, Ordering::Relaxed);
            }
        }
        next
    }

    /// Return the fixed sync sequence number used by Mars xlog.
    pub fn sync_seq() -> u16 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::{
        update_end_hour_in_place, update_log_len_in_place, BlockMagic, Hour, LogHeader,
        ProtocolError, SeqGenerator, HEADER_LEN, MAGIC_ASYNC_NO_CRYPT_ZLIB_START,
    };

    #[test]
    fn decode_rejects_short_buffers_and_invalid_magic() {
        assert!(matches!(
            LogHeader::decode(&[0u8; HEADER_LEN - 1]),
            Err(ProtocolError::InvalidHeaderLen)
        ));

        let mut bytes = [0u8; HEADER_LEN];
        bytes[0] = 0x05;
        assert!(matches!(
            LogHeader::decode(&bytes),
            Err(ProtocolError::InvalidMagic(0x05))
        ));
    }

    #[test]
    fn in_place_updates_reject_short_buffers_and_saturate_length() {
        let mut short = [0u8; HEADER_LEN - 1];
        assert!(matches!(
            update_log_len_in_place(&mut short, 1),
            Err(ProtocolError::InvalidHeaderLen)
        ));
        assert!(matches!(
            update_end_hour_in_place(&mut short, 1),
            Err(ProtocolError::InvalidHeaderLen)
        ));

        let mut bytes = LogHeader {
            magic: BlockMagic::AsyncZlib { encrypted: false },
            seq: 1,
            begin_hour: Hour::try_from(2).unwrap(),
            end_hour: Hour::try_from(2).unwrap(),
            len: u32::MAX - 1,
            client_pubkey: [0; 64],
        }
        .encode();
        assert_eq!(bytes[0], MAGIC_ASYNC_NO_CRYPT_ZLIB_START);
        assert_eq!(update_log_len_in_place(&mut bytes, 10).unwrap(), u32::MAX);
        update_end_hour_in_place(&mut bytes, 9).unwrap();
        let decoded = LogHeader::decode(&bytes).unwrap();
        assert_eq!(decoded.len, u32::MAX);
        assert_eq!(u8::from(decoded.end_hour), 9);
        assert!(Hour::try_from(24).is_err());
    }

    #[test]
    fn seq_generator_wraps_without_emitting_zero() {
        let seq = SeqGenerator::with_seed(u16::MAX - 1);
        assert_eq!(seq.next_async(), u16::MAX);
        assert_eq!(seq.next_async(), 1);
        assert_eq!(seq.next_async(), 2);
        assert_eq!(SeqGenerator::sync_seq(), 0);
    }
}
