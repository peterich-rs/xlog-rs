use std::fs;

use mars_xlog_core::buffer::DEFAULT_BUFFER_BLOCK_LEN;
use mars_xlog_core::file_manager::FileManager;
use mars_xlog_core::oneshot::{oneshot_flush, FileIoAction};
use mars_xlog_core::protocol::{
    select_magic, AppendMode, CompressionKind, LogHeader, HEADER_LEN, MAGIC_END,
};

fn make_block(payload: &[u8]) -> Vec<u8> {
    let header = LogHeader {
        magic: select_magic(CompressionKind::Zstd, AppendMode::Async, false),
        seq: 1,
        begin_hour: 2,
        end_hour: 2,
        len: payload.len() as u32,
        client_pubkey: [0; 64],
    };
    let mut out = header.encode().to_vec();
    out.extend_from_slice(payload);
    out.push(MAGIC_END);
    out
}

fn parse_payloads(buf: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset + HEADER_LEN < buf.len() {
        let Ok(header) = LogHeader::decode(&buf[offset..offset + HEADER_LEN]) else {
            break;
        };
        let payload_len = header.len as usize;
        let payload_begin = offset + HEADER_LEN;
        let payload_end = payload_begin + payload_len;
        if payload_end >= buf.len() || buf[payload_end] != MAGIC_END {
            break;
        }
        if let Ok(s) = std::str::from_utf8(&buf[payload_begin..payload_end]) {
            out.push(s.to_string());
        }
        offset = payload_end + 1;
    }
    out
}

#[test]
fn oneshot_flush_writes_recovered_bytes_once() {
    let log_dir = tempfile::tempdir().unwrap();
    let manager =
        FileManager::new(log_dir.path().to_path_buf(), None, "oneshot".to_string(), 0).unwrap();

    let block = make_block(b"oneshot-regression");
    let mmap_path = manager.mmap_path();
    let mut raw = vec![0u8; DEFAULT_BUFFER_BLOCK_LEN];
    raw[..block.len()].copy_from_slice(&block);
    fs::write(&mmap_path, &raw).unwrap();

    assert_eq!(
        oneshot_flush(&manager, DEFAULT_BUFFER_BLOCK_LEN, 0),
        FileIoAction::Success
    );
    assert_eq!(
        oneshot_flush(&manager, DEFAULT_BUFFER_BLOCK_LEN, 0),
        FileIoAction::Unnecessary
    );

    let files: Vec<_> = fs::read_dir(log_dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("xlog"))
        .collect();
    assert_eq!(files.len(), 1);

    let bytes = fs::read(&files[0]).unwrap();
    let payloads = parse_payloads(&bytes);
    assert_eq!(payloads.len(), 3);
    assert_eq!(
        payloads[0],
        "~~~~~ begin of mmap from other process ~~~~~\n"
    );
    assert_eq!(payloads[1], "oneshot-regression".to_string());
    assert!(payloads[2].starts_with("~~~~~ end of mmap from other process ~~~~~["));
}

#[test]
fn oneshot_flush_rejects_truncated_mmap_file() {
    let log_dir = tempfile::tempdir().unwrap();
    let manager =
        FileManager::new(log_dir.path().to_path_buf(), None, "oneshot".to_string(), 0).unwrap();

    let block = make_block(b"truncated-mmap");
    // Simulate crash-truncated mmap file: shorter than capacity but with valid leading block.
    fs::write(manager.mmap_path(), &block).unwrap();

    assert_eq!(
        oneshot_flush(&manager, DEFAULT_BUFFER_BLOCK_LEN, 0),
        FileIoAction::ReadFailed
    );

    let files: Vec<_> = fs::read_dir(log_dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("xlog"))
        .collect();
    assert!(files.is_empty());
}

#[test]
fn oneshot_flush_ignores_zero_filled_mmap() {
    let log_dir = tempfile::tempdir().unwrap();
    let manager =
        FileManager::new(log_dir.path().to_path_buf(), None, "oneshot".to_string(), 0).unwrap();

    fs::write(manager.mmap_path(), vec![0u8; DEFAULT_BUFFER_BLOCK_LEN]).unwrap();

    assert_eq!(
        oneshot_flush(&manager, DEFAULT_BUFFER_BLOCK_LEN, 0),
        FileIoAction::Unnecessary
    );
    assert!(manager.mmap_path().exists());
    let files: Vec<_> = fs::read_dir(log_dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("xlog"))
        .collect();
    assert!(files.is_empty());
}

#[test]
fn oneshot_flush_ignores_dirty_but_unrecoverable_mmap() {
    let log_dir = tempfile::tempdir().unwrap();
    let manager =
        FileManager::new(log_dir.path().to_path_buf(), None, "oneshot".to_string(), 0).unwrap();

    let mut raw = vec![0u8; DEFAULT_BUFFER_BLOCK_LEN];
    raw[..8].copy_from_slice(b"not-xlog");
    fs::write(manager.mmap_path(), &raw).unwrap();

    assert_eq!(
        oneshot_flush(&manager, DEFAULT_BUFFER_BLOCK_LEN, 0),
        FileIoAction::Unnecessary
    );
    assert!(manager.mmap_path().exists());

    let files: Vec<_> = fs::read_dir(log_dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("xlog"))
        .collect();
    assert!(files.is_empty());
}
