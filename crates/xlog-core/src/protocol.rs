use std::sync::atomic::{AtomicU16, Ordering};

use thiserror::Error;

pub const HEADER_LEN: usize = 1 + 2 + 1 + 1 + 4 + 64;
pub const TAILER_LEN: usize = 1;
pub const MAGIC_END: u8 = 0x00;

pub const MAGIC_SYNC_ZLIB_START: u8 = 0x06;
pub const MAGIC_SYNC_NO_CRYPT_ZLIB_START: u8 = 0x08;
pub const MAGIC_ASYNC_ZLIB_START: u8 = 0x07;
pub const MAGIC_ASYNC_NO_CRYPT_ZLIB_START: u8 = 0x09;

pub const MAGIC_SYNC_ZSTD_START: u8 = 0x0A;
pub const MAGIC_SYNC_NO_CRYPT_ZSTD_START: u8 = 0x0B;
pub const MAGIC_ASYNC_ZSTD_START: u8 = 0x0C;
pub const MAGIC_ASYNC_NO_CRYPT_ZSTD_START: u8 = 0x0D;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CompressionKind {
    Zlib,
    Zstd,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum AppendMode {
    Sync,
    Async,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct LogHeader {
    pub magic: u8,
    pub seq: u16,
    pub begin_hour: u8,
    pub end_hour: u8,
    pub len: u32,
    pub client_pubkey: [u8; 64],
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("invalid header length")]
    InvalidHeaderLen,
    #[error("invalid magic byte: {0:#x}")]
    InvalidMagic(u8),
}

impl LogHeader {
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

pub fn update_log_len_in_place(buf: &mut [u8], add_len: u32) -> Result<u32, ProtocolError> {
    if buf.len() < HEADER_LEN {
        return Err(ProtocolError::InvalidHeaderLen);
    }
    let current = u32::from_le_bytes([buf[5], buf[6], buf[7], buf[8]]);
    let next = current.saturating_add(add_len);
    buf[5..9].copy_from_slice(&next.to_le_bytes());
    Ok(next)
}

pub fn update_end_hour_in_place(buf: &mut [u8], hour: u8) -> Result<(), ProtocolError> {
    if buf.len() < HEADER_LEN {
        return Err(ProtocolError::InvalidHeaderLen);
    }
    buf[4] = hour;
    Ok(())
}

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
    pub fn with_seed(seed: u16) -> Self {
        Self {
            seq: AtomicU16::new(seed),
        }
    }

    /// Matches C++ behavior for async logs: increment, and skip 0.
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

    pub fn sync_seq() -> u16 {
        0
    }
}
