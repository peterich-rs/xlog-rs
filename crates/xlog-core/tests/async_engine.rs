use std::collections::HashSet;
use std::fs;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use chrono::{Datelike, Local};
use mars_xlog_core::appender_engine::{
    AppenderEngine, AppenderEngineError, AsyncFlushReason, EngineMode,
};
use mars_xlog_core::buffer::{PersistentBuffer, DEFAULT_BUFFER_BLOCK_LEN};
use mars_xlog_core::file_manager::FileManager;
use mars_xlog_core::protocol::{
    select_magic, AppendMode, CompressionKind, LogHeader, HEADER_LEN, MAGIC_END,
};

fn make_block(seq: u16, payload: &str) -> Vec<u8> {
    let bytes = payload.as_bytes();
    let header = LogHeader {
        magic: select_magic(CompressionKind::Zlib, AppendMode::Async, false),
        seq,
        begin_hour: 1,
        end_hour: 1,
        len: bytes.len() as u32,
        client_pubkey: [0; 64],
    };
    let mut out = header.encode().to_vec();
    out.extend_from_slice(bytes);
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

fn make_pending_header(seq: u16) -> LogHeader {
    LogHeader {
        magic: select_magic(CompressionKind::Zlib, AppendMode::Async, false),
        seq,
        begin_hour: 1,
        end_hour: 1,
        len: 0,
        client_pubkey: [0; 64],
    }
}

fn merged_xlog_bytes(dir: &std::path::Path) -> Vec<u8> {
    let mut paths: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("xlog"))
        .collect();
    paths.sort();

    let mut merged = Vec::new();
    for path in paths {
        merged.extend_from_slice(&fs::read(path).unwrap());
    }
    merged
}

#[test]
fn async_flush_sync_ack_persists_data() {
    let dir = tempfile::tempdir().unwrap();
    let manager = FileManager::new(dir.path().to_path_buf(), None, "ack".to_string(), 0).unwrap();
    let buffer =
        PersistentBuffer::open_with_capacity(manager.mmap_path(), DEFAULT_BUFFER_BLOCK_LEN)
            .unwrap();
    let engine = AppenderEngine::new(manager, buffer, EngineMode::Async, 0, 10 * 24 * 60 * 60);

    engine.write_block(&make_block(1, "A"), false).unwrap();
    engine.write_block(&make_block(2, "B"), false).unwrap();
    engine.flush(true).unwrap();

    let mut paths: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("xlog"))
        .collect();
    paths.sort();
    assert_eq!(paths.len(), 1);
    let bytes = fs::read(&paths[0]).unwrap();
    let payloads = parse_payloads(&bytes);
    assert_eq!(payloads, vec!["A".to_string(), "B".to_string()]);
}

#[test]
fn startup_drains_recovered_mmap_bytes_to_logfile() {
    let dir = tempfile::tempdir().unwrap();
    let manager =
        FileManager::new(dir.path().to_path_buf(), None, "startup".to_string(), 0).unwrap();
    let mmap_path = manager.mmap_path();

    {
        let mut buffer =
            PersistentBuffer::open_with_capacity(&mmap_path, DEFAULT_BUFFER_BLOCK_LEN).unwrap();
        assert!(buffer.append_block(&make_block(1, "RECOVERED")).unwrap());
    }

    let buffer =
        PersistentBuffer::open_with_capacity(&mmap_path, DEFAULT_BUFFER_BLOCK_LEN).unwrap();
    let engine = AppenderEngine::new(manager, buffer, EngineMode::Async, 0, 10 * 24 * 60 * 60);
    engine.flush(true).unwrap();

    let mut paths: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("xlog"))
        .collect();
    paths.sort();
    assert_eq!(paths.len(), 1);
    let payloads = parse_payloads(&fs::read(&paths[0]).unwrap());
    assert_eq!(payloads.len(), 3);
    assert_eq!(payloads[0], "~~~~~ begin of mmap ~~~~~\n");
    assert_eq!(payloads[1], "RECOVERED".to_string());
    assert!(payloads[2].starts_with("~~~~~ end of mmap ~~~~~["));
}

