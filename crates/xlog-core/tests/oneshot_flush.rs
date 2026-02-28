use std::fs;

use mars_xlog_core::buffer::DEFAULT_BUFFER_BLOCK_LEN;
use mars_xlog_core::file_manager::FileManager;
use mars_xlog_core::oneshot::{oneshot_flush, FileIoAction};
use mars_xlog_core::protocol::{select_magic, AppendMode, CompressionKind, LogHeader, MAGIC_END};

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
    assert_eq!(bytes, block);
}
