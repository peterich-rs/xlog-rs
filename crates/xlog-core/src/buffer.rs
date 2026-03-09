use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::mmap_store::{MmapStore, MmapStoreError};
use crate::protocol::{update_end_hour_in_place, LogHeader, HEADER_LEN, MAGIC_END};

/// Default mmap buffer size used by the async write path.
pub const DEFAULT_BUFFER_BLOCK_LEN: usize = 150 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
/// Recovered bytes and recovery-side metadata produced from raw mmap contents.
pub struct RecoveryResult {
    /// Recovered xlog bytes, including an appended tail marker when a pending
    /// block could be salvaged.
    pub bytes: Vec<u8>,
    /// Whether recovery turned a tail-less pending block into a complete block.
    pub recovered_pending_block: bool,
    /// Number of non-zero bytes that were ignored after the valid prefix.
    pub dropped_nonzero_tail_bytes: usize,
}

impl RecoveryResult {
    /// Return `true` when recovery did not need to repair or drop any data.
    pub fn is_clean(&self) -> bool {
        !self.recovered_pending_block && self.dropped_nonzero_tail_bytes == 0
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
/// Lightweight recovery scan metadata without materializing recovered bytes.
pub struct RecoveryScan {
    /// Length of the valid prefix already present in the mmap contents.
    pub valid_len: usize,
    /// Whether the valid prefix ended with a recoverable pending block.
    pub recovered_pending_block: bool,
    /// Number of trailing non-zero bytes that were ignored as torn/dirty tail data.
    pub dropped_nonzero_tail_bytes: usize,
}

#[derive(Debug, Error)]
/// Errors returned by [`PersistentBuffer`] and buffer recovery helpers.
pub enum BufferError {
    #[error("mmap store error: {0}")]
    /// Opening, resizing, or flushing the underlying mmap file failed.
    Mmap(#[from] MmapStoreError),
    #[error("block is larger than buffer capacity: {block_len} > {capacity}")]
    /// The requested block or pending image exceeds the configured capacity.
    BlockTooLarge {
        /// Size in bytes of the block that could not fit.
        block_len: usize,
        /// Total mmap capacity available for persisted bytes.
        capacity: usize,
    },
    #[error("invalid xlog block")]
    /// The supplied bytes do not form a structurally valid xlog block.
    InvalidBlock,
    #[error("block length does not fit in usize")]
    /// The encoded payload length overflowed host pointer size arithmetic.
    BlockLenOverflow,
}

/// Mmap-backed buffer used by the async runtime and oneshot recovery flows.
pub struct PersistentBuffer {
    store: MmapStore,
    len: usize,
}

impl PersistentBuffer {
    /// Open a buffer at `path` using [`DEFAULT_BUFFER_BLOCK_LEN`].
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, BufferError> {
        Self::open_with_capacity(path, DEFAULT_BUFFER_BLOCK_LEN)
    }

    /// Open a buffer at `path` with an explicit mmap capacity.
    ///
    /// Existing contents are scanned and repaired in place when they end with a
    /// recoverable pending block or dirty tail bytes.
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

    /// Return the on-disk path of the mmap file.
    pub fn path(&self) -> &Path {
        self.store.path()
    }

    /// Return the currently used byte length.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Return the valid byte prefix currently stored in the buffer.
    pub fn as_bytes(&self) -> &[u8] {
        &self.store.as_slice()[..self.len]
    }

    /// Return the total mmap capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.store.len()
    }

    /// Return `true` when the buffer currently contains no valid bytes.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Scan the current valid prefix and report recovery metadata.
    pub fn recovery_scan(&self) -> RecoveryScan {
        scan_recovery(self.as_bytes())
    }

    /// Append a validated full xlog block and flush the mmap immediately.
    pub fn append_block(&mut self, block: &[u8]) -> Result<bool, BufferError> {
        self.append_block_with_flush(block, true)
    }

    /// Append a validated full xlog block.
    ///
    /// Returns `Ok(false)` when the block is valid but there is not enough
    /// remaining capacity.
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

    /// Replace the valid prefix with `bytes` and flush the mmap immediately.
    pub fn replace_bytes(&mut self, bytes: &[u8]) -> Result<(), BufferError> {
        self.replace_bytes_with_flush(bytes, true)
    }

    /// Replace the valid prefix with `bytes`.
    ///
    /// Trailing bytes are only cleared when the new prefix shrinks, which avoids
    /// repeatedly zeroing the full mmap during async pending updates.
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

    /// Return all valid bytes and clear the buffer.
    pub fn take_all(&mut self) -> Result<Vec<u8>, BufferError> {
        let out = self.store.as_slice()[..self.len].to_vec();
        self.clear()?;
        Ok(out)
    }

    /// Zero only the currently used portion of the mmap and reset the length.
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

    /// Initialize a new pending async block with its encoded header.
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

    /// Replace the tail of the current pending block with new payload bytes.
    ///
    /// `truncate_bytes` removes previously buffered payload bytes before `bytes`
    /// are appended. The header length and end hour are updated in place.
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

    /// Finalize the current pending block by appending [`MAGIC_END`].
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

    /// Zero the entire mmap region and reset the valid length to zero.
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

/// Recover valid xlog bytes from raw mmap contents.
///
/// When the tail marker is missing but the header length still points to a
/// complete payload, a synthetic [`MAGIC_END`] is appended.
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

/// Scan raw mmap contents and report how much of the prefix is recoverable.
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

/// Validate that `block` is a complete xlog block with a matching header length
/// and trailing [`MAGIC_END`].
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
    use super::{recover_blocks, validate_block, BufferError, PersistentBuffer};
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

    #[test]
    fn take_all_returns_bytes_and_clears_underlying_storage() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("buffer.mmap4");
        let mut buffer = PersistentBuffer::open_with_capacity(path, 256).unwrap();

        let block = make_block(b"take-all");
        buffer.append_block_with_flush(&block, false).unwrap();

        let taken = buffer.take_all().unwrap();
        assert_eq!(taken, block);
        assert!(buffer.is_empty());
        assert!(buffer.store.as_slice()[..taken.len()]
            .iter()
            .all(|b| *b == 0));
    }

    #[test]
    fn replace_bytes_shrink_clears_stale_tail_bytes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("buffer.mmap5");
        let mut buffer = PersistentBuffer::open_with_capacity(path, 256).unwrap();

        buffer.replace_bytes_with_flush(b"abcdef", false).unwrap();
        buffer.replace_bytes_with_flush(b"xy", false).unwrap();

        assert_eq!(buffer.as_bytes(), b"xy");
        assert_eq!(&buffer.store.as_slice()[..6], b"xy\0\0\0\0");
    }

