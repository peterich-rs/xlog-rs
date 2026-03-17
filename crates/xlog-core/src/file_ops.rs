use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::time::SystemTime;

use crate::active_append::rollback_file_to_len;
use crate::file_manager::FileManagerError;

const FILE_COPY_BUFFER_SIZE: usize = 128 * 1024;

pub(crate) fn local_file_state(path: &Path) -> (bool, u64) {
    match fs::metadata(path) {
        Ok(meta) => (true, meta.len()),
        Err(_) => (false, 0),
    }
}

pub(crate) fn append_file_to_file(src: &Path, dst: &Path) -> Result<(), FileManagerError> {
    if src == dst || !src.exists() {
        return Ok(());
    }
    let src_meta =
        fs::metadata(src).map_err(|e| FileManagerError::Metadata(src.to_path_buf(), e))?;
    if src_meta.len() == 0 {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| FileManagerError::CreateDir(parent.to_path_buf(), e))?;
    }

    let mut src_file =
        File::open(src).map_err(|e| FileManagerError::OpenFile(src.to_path_buf(), e))?;
    let mut dst_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dst)
        .map_err(|e| FileManagerError::OpenFile(dst.to_path_buf(), e))?;
    let dst_before_len = dst_file
        .metadata()
        .map_err(|e| FileManagerError::Metadata(dst.to_path_buf(), e))?
        .len();

    let mut buf = vec![0u8; FILE_COPY_BUFFER_SIZE];
    let mut copied = 0u64;
    loop {
        let n = match src_file.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                rollback_file_to_len(&mut dst_file, dst_before_len);
                return Err(FileManagerError::ReadFile(src.to_path_buf(), e));
            }
        };
        if n == 0 {
            break;
        }
        if let Err(e) = dst_file.write_all(&buf[..n]) {
            rollback_file_to_len(&mut dst_file, dst_before_len);
            return Err(FileManagerError::WriteFile(dst.to_path_buf(), e));
        }
        copied = copied.saturating_add(n as u64);
    }
    if copied < src_meta.len() {
        rollback_file_to_len(&mut dst_file, dst_before_len);
        return Err(FileManagerError::WriteFile(
            dst.to_path_buf(),
            std::io::Error::new(std::io::ErrorKind::WriteZero, "partial append"),
        ));
    }
    if let Err(e) = dst_file.sync_data() {
        rollback_file_to_len(&mut dst_file, dst_before_len);
        return Err(FileManagerError::SyncFile(dst.to_path_buf(), e));
    }
    Ok(())
}

pub(crate) fn file_mtime(path: &Path) -> Result<SystemTime, FileManagerError> {
    let meta = fs::metadata(path).map_err(|e| FileManagerError::Metadata(path.to_path_buf(), e))?;
    meta.modified()
        .map_err(|e| FileManagerError::Metadata(path.to_path_buf(), e))
}
