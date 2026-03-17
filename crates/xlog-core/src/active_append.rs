use std::fs::File;
use std::io::{IoSlice, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::file_manager::FileManagerError;

// Keep-open sync path benefits from a moderate userspace append buffer under
// contention without turning flush bursts into a new tail-latency problem.
pub(crate) const ACTIVE_APPEND_BUFFER_CAPACITY: usize = 64 * 1024;

#[derive(Debug)]
pub(crate) struct ActiveAppendFile {
    pub(crate) path: PathBuf,
    pub(crate) day_key: i32,
    pub(crate) logical_len: u64,
    pub(crate) disk_len: u64,
    pub(crate) buffered: bool,
    pub(crate) write_buffer: Vec<u8>,
    pub(crate) file: File,
}

impl Drop for ActiveAppendFile {
    fn drop(&mut self) {
        if self.write_buffer.is_empty() {
            return;
        }
        // Drop cannot propagate I/O failures. Durability-sensitive paths must
        // flush explicitly instead of relying on this best-effort write.
        let _ = self.file.write_all(&self.write_buffer);
        self.write_buffer.clear();
    }
}

pub(crate) fn append_slices_keep_open(
    active: &mut ActiveAppendFile,
    slices: &[&[u8]],
) -> Result<(), FileManagerError> {
    let incoming = slices.iter().map(|slice| slice.len()).sum::<usize>();
    if incoming == 0 {
        return Ok(());
    }

    if !active.buffered {
        active.buffered = true;
        active
            .write_buffer
            .reserve(ACTIVE_APPEND_BUFFER_CAPACITY.max(incoming));
    }

    if !active.write_buffer.is_empty()
        && active.write_buffer.len().saturating_add(incoming) >= ACTIVE_APPEND_BUFFER_CAPACITY
    {
        flush_active_append_file(active)?;
    }

    if incoming >= ACTIVE_APPEND_BUFFER_CAPACITY {
        append_slices_direct(active, slices)?;
        return Ok(());
    }

    for slice in slices {
        if slice.is_empty() {
            continue;
        }
        active.write_buffer.extend_from_slice(slice);
        active.logical_len = active.logical_len.saturating_add(slice.len() as u64);
    }
    Ok(())
}

pub(crate) fn append_slices_direct(
    active: &mut ActiveAppendFile,
    slices: &[&[u8]],
) -> Result<(), FileManagerError> {
    if !active.write_buffer.is_empty() {
        flush_active_append_file(active)?;
    }

    let before_len = active.disk_len;
    match write_all_slices_vectored(&mut active.file, slices) {
        Ok(written) => {
            active.disk_len = before_len.saturating_add(written);
            active.logical_len = active.disk_len;
            Ok(())
        }
        Err(e) => {
            rollback_file_to_len(&mut active.file, before_len);
            Err(FileManagerError::WriteFile(active.path.clone(), e))
        }
    }
}

pub(crate) fn flush_active_append_file(
    active: &mut ActiveAppendFile,
) -> Result<(), FileManagerError> {
    if active.write_buffer.is_empty() {
        return Ok(());
    }

    let before_len = active.disk_len;
    if let Err(e) = active.file.write_all(&active.write_buffer) {
        rollback_file_to_len(&mut active.file, before_len);
        return Err(FileManagerError::WriteFile(active.path.clone(), e));
    }
    active.disk_len = active.logical_len;
    active.write_buffer.clear();
    Ok(())
}

pub(crate) fn sync_active_append_file_data(
    active: &mut ActiveAppendFile,
) -> Result<(), FileManagerError> {
    flush_active_append_file(active)?;
    active
        .file
        .sync_data()
        .map_err(|e| FileManagerError::SyncFile(active.path.clone(), e))
}

pub(crate) fn rollback_file_to_len(file: &mut File, target_len: u64) {
    let _ = file.set_len(target_len);
    let _ = file.seek(SeekFrom::Start(target_len));
}

fn write_all_slices_vectored(file: &mut File, slices: &[&[u8]]) -> std::io::Result<u64> {
    let mut total_written = 0u64;
    let mut slice_idx = 0usize;
    let mut slice_offset = 0usize;

    while slice_idx < slices.len() {
        while slice_idx < slices.len() && slice_offset >= slices[slice_idx].len() {
            slice_idx += 1;
            slice_offset = 0;
        }
        if slice_idx >= slices.len() {
            break;
        }

        let mut iovecs = Vec::with_capacity(slices.len().saturating_sub(slice_idx));
        let first = &slices[slice_idx][slice_offset..];
        if !first.is_empty() {
            iovecs.push(IoSlice::new(first));
        }
        for slice in &slices[slice_idx + 1..] {
            if !slice.is_empty() {
                iovecs.push(IoSlice::new(slice));
            }
        }
        if iovecs.is_empty() {
            break;
        }

        let written = loop {
            match file.write_vectored(&iovecs) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "write_vectored returned 0",
                    ));
                }
                Ok(n) => break n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        };
        total_written = total_written.saturating_add(written as u64);

        let mut remaining = written;
        while slice_idx < slices.len() {
            let current = &slices[slice_idx];
            if slice_offset >= current.len() {
                slice_idx += 1;
                slice_offset = 0;
                continue;
            }
            let available = current.len() - slice_offset;
            if remaining < available {
                slice_offset += remaining;
                break;
            }
            remaining -= available;
            slice_idx += 1;
            slice_offset = 0;
            if remaining == 0 {
                break;
            }
        }
    }

    Ok(total_written)
}