    #[test]
    fn pending_block_operations_reject_invalid_state() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("buffer.mmap6");
        let mut buffer = PersistentBuffer::open_with_capacity(path, 256).unwrap();

        assert!(matches!(
            buffer.append_to_pending_with_flush(0, b"payload", 7, false),
            Err(BufferError::InvalidBlock)
        ));
        assert!(matches!(
            buffer.finalize_pending_block_with_flush(7, false),
            Err(BufferError::InvalidBlock)
        ));
    }

    #[test]
    fn validate_block_rejects_missing_tailer_and_length_mismatch() {
        let full = make_block(b"hello");
        let missing_tailer = &full[..full.len() - 1];
        assert!(matches!(
            validate_block(missing_tailer),
            Err(BufferError::InvalidBlock)
        ));

        let bad_len_header = LogHeader {
            magic: select_magic(CompressionKind::Zlib, AppendMode::Async, false),
            seq: 1,
            begin_hour: 1,
            end_hour: 1,
            len: 6,
            client_pubkey: [0; 64],
        };
        let mut mismatched = bad_len_header.encode().to_vec();
        mismatched.extend_from_slice(b"hello");
        mismatched.push(MAGIC_END);

        assert!(matches!(
            validate_block(&mismatched),
            Err(BufferError::InvalidBlock)
        ));
    }
}
