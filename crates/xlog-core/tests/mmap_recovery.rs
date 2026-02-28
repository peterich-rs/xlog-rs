use std::fs;

use mars_xlog_core::buffer::{PersistentBuffer, DEFAULT_BUFFER_BLOCK_LEN};
use mars_xlog_core::file_manager::FileManager;
use mars_xlog_core::oneshot::{oneshot_flush, FileIoAction};
use mars_xlog_core::protocol::{select_magic, AppendMode, CompressionKind, LogHeader, MAGIC_END};

fn make_block(seq: u16, payload: &[u8]) -> Vec<u8> {
    let header = LogHeader {
        magic: select_magic(CompressionKind::Zlib, AppendMode::Async, false),
        seq,
        begin_hour: 10,
        end_hour: 10,
        len: payload.len() as u32,
        client_pubkey: [0; 64],
    };
    let mut out = header.encode().to_vec();
    out.extend_from_slice(payload);
    out.push(MAGIC_END);
    out
}

#[test]
fn mmap_recovery_keeps_valid_prefix_only() {
    let dir = tempfile::tempdir().unwrap();
    let manager = FileManager::new(dir.path().to_path_buf(), None, "demo".to_string(), 0).unwrap();
    let mmap_path = manager.mmap_path();

    let mut buffer =
        PersistentBuffer::open_with_capacity(&mmap_path, DEFAULT_BUFFER_BLOCK_LEN).unwrap();
    let b1 = make_block(1, b"first");
    let b2 = make_block(2, b"second");
    assert!(buffer.append_block(&b1).unwrap());
    assert!(buffer.append_block(&b2).unwrap());
    drop(buffer);

    let mut raw = fs::read(&mmap_path).unwrap();
    let second_tailer = b1.len() + b2.len() - 1;
    raw[second_tailer] = 0x7f;
    fs::write(&mmap_path, raw).unwrap();

    let mut reopened =
        PersistentBuffer::open_with_capacity(&mmap_path, DEFAULT_BUFFER_BLOCK_LEN).unwrap();
    assert_eq!(reopened.len(), b1.len());
    let recovered = reopened.take_all().unwrap();
    assert_eq!(recovered, b1);
}

#[test]
fn oneshot_flush_recovers_cpp_pending_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let manager = FileManager::new(dir.path().to_path_buf(), None, "demo".to_string(), 0).unwrap();

    let mmap_path = manager.mmap_path();
    let full = make_block(7, b"pending");
    let mut raw = vec![0u8; DEFAULT_BUFFER_BLOCK_LEN];
    raw[..full.len() - 1].copy_from_slice(&full[..full.len() - 1]);
    fs::write(&mmap_path, raw).unwrap();

    let action = oneshot_flush(&manager, DEFAULT_BUFFER_BLOCK_LEN, 0);
    assert_eq!(action, FileIoAction::Success);
    assert!(!mmap_path.exists());

    let mut xlogs: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("xlog"))
        .collect();
    xlogs.sort();
    assert_eq!(xlogs.len(), 1);

    let bytes = fs::read(&xlogs[0]).unwrap();
    assert_eq!(bytes, full);
}

#[test]
fn oneshot_flush_returns_unnecessary_without_mmap() {
    let dir = tempfile::tempdir().unwrap();
    let manager = FileManager::new(dir.path().to_path_buf(), None, "demo".to_string(), 0).unwrap();

    let action = oneshot_flush(&manager, DEFAULT_BUFFER_BLOCK_LEN, 0);
    assert_eq!(action, FileIoAction::Unnecessary);
}
