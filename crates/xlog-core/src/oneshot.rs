use std::fs::File;
use std::io::Read;

use chrono::{Local, Timelike};

use crate::buffer::recover_blocks;
use crate::file_manager::FileManager;
use crate::platform_tid::current_tid;
use crate::protocol::{
    select_magic, AppendMode, CompressionKind, LogHeader, HEADER_LEN,
    MAGIC_ASYNC_NO_CRYPT_ZLIB_START, MAGIC_ASYNC_NO_CRYPT_ZSTD_START, MAGIC_ASYNC_ZLIB_START,
    MAGIC_ASYNC_ZSTD_START, MAGIC_END, MAGIC_SYNC_NO_CRYPT_ZLIB_START,
    MAGIC_SYNC_NO_CRYPT_ZSTD_START, MAGIC_SYNC_ZLIB_START, MAGIC_SYNC_ZSTD_START,
};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum FileIoAction {
    None = 0,
    Success = 1,
    Unnecessary = 2,
    OpenFailed = 3,
    ReadFailed = 4,
    WriteFailed = 5,
    CloseFailed = 6,
    RemoveFailed = 7,
}

pub fn oneshot_flush(
    file_manager: &FileManager,
    mmap_capacity: usize,
    max_file_size: u64,
) -> FileIoAction {
    let mmap_path = file_manager.mmap_path();
    if !mmap_path.exists() {
        return FileIoAction::Unnecessary;
    }

    let mut f = match File::open(&mmap_path) {
        Ok(f) => f,
        Err(_) => return FileIoAction::OpenFailed,
    };

    let mut data = vec![0u8; mmap_capacity];
    if f.read_exact(&mut data).is_err() {
        return FileIoAction::ReadFailed;
    }

    let recovered = recover_blocks(&data);
    if recovered.bytes.is_empty() {
        return FileIoAction::Unnecessary;
    }

    let sample_header = if recovered.bytes.len() >= HEADER_LEN {
        LogHeader::decode(&recovered.bytes[..HEADER_LEN]).ok()
    } else {
        None
    };
    if let Some(begin) = build_sync_tip_block(
        sample_header,
        "~~~~~ begin of mmap from other process ~~~~~\n",
    ) {
        if file_manager
            .append_log_bytes(&begin, max_file_size, false)
            .is_err()
        {
            return FileIoAction::WriteFailed;
        }
    }

    if file_manager
        .append_log_bytes(&recovered.bytes, max_file_size, false)
        .is_err()
    {
        return FileIoAction::WriteFailed;
    }
    let end_tip = format!(
        "~~~~~ end of mmap from other process ~~~~~{}\n",
        current_mark_info()
    );
    if let Some(end) = build_sync_tip_block(sample_header, &end_tip) {
        if file_manager
            .append_log_bytes(&end, max_file_size, false)
            .is_err()
        {
            return FileIoAction::WriteFailed;
        }
    }

    if std::fs::remove_file(&mmap_path).is_err() {
        return FileIoAction::RemoveFailed;
    }

    FileIoAction::Success
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