#[test]
fn concurrent_async_writes_keep_all_messages() {
    let dir = tempfile::tempdir().unwrap();
    let manager =
        FileManager::new(dir.path().to_path_buf(), None, "concurrent".to_string(), 0).unwrap();
    let buffer =
        PersistentBuffer::open_with_capacity(manager.mmap_path(), DEFAULT_BUFFER_BLOCK_LEN)
            .unwrap();
    let engine = Arc::new(AppenderEngine::new(
        manager,
        buffer,
        EngineMode::Async,
        0,
        10 * 24 * 60 * 60,
    ));

    let threads = 6u16;
    let per_thread = 80u16;
    let mut handles = Vec::new();
    for t in 0..threads {
        let engine = Arc::clone(&engine);
        handles.push(thread::spawn(move || {
            for i in 0..per_thread {
                let payload = format!("T{t:02}-{i:03}");
                let seq = 1 + t * per_thread + i;
                engine
                    .write_block(&make_block(seq, &payload), false)
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    engine.flush(true).unwrap();

    let mut merged = Vec::new();
    for entry in fs::read_dir(dir.path()).unwrap().flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) == Some("xlog") {
            merged.extend_from_slice(&fs::read(path).unwrap());
        }
    }

    let got: HashSet<_> = parse_payloads(&merged).into_iter().collect();
    assert_eq!(got.len(), (threads as usize) * (per_thread as usize));
    assert!(got.contains("T00-000"));
    assert!(got.contains("T05-079"));
}

#[test]
fn async_timeout_flushes_pending_block_without_explicit_flush() {
    let dir = tempfile::tempdir().unwrap();
    let manager =
        FileManager::new(dir.path().to_path_buf(), None, "timeout".to_string(), 0).unwrap();
    let buffer =
        PersistentBuffer::open_with_capacity(manager.mmap_path(), DEFAULT_BUFFER_BLOCK_LEN)
            .unwrap();
    let engine = AppenderEngine::new_with_flush_timeout(
        manager,
        buffer,
        EngineMode::Async,
        0,
        10 * 24 * 60 * 60,
        Duration::from_millis(120),
    );

    let block = make_block(1, "TIMEOUT-FLUSH");
    let pending_without_tailer = &block[..block.len() - 1];
    engine
        .write_async_pending(pending_without_tailer, false)
        .unwrap();

    thread::sleep(Duration::from_millis(360));
    drop(engine);

    let mut paths: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("xlog"))
        .collect();
    paths.sort();
    assert_eq!(paths.len(), 1);
    let payloads = parse_payloads(&fs::read(&paths[0]).unwrap());
    assert!(payloads.iter().any(|s| s.contains("TIMEOUT-FLUSH")));
}

#[test]
fn startup_recovers_pending_block_without_tailer() {
    let dir = tempfile::tempdir().unwrap();
    let manager =
        FileManager::new(dir.path().to_path_buf(), None, "restart".to_string(), 0).unwrap();
    let mmap_path = manager.mmap_path();

    {
        let mut buffer =
            PersistentBuffer::open_with_capacity(&mmap_path, DEFAULT_BUFFER_BLOCK_LEN).unwrap();
        let block = make_block(7, "PENDING-WITHOUT-TAILER");
        let pending_without_tailer = &block[..block.len() - 1];
        buffer
            .replace_bytes_with_flush(pending_without_tailer, true)
            .unwrap();
    }

    let buffer =
        PersistentBuffer::open_with_capacity(&mmap_path, DEFAULT_BUFFER_BLOCK_LEN).unwrap();
    let engine = AppenderEngine::new(manager, buffer, EngineMode::Async, 0, 10 * 24 * 60 * 60);
    engine.flush(true).unwrap();

    let mut paths: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("xlog"))
        .collect();
    paths.sort();
    assert_eq!(paths.len(), 1);
    let payloads = parse_payloads(&fs::read(&paths[0]).unwrap());
    assert!(payloads.iter().any(|s| s == "~~~~~ begin of mmap ~~~~~\n"));
    assert!(payloads
        .iter()
        .any(|s| s.contains("PENDING-WITHOUT-TAILER")));
    assert!(payloads
        .iter()
        .any(|s| s.starts_with("~~~~~ end of mmap ~~~~~[")));
}

#[test]
fn flush_sync_keeps_cache_file_without_move() {
    let dir = tempfile::tempdir().unwrap();
    let log_dir = dir.path().join("log");
    let cache_dir = dir.path().join("cache");
    let manager = FileManager::new(
        log_dir.clone(),
        Some(cache_dir.clone()),
        "flushsync".to_string(),
        1,
    )
    .unwrap();
    let mmap_path = manager.mmap_path();

    let now = Local::now();
    let file_name = format!(
        "flushsync_{:04}{:02}{:02}.xlog",
        now.year(),
        now.month(),
        now.day()
    );
    let log_path = log_dir.join(&file_name);
    let cache_path = cache_dir.join(&file_name);
    fs::write(&log_path, b"log-base").unwrap();
    fs::write(&cache_path, b"cache-base").unwrap();

    let buffer =
        PersistentBuffer::open_with_capacity(&mmap_path, DEFAULT_BUFFER_BLOCK_LEN).unwrap();
    let engine = AppenderEngine::new(manager, buffer, EngineMode::Async, 0, 10 * 24 * 60 * 60);
    let block = make_block(11, "SYNC-FLUSH");
    engine.write_block(&block, false).unwrap();
    engine.flush(true).unwrap();

    assert!(cache_path.exists());
    assert_eq!(fs::read(&log_path).unwrap(), b"log-base".to_vec());
    let cache_bytes = fs::read(&cache_path).unwrap();
    assert!(cache_bytes.starts_with(b"cache-base"));
    assert!(cache_bytes.ends_with(&block));
}

#[test]
fn set_mode_async_to_sync_flushes_pending_data() {
    let dir = tempfile::tempdir().unwrap();
    let manager = FileManager::new(dir.path().to_path_buf(), None, "mode".to_string(), 0).unwrap();
    let buffer =
        PersistentBuffer::open_with_capacity(manager.mmap_path(), DEFAULT_BUFFER_BLOCK_LEN)
            .unwrap();
    let engine = AppenderEngine::new(manager, buffer, EngineMode::Async, 0, 10 * 24 * 60 * 60);

    engine
        .write_block(&make_block(1, "MODE-SWITCH"), false)
        .unwrap();
    engine.set_mode(EngineMode::Sync).unwrap();

    assert_eq!(engine.mode(), EngineMode::Sync);
    let payloads = parse_payloads(&merged_xlog_bytes(dir.path()));
    assert!(payloads.iter().any(|payload| payload == "MODE-SWITCH"));
}

#[test]
fn pending_block_chunk_rewrite_persists_final_payload() {
    let dir = tempfile::tempdir().unwrap();
    let manager =
        FileManager::new(dir.path().to_path_buf(), None, "pending".to_string(), 0).unwrap();
    let buffer =
        PersistentBuffer::open_with_capacity(manager.mmap_path(), DEFAULT_BUFFER_BLOCK_LEN)
            .unwrap();
    let engine = AppenderEngine::new(manager, buffer, EngineMode::Async, 0, 10 * 24 * 60 * 60);

    engine.begin_async_pending(&make_pending_header(7)).unwrap();
    engine.append_async_chunk(0, b"abcd", 1, false).unwrap();
    engine.append_async_chunk(2, b"XY", 1, false).unwrap();
    engine.finalize_async_pending(1, false).unwrap();
    engine.flush(true).unwrap();

    let payloads = parse_payloads(&merged_xlog_bytes(dir.path()));
    assert!(payloads.iter().any(|payload| payload == "abXY"));
}

#[test]
fn sync_mode_rejects_async_pending_apis() {
    let dir = tempfile::tempdir().unwrap();
    let manager = FileManager::new(dir.path().to_path_buf(), None, "sync".to_string(), 0).unwrap();
    let buffer =
        PersistentBuffer::open_with_capacity(manager.mmap_path(), DEFAULT_BUFFER_BLOCK_LEN)
            .unwrap();
    let engine = AppenderEngine::new(manager, buffer, EngineMode::Sync, 0, 10 * 24 * 60 * 60);

    assert!(matches!(
        engine.begin_async_pending(&make_pending_header(3)),
        Err(AppenderEngineError::InvalidMode)
    ));
    assert!(matches!(
        engine.append_async_chunk(0, b"abc", 1, false),
        Err(AppenderEngineError::InvalidMode)
    ));
    assert!(matches!(
        engine.finalize_async_pending(1, false),
        Err(AppenderEngineError::InvalidMode)
    ));
    assert!(matches!(
        engine.write_async_pending(b"pending-bytes", false),
        Err(AppenderEngineError::InvalidMode)
    ));
}

#[test]
fn flush_with_reason_updates_async_flush_state() {
    let dir = tempfile::tempdir().unwrap();
    let manager =
        FileManager::new(dir.path().to_path_buf(), None, "reason".to_string(), 0).unwrap();
    let buffer =
        PersistentBuffer::open_with_capacity(manager.mmap_path(), DEFAULT_BUFFER_BLOCK_LEN)
            .unwrap();
    let engine = AppenderEngine::new(manager, buffer, EngineMode::Async, 0, 10 * 24 * 60 * 60);

    let epoch_before = engine.async_flush_epoch();
    engine
        .write_block(&make_block(1, "FLUSH-REASON"), false)
        .unwrap();
    engine
        .flush_with_reason(true, AsyncFlushReason::Explicit)
        .unwrap();

    let (epoch_after, reason) = engine.async_flush_state();
    assert!(epoch_after > epoch_before);
    assert_eq!(reason, AsyncFlushReason::Explicit);
}

#[test]
fn async_buffer_snapshot_and_stats_reflect_pending_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let manager =
        FileManager::new(dir.path().to_path_buf(), None, "snapshot".to_string(), 0).unwrap();
    let buffer =
        PersistentBuffer::open_with_capacity(manager.mmap_path(), DEFAULT_BUFFER_BLOCK_LEN)
            .unwrap();
    let engine = AppenderEngine::new(manager, buffer, EngineMode::Async, 0, 10 * 24 * 60 * 60);

    engine
        .begin_async_pending(&make_pending_header(12))
        .unwrap();
    engine.append_async_chunk(0, b"xyz", 5, false).unwrap();

    let (used, capacity) = engine.async_buffer_stats().unwrap();
    assert_eq!(capacity, DEFAULT_BUFFER_BLOCK_LEN);
    assert_eq!(used, HEADER_LEN + 3);

    let snapshot = engine.async_buffer_snapshot().unwrap();
    assert_eq!(snapshot.len(), used);
    let header = LogHeader::decode(&snapshot[..HEADER_LEN]).unwrap();
    assert_eq!(header.seq, 12);
    assert_eq!(header.end_hour, 5);
    assert_eq!(header.len, 3);
    assert_eq!(&snapshot[HEADER_LEN..], b"xyz");

    engine.flush(true).unwrap();
}

#[test]
fn sync_mode_hides_async_buffer_observability_apis() {
    let dir = tempfile::tempdir().unwrap();
    let manager = FileManager::new(dir.path().to_path_buf(), None, "sync".to_string(), 0).unwrap();
    let buffer =
        PersistentBuffer::open_with_capacity(manager.mmap_path(), DEFAULT_BUFFER_BLOCK_LEN)
            .unwrap();
    let engine = AppenderEngine::new(manager, buffer, EngineMode::Sync, 0, 10 * 24 * 60 * 60);

    assert!(engine.async_buffer_stats().is_none());
    assert!(engine.async_buffer_snapshot().is_none());
}
