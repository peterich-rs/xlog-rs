use std::sync::atomic::{AtomicU16, Ordering};

use thiserror::Error;

/// Byte length of one encoded xlog header.
pub const HEADER_LEN: usize = 1 + 2 + 1 + 1 + 4 + 64;
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
/// Decoded header for one xlog block.
pub struct LogHeader {
    /// Magic byte describing compression, append mode, and crypto usage.
    pub magic: u8,
    /// Sequence number. Async blocks increment and skip zero; sync blocks use zero.
    pub seq: u16,
    /// Begin hour in local time.
    pub begin_hour: u8,
    /// End hour in local time.
    pub end_hour: u8,
    /// Payload byte length, excluding header and tail marker.
    pub len: u32,
    /// Optional client public key bytes used by encrypted logs.
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
}

impl LogHeader {
    /// Encode this header into the on-disk xlog header layout.
    pub fn encode(self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0] = self.magic;
        out[1..3].copy_from_slice(&self.seq.to_le_bytes());
        out[3] = self.begin_hour;
        out[4] = self.end_hour;
        out[5..9].copy_from_slice(&self.len.to_le_bytes());
        out[9..73].copy_from_slice(&self.client_pubkey);
        out
    }

    /// Decode one xlog header from the start of `buf`.
    pub fn decode(buf: &[u8]) -> Result<Self, ProtocolError> {
        if buf.len() < HEADER_LEN {
            return Err(ProtocolError::InvalidHeaderLen);
        }
        let magic = buf[0];
        if !magic_start_is_valid(magic) {
            return Err(ProtocolError::InvalidMagic(magic));
        }

        let mut key = [0u8; 64];
        key.copy_from_slice(&buf[9..73]);
        Ok(Self {
            magic,
            seq: u16::from_le_bytes([buf[1], buf[2]]),
            begin_hour: buf[3],
            end_hour: buf[4],
            len: u32::from_le_bytes([buf[5], buf[6], buf[7], buf[8]]),
            client_pubkey: key,
        })
    }
}

/// Select the xlog magic byte for a `(compression, append mode, crypto)` tuple.
pub fn select_magic(compress: CompressionKind, mode: AppendMode, crypt: bool) -> u8 {
    match (compress, mode, crypt) {
        (CompressionKind::Zlib, AppendMode::Sync, true) => MAGIC_SYNC_ZLIB_START,
        (CompressionKind::Zlib, AppendMode::Sync, false) => MAGIC_SYNC_NO_CRYPT_ZLIB_START,
        (CompressionKind::Zlib, AppendMode::Async, true) => MAGIC_ASYNC_ZLIB_START,
        (CompressionKind::Zlib, AppendMode::Async, false) => MAGIC_ASYNC_NO_CRYPT_ZLIB_START,
        (CompressionKind::Zstd, AppendMode::Sync, true) => MAGIC_SYNC_ZSTD_START,
        (CompressionKind::Zstd, AppendMode::Sync, false) => MAGIC_SYNC_NO_CRYPT_ZSTD_START,
        (CompressionKind::Zstd, AppendMode::Async, true) => MAGIC_ASYNC_ZSTD_START,
        (CompressionKind::Zstd, AppendMode::Async, false) => MAGIC_ASYNC_NO_CRYPT_ZSTD_START,
    }
}

/// Return whether `magic` is one of the supported xlog start markers.
pub fn magic_start_is_valid(magic: u8) -> bool {
    matches!(
        magic,
        MAGIC_SYNC_ZLIB_START
            | MAGIC_SYNC_NO_CRYPT_ZLIB_START
            | MAGIC_ASYNC_ZLIB_START
            | MAGIC_ASYNC_NO_CRYPT_ZLIB_START
            | MAGIC_SYNC_ZSTD_START
            | MAGIC_SYNC_NO_CRYPT_ZSTD_START
            | MAGIC_ASYNC_ZSTD_START
            | MAGIC_ASYNC_NO_CRYPT_ZSTD_START
    )
}

/// Add `add_len` to the payload length stored in an encoded header.
///
/// Returns the new payload length.
pub fn update_log_len_in_place(buf: &mut [u8], add_len: u32) -> Result<u32, ProtocolError> {
    if buf.len() < HEADER_LEN {
        return Err(ProtocolError::InvalidHeaderLen);
    }
    let current = u32::from_le_bytes([buf[5], buf[6], buf[7], buf[8]]);
    let next = current.saturating_add(add_len);
    buf[5..9].copy_from_slice(&next.to_le_bytes());
    Ok(next)
}

/// Update the encoded end-hour field inside an existing header.
pub fn update_end_hour_in_place(buf: &mut [u8], hour: u8) -> Result<(), ProtocolError> {
    if buf.len() < HEADER_LEN {
        return Err(ProtocolError::InvalidHeaderLen);
    }
    buf[4] = hour;
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
        update_end_hour_in_place, update_log_len_in_place, LogHeader, ProtocolError, SeqGenerator,
        HEADER_LEN, MAGIC_ASYNC_NO_CRYPT_ZLIB_START,
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
            magic: MAGIC_ASYNC_NO_CRYPT_ZLIB_START,
            seq: 1,
            begin_hour: 2,
            end_hour: 2,
            len: u32::MAX - 1,
            client_pubkey: [0; 64],
        }
        .encode();
        assert_eq!(update_log_len_in_place(&mut bytes, 10).unwrap(), u32::MAX);
        update_end_hour_in_place(&mut bytes, 9).unwrap();
        let decoded = LogHeader::decode(&bytes).unwrap();
        assert_eq!(decoded.len, u32::MAX);
        assert_eq!(decoded.end_hour, 9);
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
