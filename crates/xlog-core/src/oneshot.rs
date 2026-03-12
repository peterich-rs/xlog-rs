use std::fs::File;

use chrono::{Local, Timelike};
use memmap2::MmapOptions;

use crate::buffer::scan_recovery;
use crate::file_manager::FileManager;
use crate::platform_tid::current_tid;
use crate::protocol::{
    select_magic, AppendMode, CompressionKind, LogHeader, HEADER_LEN,
    MAGIC_ASYNC_NO_CRYPT_ZLIB_START, MAGIC_ASYNC_NO_CRYPT_ZSTD_START, MAGIC_ASYNC_ZLIB_START,
    MAGIC_ASYNC_ZSTD_START, MAGIC_END, MAGIC_SYNC_NO_CRYPT_ZLIB_START,
    MAGIC_SYNC_NO_CRYPT_ZSTD_START, MAGIC_SYNC_ZLIB_START, MAGIC_SYNC_ZSTD_START,
};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
/// Result code returned by [`oneshot_flush`].
pub enum FileIoAction {
    /// No file action was taken.
    None = 0,
    /// The recovered bytes were flushed successfully.
    Success = 1,
    /// No mmap file was present or it contained no recoverable bytes.
    Unnecessary = 2,
    /// Opening the mmap file failed.
    OpenFailed = 3,
    /// Reading or mapping the mmap file failed.
    ReadFailed = 4,
    /// Writing recovered bytes to the logfile failed.
    WriteFailed = 5,
    /// Reserved for parity with historical file-action result codes.
    CloseFailed = 6,
    /// Removing the consumed mmap file failed.
    RemoveFailed = 7,
}

/// Drain another process's mmap buffer into the active logfile exactly once.
///
/// This is the Rust equivalent of Mars xlog's oneshot recovery path. It reads
/// the raw mmap bytes, recovers a pending block when possible, appends optional
/// begin/end marker blocks, durably syncs each appended recovery block, and
/// removes the mmap file only after the destination writes have been synced.
pub fn oneshot_flush(
    file_manager: &FileManager,
    mmap_capacity: usize,
    max_file_size: u64,
) -> FileIoAction {
    let mmap_path = file_manager.mmap_path();
    if !mmap_path.exists() {
        return FileIoAction::Unnecessary;
    }

    let f = match File::open(&mmap_path) {
        Ok(f) => f,
        Err(_) => return FileIoAction::OpenFailed,
    };

    let mmap_len = match f.metadata() {
        Ok(meta) => meta.len() as usize,
        Err(_) => return FileIoAction::ReadFailed,
    };
    if mmap_len != mmap_capacity {
        return FileIoAction::ReadFailed;
    }
    let data = match unsafe { MmapOptions::new().len(mmap_capacity).map(&f) } {
        Ok(mapped) => mapped,
        Err(_) => return FileIoAction::ReadFailed,
    };

    let scan = scan_recovery(&data);
    if scan.valid_len == 0 {
        return FileIoAction::Unnecessary;
    }

    let sample_header = if scan.valid_len >= HEADER_LEN {
        LogHeader::decode(&data[..HEADER_LEN]).ok()
    } else {
        None
    };
    if let Some(begin) = build_sync_tip_block(
        sample_header,
        "~~~~~ begin of mmap from other process ~~~~~\n",
    ) {
        if append_recovered_bytes_durable(file_manager, &begin, max_file_size).is_err() {
            return FileIoAction::WriteFailed;
        }
    }

    if scan.recovered_pending_block {
        // Keep the recovered block contiguous so another process cannot
        // interleave between payload bytes and the repaired tail marker.
        let mut recovered = Vec::with_capacity(scan.valid_len.saturating_add(1));
        recovered.extend_from_slice(&data[..scan.valid_len]);
        recovered.push(MAGIC_END);
        if append_recovered_bytes_durable(file_manager, &recovered, max_file_size).is_err() {
            return FileIoAction::WriteFailed;
        }
    } else if append_recovered_bytes_durable(file_manager, &data[..scan.valid_len], max_file_size)
        .is_err()
    {
        return FileIoAction::WriteFailed;
    }
    let end_tip = format!(
        "~~~~~ end of mmap from other process ~~~~~{}\n",
        current_mark_info()
    );
    if let Some(end) = build_sync_tip_block(sample_header, &end_tip) {
        if append_recovered_bytes_durable(file_manager, &end, max_file_size).is_err() {
            return FileIoAction::WriteFailed;
        }
    }

    drop(data);
    if std::fs::remove_file(&mmap_path).is_err() {
        return FileIoAction::RemoveFailed;
    }

    FileIoAction::Success
}

fn append_recovered_bytes_durable(
    file_manager: &FileManager,
    bytes: &[u8],
    max_file_size: u64,
) -> Result<(), crate::file_manager::FileManagerError> {
    file_manager.append_log_bytes_durable(bytes, max_file_size, false)
}

fn magic_profile(magic: u8) -> Option<(CompressionKind, bool)> {
    match magic {
        MAGIC_SYNC_ZLIB_START | MAGIC_ASYNC_ZLIB_START => Some((CompressionKind::Zlib, true)),
        MAGIC_SYNC_NO_CRYPT_ZLIB_START | MAGIC_ASYNC_NO_CRYPT_ZLIB_START => {
            Some((CompressionKind::Zlib, false))
        }
        MAGIC_SYNC_ZSTD_START | MAGIC_ASYNC_ZSTD_START => Some((CompressionKind::Zstd, true)),
        MAGIC_SYNC_NO_CRYPT_ZSTD_START | MAGIC_ASYNC_NO_CRYPT_ZSTD_START => {
            Some((CompressionKind::Zstd, false))
        }
        _ => None,
    }
}

