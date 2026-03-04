use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use memmap2::MmapMut;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MmapStoreError {
    #[error("invalid mmap capacity: {0}")]
    InvalidCapacity(usize),
    #[error("create parent directory failed for {0}: {1}")]
    CreateParent(PathBuf, #[source] std::io::Error),
    #[error("open mmap file failed for {0}: {1}")]
    OpenFile(PathBuf, #[source] std::io::Error),
    #[error("resize mmap file failed for {0}: {1}")]
    ResizeFile(PathBuf, #[source] std::io::Error),
    #[error("preallocate mmap file failed for {0}: {1}")]
    PreallocateFile(PathBuf, #[source] std::io::Error),
    #[error("memory-map file failed for {0}: {1}")]
    MapFile(PathBuf, #[source] std::io::Error),
    #[error("flush mmap file failed for {0}: {1}")]
    Flush(PathBuf, #[source] std::io::Error),
}

pub struct MmapStore {
    path: PathBuf,
    mmap: MmapMut,
}

impl MmapStore {
    pub fn open_or_create(
        path: impl Into<PathBuf>,
        capacity: usize,
    ) -> Result<Self, MmapStoreError> {
        if capacity == 0 {
            return Err(MmapStoreError::InvalidCapacity(capacity));
        }

        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| MmapStoreError::CreateParent(parent.to_path_buf(), e))?;
        }

        let existed = path.exists();
        let mut file = open_rw_file(&path)?;

        let mut must_zero_fill = !existed;
        let current_len = file
            .metadata()
            .map_err(|e| MmapStoreError::OpenFile(path.clone(), e))?
            .len();
        if current_len != capacity as u64 {
            file.set_len(capacity as u64)
                .map_err(|e| MmapStoreError::ResizeFile(path.clone(), e))?;
            must_zero_fill = true;
        }

        if must_zero_fill {
            preallocate_by_zero_write(&mut file, capacity)
                .map_err(|e| MmapStoreError::PreallocateFile(path.clone(), e))?;
        }

        let mmap = unsafe {
            MmapMut::map_mut(&file).map_err(|e| MmapStoreError::MapFile(path.clone(), e))?
        };

        Ok(Self { path, mmap })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> usize {
        self.mmap.len()
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.mmap
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.mmap
    }

    pub fn flush(&mut self) -> Result<(), MmapStoreError> {
        self.mmap
            .flush()
            .map_err(|e| MmapStoreError::Flush(self.path.clone(), e))
    }
}

fn open_rw_file(path: &Path) -> Result<File, MmapStoreError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
        .map_err(|e| MmapStoreError::OpenFile(path.to_path_buf(), e))
}

fn preallocate_by_zero_write(file: &mut File, capacity: usize) -> std::io::Result<()> {
    // Match Mars behavior: explicitly write zeroes to back storage to avoid sparse-file SIGBUS.
    file.seek(SeekFrom::Start(0))?;
    let zeros = vec![0u8; capacity];
    file.write_all(&zeros)?;
    file.flush()?;
    Ok(())
}
