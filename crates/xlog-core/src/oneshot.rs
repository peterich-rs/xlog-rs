use std::fs::File;
use std::io::Error as IoError;

use memmap2::MmapOptions;
use thiserror::Error;

use crate::buffer::scan_recovery;
use crate::file_manager::{FileManager, FileManagerError};
use crate::protocol::{LogHeader, HEADER_LEN, MAGIC_END};
use crate::recovery::{build_sync_tip_block, current_mark_info};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
/// Outcome of a oneshot flush that did not fail.
pub enum OneshotOutcome {
    /// Recovered bytes were appended and the mmap file was removed.
    Success,
    /// No mmap file was present, or it contained no recoverable bytes.
    Unnecessary,
}

#[derive(Debug, Error)]
/// Failures while draining another process's mmap buffer.
pub enum OneshotError {
    /// Opening the mmap file failed.
    #[error("open mmap failed")]
    Open(#[source] IoError),
    /// Reading or mapping the mmap file failed.
    #[error("read mmap failed")]
    Read(#[source] IoError),
    /// The mmap file length did not match the expected capacity.
    #[error("mmap length {actual} does not match capacity {expected}")]
    LengthMismatch {
        /// Length reported by the filesystem.
        actual: usize,
        /// Capacity the caller expected.
        expected: usize,
    },
    /// Writing recovered bytes to the logfile failed.
    #[error("write recovered bytes failed")]
    Write(#[source] FileManagerError),
    /// Removing the consumed mmap file failed.
    #[error("remove mmap failed")]
    Remove(#[source] IoError),
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
) -> Result<OneshotOutcome, OneshotError> {
    let mmap_path = file_manager.mmap_path();
    if !mmap_path.exists() {
        return Ok(OneshotOutcome::Unnecessary);
    }

    let file = File::open(&mmap_path).map_err(OneshotError::Open)?;
    let mmap_len = file
        .metadata()
        .and_then(|meta| {
            usize::try_from(meta.len())
                .map_err(|err| IoError::new(std::io::ErrorKind::InvalidData, err))
        })
        .map_err(OneshotError::Read)?;
    if mmap_len != mmap_capacity {
        return Err(OneshotError::LengthMismatch {
            actual: mmap_len,
            expected: mmap_capacity,
        });
    }
    // SAFETY: `file` is a read-only handle of `mmap_path`. The mapping length
    // matches the file length checked above. The map is dropped before the file
    // is removed, and this process does not mutate the mapping.
    let data =
        unsafe { MmapOptions::new().len(mmap_capacity).map(&file) }.map_err(OneshotError::Read)?;

    let scan = scan_recovery(&data);
    if scan.valid_len == 0 {
        return Ok(OneshotOutcome::Unnecessary);
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
        append_recovered_bytes_durable(file_manager, &begin, max_file_size)?;
    }

    if scan.recovered_pending_block {
        let mut recovered = Vec::with_capacity(scan.valid_len.saturating_add(1));
        recovered.extend_from_slice(&data[..scan.valid_len]);
        recovered.push(MAGIC_END);
        append_recovered_bytes_durable(file_manager, &recovered, max_file_size)?;
    } else {
        append_recovered_bytes_durable(file_manager, &data[..scan.valid_len], max_file_size)?;
    }
    let end_tip = format!(
        "~~~~~ end of mmap from other process ~~~~~{}\n",
        current_mark_info()
    );
    if let Some(end) = build_sync_tip_block(sample_header, &end_tip) {
        append_recovered_bytes_durable(file_manager, &end, max_file_size)?;
    }

    drop(data);
    std::fs::remove_file(&mmap_path).map_err(OneshotError::Remove)?;
    Ok(OneshotOutcome::Success)
}

fn append_recovered_bytes_durable(
    file_manager: &FileManager,
    bytes: &[u8],
    max_file_size: u64,
) -> Result<(), OneshotError> {
    file_manager
        .append_log_bytes_durable(bytes, max_file_size, false)
        .map_err(OneshotError::Write)
}