fn build_sync_tip_block(sample_header: Option<LogHeader>, tip: &str) -> Option<Vec<u8>> {
    let sample = sample_header?;
    let (compression, crypt) = magic_profile(sample.magic)?;
    let payload = tip.as_bytes();
    let now_hour = Local::now().hour() as u8;
    let header = LogHeader {
        magic: select_magic(compression, AppendMode::Sync, crypt),
        seq: 0,
        begin_hour: now_hour,
        end_hour: now_hour,
        len: u32::try_from(payload.len()).ok()?,
        client_pubkey: if crypt { sample.client_pubkey } else { [0; 64] },
    };
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len() + 1);
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(payload);
    out.push(MAGIC_END);
    Some(out)
}

fn current_mark_info() -> String {
    let now = Local::now();
    format!(
        "[{},{}][{}]",
        std::process::id(),
        current_tid(),
        now.format("%Y-%m-%d %z %H:%M:%S")
    )
}

#[cfg(test)]
mod tests {
    use super::{build_sync_tip_block, magic_profile};
    use crate::protocol::{
        select_magic, AppendMode, CompressionKind, LogHeader, HEADER_LEN,
        MAGIC_ASYNC_NO_CRYPT_ZLIB_START, MAGIC_ASYNC_NO_CRYPT_ZSTD_START, MAGIC_ASYNC_ZLIB_START,
        MAGIC_ASYNC_ZSTD_START, MAGIC_END, MAGIC_SYNC_NO_CRYPT_ZLIB_START,
        MAGIC_SYNC_NO_CRYPT_ZSTD_START, MAGIC_SYNC_ZLIB_START, MAGIC_SYNC_ZSTD_START,
    };

    #[test]
    fn magic_profile_maps_all_supported_magic_variants() {
        assert_eq!(
            magic_profile(MAGIC_SYNC_ZLIB_START),
            Some((CompressionKind::Zlib, true))
        );
        assert_eq!(
            magic_profile(MAGIC_ASYNC_ZLIB_START),
            Some((CompressionKind::Zlib, true))
        );
        assert_eq!(
            magic_profile(MAGIC_SYNC_NO_CRYPT_ZLIB_START),
            Some((CompressionKind::Zlib, false))
        );
        assert_eq!(
            magic_profile(MAGIC_ASYNC_NO_CRYPT_ZLIB_START),
            Some((CompressionKind::Zlib, false))
        );
        assert_eq!(
            magic_profile(MAGIC_SYNC_ZSTD_START),
            Some((CompressionKind::Zstd, true))
        );
        assert_eq!(
            magic_profile(MAGIC_ASYNC_ZSTD_START),
            Some((CompressionKind::Zstd, true))
        );
        assert_eq!(
            magic_profile(MAGIC_SYNC_NO_CRYPT_ZSTD_START),
            Some((CompressionKind::Zstd, false))
        );
        assert_eq!(
            magic_profile(MAGIC_ASYNC_NO_CRYPT_ZSTD_START),
            Some((CompressionKind::Zstd, false))
        );
        assert_eq!(magic_profile(0), None);
    }

    #[test]
    fn build_sync_tip_block_preserves_crypto_profile_for_encrypted_headers() {
        let sample = LogHeader {
            magic: select_magic(CompressionKind::Zstd, AppendMode::Async, true),
            seq: 42,
            begin_hour: 1,
            end_hour: 1,
            len: 3,
            client_pubkey: [7; 64],
        };

        let block = build_sync_tip_block(Some(sample), "tip").unwrap();
        let header = LogHeader::decode(&block[..HEADER_LEN]).unwrap();

        assert_eq!(
            header.magic,
            select_magic(CompressionKind::Zstd, AppendMode::Sync, true)
        );
        assert_eq!(header.client_pubkey, [7; 64]);
        assert_eq!(&block[HEADER_LEN..block.len() - 1], b"tip");
        assert_eq!(block[block.len() - 1], MAGIC_END);
    }

    #[test]
    fn build_sync_tip_block_zeroes_pubkey_for_plaintext_headers() {
        let sample = LogHeader {
            magic: select_magic(CompressionKind::Zlib, AppendMode::Async, false),
            seq: 7,
            begin_hour: 1,
            end_hour: 1,
            len: 5,
            client_pubkey: [9; 64],
        };

        let block = build_sync_tip_block(Some(sample), "plain").unwrap();
        let header = LogHeader::decode(&block[..HEADER_LEN]).unwrap();

        assert_eq!(
            header.magic,
            select_magic(CompressionKind::Zlib, AppendMode::Sync, false)
        );
        assert_eq!(header.client_pubkey, [0; 64]);
    }

    #[test]
    fn build_sync_tip_block_rejects_missing_or_unknown_sample_headers() {
        assert!(build_sync_tip_block(None, "tip").is_none());

        let sample = LogHeader {
            magic: 0,
            seq: 1,
            begin_hour: 1,
            end_hour: 1,
            len: 3,
            client_pubkey: [0; 64],
        };
        assert!(build_sync_tip_block(Some(sample), "tip").is_none());
    }
}
