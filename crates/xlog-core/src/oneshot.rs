use std::fs::File;
use std::io::Read;

use crate::buffer::recover_blocks;
use crate::file_manager::FileManager;

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

    if file_manager
        .append_log_bytes(&recovered.bytes, max_file_size, false)
        .is_err()
    {
        return FileIoAction::WriteFailed;
    }

    if std::fs::remove_file(&mmap_path).is_err() {
        return FileIoAction::RemoveFailed;
    }

    FileIoAction::Success
}
