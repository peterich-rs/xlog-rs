use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use memmap2::MmapMut;
use thiserror::Error;

#[derive(Debug, Error)]
/// Errors returned by [`MmapStore`] operations.
pub enum MmapStoreError {
    #[error("invalid mmap capacity: {0}")]
    /// The requested mmap capacity was zero.
    InvalidCapacity(usize),
    #[error("create parent directory failed for {0}: {1}")]
    /// Creating the parent directory for the mmap file failed.
    CreateParent(PathBuf, #[source] std::io::Error),
    #[error("open mmap file failed for {0}: {1}")]
    /// Opening or creating the mmap file failed.
    OpenFile(PathBuf, #[source] std::io::Error),
    #[error("resize mmap file failed for {0}: {1}")]
    /// Resizing the mmap file to the requested capacity failed.
    ResizeFile(PathBuf, #[source] std::io::Error),
    #[error("preallocate mmap file failed for {0}: {1}")]
    /// Writing zeroes to fully back the mmap file failed.
    PreallocateFile(PathBuf, #[source] std::io::Error),
    #[error("memory-map file failed for {0}: {1}")]
    /// Creating a mutable memory map over the file failed.
    MapFile(PathBuf, #[source] std::io::Error),
    #[error("flush mmap file failed for {0}: {1}")]
    /// Flushing the mmap contents to storage failed.
    Flush(PathBuf, #[source] std::io::Error),
}

/// Thin wrapper around a fixed-size mutable file-backed memory map.
pub struct MmapStore {
    path: PathBuf,
    mmap: MmapMut,
}

impl MmapStore {
    /// Open or create a file-backed mutable mmap with the requested capacity.
    ///
    /// New or resized files are explicitly zero-filled to avoid sparse-file
    /// behavior that could otherwise trigger SIGBUS on later access.
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

    /// Return the on-disk path backing this mmap.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the mmap length in bytes.
    pub fn len(&self) -> usize {
        self.mmap.len()
    }

    /// Return `true` when the mmap length is zero.
    pub fn is_empty(&self) -> bool {
        self.mmap.is_empty()
    }

    /// Borrow the full mmap as an immutable byte slice.
    pub fn as_slice(&self) -> &[u8] {
        &self.mmap
    }

    /// Borrow the full mmap as a mutable byte slice.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.mmap
    }

    /// Flush pending mmap mutations to the backing file.
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
        .truncate(false)
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

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{MmapStore, MmapStoreError};

    #[test]
    fn zero_capacity_is_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("buffer.mmap");
        assert!(matches!(
            MmapStore::open_or_create(path, 0),
            Err(MmapStoreError::InvalidCapacity(0))
        ));
    }

    #[test]
    fn open_create_zero_fills_and_persists_mutations() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("buffer.mmap");

        let mut store = MmapStore::open_or_create(&path, 16).unwrap();
        assert_eq!(store.path(), path.as_path());
        assert_eq!(store.len(), 16);
        assert!(!store.is_empty());
        assert!(store.as_slice().iter().all(|&byte| byte == 0));

        store.as_mut_slice()[..4].copy_from_slice(b"mars");
        store.flush().unwrap();
        drop(store);

        let reopened = MmapStore::open_or_create(&path, 16).unwrap();
        assert_eq!(&reopened.as_slice()[..4], b"mars");
    }

    #[test]
    fn resizing_existing_map_rezeros_the_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("buffer.mmap");

        let mut store = MmapStore::open_or_create(&path, 8).unwrap();
        store.as_mut_slice()[..4].copy_from_slice(b"data");
        store.flush().unwrap();
        drop(store);

        let resized = MmapStore::open_or_create(&path, 12).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 12);
        assert!(resized.as_slice().iter().all(|&byte| byte == 0));
    }
}
