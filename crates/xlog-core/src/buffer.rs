use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::mmap_store::{MmapStore, MmapStoreError};
use crate::protocol::{update_end_hour_in_place, LogHeader, HEADER_LEN, MAGIC_END};

pub const DEFAULT_BUFFER_BLOCK_LEN: usize = 150 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryResult {
    pub bytes: Vec<u8>,
    pub recovered_pending_block: bool,
    pub dropped_nonzero_tail_bytes: usize,
}

impl RecoveryResult {
    pub fn is_clean(&self) -> bool {
        !self.recovered_pending_block && self.dropped_nonzero_tail_bytes == 0
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct RecoveryScan {
    pub valid_len: usize,
    pub recovered_pending_block: bool,
    pub dropped_nonzero_tail_bytes: usize,
}

#[derive(Debug, Error)]
pub enum BufferError {
    #[error("mmap store error: {0}")]
    Mmap(#[from] MmapStoreError),
    #[error("block is larger than buffer capacity: {block_len} > {capacity}")]
    BlockTooLarge { block_len: usize, capacity: usize },
    #[error("invalid xlog block")]
    InvalidBlock,
    #[error("block length does not fit in usize")]
    BlockLenOverflow,
}

pub struct PersistentBuffer {
    store: MmapStore,
    len: usize,
}

impl PersistentBuffer {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, BufferError> {
        Self::open_with_capacity(path, DEFAULT_BUFFER_BLOCK_LEN)
    }

    pub fn open_with_capacity(
        path: impl Into<PathBuf>,
        capacity: usize,
    ) -> Result<Self, BufferError> {
        let mut store = MmapStore::open_or_create(path, capacity)?;
        let scan = scan_recovery(store.as_slice());
        let len = scan.valid_len + usize::from(scan.recovered_pending_block);
        let needs_repair = scan.recovered_pending_block || scan.dropped_nonzero_tail_bytes > 0;

        if needs_repair {
            let data = store.as_mut_slice();
            if scan.recovered_pending_block {
                data[scan.valid_len] = MAGIC_END;
            }
            if len < data.len() {
                data[len..].fill(0);
            }
            store.flush()?;
        }

        Ok(Self { store, len })
    }

    pub fn path(&self) -> &Path {
        self.store.path()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.store.as_slice()[..self.len]
    }

    pub fn capacity(&self) -> usize {
        self.store.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn recovery_scan(&self) -> RecoveryScan {
        scan_recovery(self.as_bytes())
    }

    pub fn append_block(&mut self, block: &[u8]) -> Result<bool, BufferError> {
        self.append_block_with_flush(block, true)
    }

    pub fn append_block_with_flush(
        &mut self,
        block: &[u8],
        flush: bool,
    ) -> Result<bool, BufferError> {
        validate_block(block)?;

        if block.len() > self.capacity() {
            return Err(BufferError::BlockTooLarge {
                block_len: block.len(),
                capacity: self.capacity(),
            });
        }

        if self.len + block.len() > self.capacity() {
            return Ok(false);
        }

        {
            let data = self.store.as_mut_slice();
            let begin = self.len;
            let end = self.len + block.len();
            data[begin..end].copy_from_slice(block);
            self.len = end;
            if self.len < data.len() {
                data[self.len] = 0;
            }
        }
        if flush {
            self.store.flush()?;
        }
        Ok(true)
    }

    pub fn replace_bytes(&mut self, bytes: &[u8]) -> Result<(), BufferError> {
        self.replace_bytes_with_flush(bytes, true)
    }

    pub fn replace_bytes_with_flush(
        &mut self,
        bytes: &[u8],
        flush: bool,
    ) -> Result<(), BufferError> {
        if bytes.len() > self.capacity() {
            return Err(BufferError::BlockTooLarge {
                block_len: bytes.len(),
                capacity: self.capacity(),
            });
        }

        let old_len = self.len;
        {
            let data = self.store.as_mut_slice();
            if !bytes.is_empty() {
                data[..bytes.len()].copy_from_slice(bytes);
            }
            // Keep trailing bytes untouched when length grows to avoid repeatedly
            // zeroing the full mmap region on every async pending update.
            if bytes.len() < old_len {
                data[bytes.len()..old_len].fill(0);
            } else if bytes.len() < data.len() {
                data[bytes.len()] = 0;
            }
            self.len = bytes.len();
        }
        if flush {
            self.store.flush()?;
        }
        Ok(())
    }

    pub fn take_all(&mut self) -> Result<Vec<u8>, BufferError> {
        let out = self.store.as_slice()[..self.len].to_vec();
        self.clear()?;
        Ok(out)
    }

    pub fn clear_used_with_flush(&mut self, flush: bool) -> Result<(), BufferError> {
        let old_len = self.len;
        {
            let data = self.store.as_mut_slice();
            if old_len > 0 {
                data[..old_len].fill(0);
            } else if !data.is_empty() {
                data[0] = 0;
            }
            self.len = 0;
        }
        if flush {
            self.store.flush()?;
        }
        Ok(())
    }

    pub fn begin_pending_block_with_flush(
        &mut self,
        header: &LogHeader,
        flush: bool,
    ) -> Result<(), BufferError> {
        let encoded = header.encode();
        if encoded.len() > self.capacity() {
            return Err(BufferError::BlockTooLarge {
                block_len: encoded.len(),
                capacity: self.capacity(),
            });
        }

        let old_len = self.len;
        {
            let data = self.store.as_mut_slice();
            data[..HEADER_LEN].copy_from_slice(&encoded);
            if HEADER_LEN < old_len {
                data[HEADER_LEN..old_len].fill(0);
            } else if HEADER_LEN < data.len() {
                data[HEADER_LEN] = 0;
            }
            self.len = HEADER_LEN;
        }
        if flush {
            self.store.flush()?;
        }
        Ok(())
    }

    pub fn append_to_pending_with_flush(
        &mut self,
        truncate_bytes: usize,
        bytes: &[u8],
        end_hour: u8,
        flush: bool,
    ) -> Result<(), BufferError> {
        if self.len < HEADER_LEN || truncate_bytes > self.len.saturating_sub(HEADER_LEN) {
            return Err(BufferError::InvalidBlock);
        }
        let next_len = self
            .len
            .checked_sub(truncate_bytes)
            .and_then(|len| len.checked_add(bytes.len()))
            .ok_or(BufferError::BlockLenOverflow)?;
        if next_len > self.capacity() {
            return Err(BufferError::BlockTooLarge {
                block_len: next_len,
                capacity: self.capacity(),
            });
        }

        let old_len = self.len;
        {
            let data = self.store.as_mut_slice();
            let write_offset = self.len - truncate_bytes;
            if !bytes.is_empty() {
                data[write_offset..write_offset + bytes.len()].copy_from_slice(bytes);
            }
            if next_len < old_len {
                data[next_len..old_len].fill(0);
            } else if next_len < data.len() {
                data[next_len] = 0;
            }
            let payload_len =
                u32::try_from(next_len - HEADER_LEN).map_err(|_| BufferError::BlockLenOverflow)?;
            data[5..9].copy_from_slice(&payload_len.to_le_bytes());
            update_end_hour_in_place(&mut data[..HEADER_LEN], end_hour)
                .map_err(|_| BufferError::InvalidBlock)?;
            self.len = next_len;
        }
        if flush {
            self.store.flush()?;
        }
        Ok(())
    }

    pub fn finalize_pending_block_with_flush(
        &mut self,
        end_hour: u8,
        flush: bool,
    ) -> Result<(), BufferError> {
        if self.len < HEADER_LEN {
            return Err(BufferError::InvalidBlock);
        }
        let next_len = self
            .len
            .checked_add(1)
            .ok_or(BufferError::BlockLenOverflow)?;
        if next_len > self.capacity() {
            return Err(BufferError::BlockTooLarge {
                block_len: next_len,
                capacity: self.capacity(),
            });
        }

        {
            let data = self.store.as_mut_slice();
            data[self.len] = MAGIC_END;
            if next_len < data.len() {
                data[next_len] = 0;
            }
            update_end_hour_in_place(&mut data[..HEADER_LEN], end_hour)
                .map_err(|_| BufferError::InvalidBlock)?;
            self.len = next_len;
        }
        if flush {
            self.store.flush()?;
        }
        Ok(())
    }

    pub fn clear(&mut self) -> Result<(), BufferError> {
        self.store.as_mut_slice().fill(0);
        self.len = 0;
        self.store.flush()?;
        Ok(())
    }

    #[cfg(test)]
    pub fn overwrite_raw(&mut self, bytes: &[u8]) -> Result<(), BufferError> {
        let data = self.store.as_mut_slice();
        let copy_len = bytes.len().min(data.len());
        data.fill(0);
        if copy_len > 0 {
            data[..copy_len].copy_from_slice(&bytes[..copy_len]);
        }
        self.store.flush()?;
        let recovered = recover_blocks(self.store.as_slice());
        self.len = recovered.bytes.len();
        Ok(())
    }
}

pub fn recover_blocks(raw: &[u8]) -> RecoveryResult {
    let scan = scan_recovery(raw);
    let mut out = raw[..scan.valid_len].to_vec();
    if scan.recovered_pending_block {
        out.push(MAGIC_END);
    }

    RecoveryResult {
        bytes: out,
        recovered_pending_block: scan.recovered_pending_block,
        dropped_nonzero_tail_bytes: scan.dropped_nonzero_tail_bytes,
    }
}

pub fn scan_recovery(raw: &[u8]) -> RecoveryScan {
    let mut offset = 0usize;
    let mut recovered_pending_block = false;

    while offset < raw.len() {
        if raw[offset] == 0 {
            break;
        }

        if raw.len() - offset < HEADER_LEN {
            break;
        }

        let header = match LogHeader::decode(&raw[offset..offset + HEADER_LEN]) {
            Ok(header) => header,
            Err(_) => break,
        };

        let payload_len = match usize::try_from(header.len) {
            Ok(v) => v,
            Err(_) => break,
        };

        let payload_end = match offset
            .checked_add(HEADER_LEN)
            .and_then(|v| v.checked_add(payload_len))
        {
            Some(v) => v,
            None => break,
        };
        if payload_end > raw.len() {
            break;
        }

        if payload_end < raw.len() && raw[payload_end] == MAGIC_END {
            offset = payload_end + 1;
            continue;
        }

        // Recover pending block without tailer.
        //
        // Mars C++ `LogCrypt::Fix` trusts header length and keeps the valid prefix
        // even when trailing bytes are dirty/torn, so recovery should not require a
        // zero-only remainder.
        recovered_pending_block = true;
        offset = payload_end;
        break;
    }

    let dropped_nonzero_tail_bytes = raw[offset..].iter().filter(|b| **b != 0).count();

    RecoveryScan {
        valid_len: offset,
        recovered_pending_block,
        dropped_nonzero_tail_bytes,
    }
}

pub fn validate_block(block: &[u8]) -> Result<(), BufferError> {
    if block.len() < HEADER_LEN + 1 {
        return Err(BufferError::InvalidBlock);
    }

    let header = LogHeader::decode(&block[..HEADER_LEN]).map_err(|_| BufferError::InvalidBlock)?;
    let payload_len = usize::try_from(header.len).map_err(|_| BufferError::BlockLenOverflow)?;
    let expected_len = HEADER_LEN
        .checked_add(payload_len)
        .and_then(|v| v.checked_add(1))
        .ok_or(BufferError::BlockLenOverflow)?;
    if block.len() != expected_len {
        return Err(BufferError::InvalidBlock);
    }
    if block[expected_len - 1] != MAGIC_END {
        return Err(BufferError::InvalidBlock);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{recover_blocks, PersistentBuffer};
    use crate::protocol::{select_magic, AppendMode, CompressionKind, LogHeader, MAGIC_END};

    fn make_block(payload: &[u8]) -> Vec<u8> {
        let header = LogHeader {
            magic: select_magic(CompressionKind::Zlib, AppendMode::Async, false),
            seq: 1,
            begin_hour: 1,
            end_hour: 1,
            len: payload.len() as u32,
            client_pubkey: [0; 64],
        };
        let mut out = header.encode().to_vec();
        out.extend_from_slice(payload);
        out.push(MAGIC_END);
        out
    }

    #[test]
    fn recover_cpp_pending_block_without_tailer() {
        let payload = b"hello";
        let full = make_block(payload);
        let mut pending = full[..full.len() - 1].to_vec();
        pending.extend_from_slice(&[0; 64]);

        let recovered = recover_blocks(&pending);
        assert_eq!(recovered.bytes, full);
    }

    #[test]
    fn recover_pending_block_even_with_dirty_tail_bytes() {
        let payload = b"hello";
        let full = make_block(payload);
        let mut pending = full[..full.len() - 1].to_vec();
        pending.extend_from_slice(b"dirty-tail");
        pending.resize(full.len() + 16, 0);

        let recovered = recover_blocks(&pending);
        assert_eq!(recovered.bytes, full);
        assert!(recovered.recovered_pending_block);
        assert!(recovered.dropped_nonzero_tail_bytes >= b"dirty-tail".len());
    }

    #[test]
    fn recover_stops_at_invalid_tail() {
        let b1 = make_block(b"one");
        let mut bytes = b1.clone();
        bytes.extend_from_slice(b"bad-tail");
        let recovered = recover_blocks(&bytes);
        assert_eq!(recovered.bytes, b1);
        assert!(recovered.dropped_nonzero_tail_bytes > 0);
    }

    #[test]
    fn recovery_scan_and_clear_used_track_pending_prefix() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("buffer.mmap3");
        let mut buffer = PersistentBuffer::open_with_capacity(path, 256).unwrap();

        let full = make_block(b"pending");
        buffer
            .replace_bytes_with_flush(&full[..full.len() - 1], false)
            .unwrap();

        let scan = buffer.recovery_scan();
        assert_eq!(scan.valid_len, full.len() - 1);
        assert!(scan.recovered_pending_block);

        buffer.clear_used_with_flush(false).unwrap();
        assert!(buffer.is_empty());
        assert!(buffer.as_bytes().is_empty());
    }
}
