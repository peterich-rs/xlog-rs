use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{Local, Timelike};
use crossbeam_channel::{bounded, unbounded, Receiver, RecvTimeoutError, Sender};
use thiserror::Error;

use crate::buffer::{recover_blocks, validate_block, BufferError, PersistentBuffer};
use crate::file_manager::{FileManager, FileManagerError};
use crate::platform_tid::current_tid;
use crate::protocol::{
    select_magic, AppendMode, CompressionKind, LogHeader, HEADER_LEN,
    MAGIC_ASYNC_NO_CRYPT_ZLIB_START, MAGIC_ASYNC_NO_CRYPT_ZSTD_START, MAGIC_ASYNC_ZLIB_START,
    MAGIC_ASYNC_ZSTD_START, MAGIC_END, MAGIC_SYNC_NO_CRYPT_ZLIB_START,
    MAGIC_SYNC_NO_CRYPT_ZSTD_START, MAGIC_SYNC_ZLIB_START, MAGIC_SYNC_ZSTD_START,
};

const DEFAULT_ASYNC_FLUSH_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const ASYNC_PENDING_MMAP_PERSIST_EVERY_UPDATES: u32 = 8;
const MIN_LOG_ALIVE_SECONDS: i64 = 24 * 60 * 60;
const EXPIRED_SWEEP_INTERVAL: Duration = Duration::from_secs(2 * 60);
const CACHE_MOVE_INTERVAL: Duration = Duration::from_secs(3 * 60);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum EngineMode {
    Async,
    Sync,
}

#[derive(Debug, Error)]
pub enum AppenderEngineError {
    #[error("buffer error: {0}")]
    Buffer(#[from] BufferError),
    #[error("file manager error: {0}")]
    FileManager(#[from] FileManagerError),
    #[error("engine worker channel closed")]
    ChannelClosed,
    #[error("invalid engine mode for this operation")]
    InvalidMode,
}

struct EngineState {
    file_manager: FileManager,
    buffer: PersistentBuffer,
    max_file_size: u64,
    max_alive_time: i64,
    async_pending_updates_since_persist: u32,
    last_async_buffer_mutation_at: Instant,
    last_expired_sweep_at: Instant,
    last_cache_move_at: Instant,
}

enum EngineCommand {
    Flush {
        move_file: bool,
        ack: Option<Sender<()>>,
    },
    Stop {
        ack: Sender<()>,
    },
}

pub struct AppenderEngine {
    mode: AtomicI32,
    state: Arc<Mutex<EngineState>>,
    buffer_capacity: usize,
    tx: Sender<EngineCommand>,
    pending_async_flush: Arc<AtomicBool>,
    async_flush_epoch: Arc<AtomicU64>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl AppenderEngine {
    pub fn new(
        file_manager: FileManager,
        buffer: PersistentBuffer,
        mode: EngineMode,
        max_file_size: u64,
        max_alive_time: i64,
    ) -> Self {
        Self::new_with_flush_timeout(
            file_manager,
            buffer,
            mode,
            max_file_size,
            max_alive_time,
            DEFAULT_ASYNC_FLUSH_TIMEOUT,
        )
    }

    pub fn new_with_flush_timeout(
        file_manager: FileManager,
        buffer: PersistentBuffer,
        mode: EngineMode,
        max_file_size: u64,
        max_alive_time: i64,
        flush_timeout: Duration,
    ) -> Self {
        let buffer_capacity = buffer.capacity();
        let now = Instant::now();
        let state = Arc::new(Mutex::new(EngineState {
            file_manager,
            buffer,
            max_file_size,
            max_alive_time: max_alive_time.max(MIN_LOG_ALIVE_SECONDS),
            async_pending_updates_since_persist: 0,
            last_async_buffer_mutation_at: now,
            last_expired_sweep_at: now,
            last_cache_move_at: now,
        }));
        if let Ok(mut state_guard) = state.lock() {
            // Keep parity with C++ appender startup behavior: drain recovered mmap data
            // into logfile immediately instead of waiting for next write/flush.
            let _ = flush_pending_locked(&mut state_guard, false, true);
        }
        let (tx, rx) = unbounded();
        let pending_async_flush = Arc::new(AtomicBool::new(false));
        let async_flush_epoch = Arc::new(AtomicU64::new(0));
        let worker_state = Arc::clone(&state);
        let worker_pending_flag = Arc::clone(&pending_async_flush);
        let worker_flush_epoch = Arc::clone(&async_flush_epoch);
        let worker = thread::Builder::new()
            .name("xlog-appender-engine".to_string())
            .spawn(move || {
                run_worker_loop(
                    worker_state,
                    rx,
                    worker_pending_flag,
                    worker_flush_epoch,
                    flush_timeout,
                )
            })
            .expect("spawn appender engine thread");

        Self {
            mode: AtomicI32::new(mode_to_i32(mode)),
            state,
            buffer_capacity,
            tx,
            pending_async_flush,
            async_flush_epoch,
            worker: Mutex::new(Some(worker)),
        }
    }

    pub fn mode(&self) -> EngineMode {
        i32_to_mode(self.mode.load(Ordering::Relaxed))
    }

    pub fn set_mode(&self, mode: EngineMode) -> Result<(), AppenderEngineError> {
        let old = i32_to_mode(self.mode.swap(mode_to_i32(mode), Ordering::Relaxed));
        if old == EngineMode::Async && mode == EngineMode::Sync {
            self.request_flush(false, true)?;
        }
        Ok(())
    }

    pub fn set_max_file_size(&self, max_file_size: u64) {
        if let Ok(mut state) = self.state.lock() {
            state.max_file_size = max_file_size;
        }
    }

    pub fn set_max_alive_time(&self, alive_seconds: i64) {
        if alive_seconds < MIN_LOG_ALIVE_SECONDS {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.max_alive_time = alive_seconds;
        }
    }

    pub fn max_file_size(&self) -> u64 {
        self.state
            .lock()
            .map(|s| s.max_file_size)
            .unwrap_or_default()
    }

    pub fn log_dir(&self) -> Option<String> {
        self.state
            .lock()
            .ok()
            .map(|s| s.file_manager.log_dir().to_string_lossy().to_string())
    }

    pub fn cache_dir(&self) -> Option<String> {
        self.state.lock().ok().and_then(|s| {
            s.file_manager
                .cache_dir()
                .map(|p| p.to_string_lossy().to_string())
        })
    }

    pub fn filepaths_from_timespan(&self, timespan: i32, prefix: &str) -> Vec<String> {
        self.state
            .lock()
            .map(|s| s.file_manager.filepaths_from_timespan(timespan, prefix))
            .unwrap_or_default()
    }

    pub fn make_logfile_name(&self, timespan: i32, prefix: &str) -> Vec<String> {
        self.state
            .lock()
            .map(|s| {
                s.file_manager
                    .make_logfile_name(timespan, prefix, s.max_file_size)
            })
            .unwrap_or_default()
    }

    pub fn write_block(&self, block: &[u8], force_flush: bool) -> Result<(), AppenderEngineError> {
        validate_block(block)?;

        match self.mode() {
            EngineMode::Sync => {
                let state = self.state.lock().expect("state lock poisoned");
                state
                    .file_manager
                    .append_log_bytes(block, state.max_file_size, false, true)?;
            }
            EngineMode::Async => {
                let should_flush = {
                    let mut state = self.state.lock().expect("state lock poisoned");
                    state.async_pending_updates_since_persist =
                        state.async_pending_updates_since_persist.saturating_add(1);
                    let should_persist_mmap = force_flush
                        || state.async_pending_updates_since_persist
                            >= ASYNC_PENDING_MMAP_PERSIST_EVERY_UPDATES;
                    let appended = state.buffer.append_block_with_flush(block, should_persist_mmap)?;
                    if should_persist_mmap {
                        state.async_pending_updates_since_persist = 0;
                    }
                    state.last_async_buffer_mutation_at = Instant::now();

                    if !appended {
                        let _ = flush_pending_locked(&mut state, true, false)?;
                        let appended_after_flush =
                            state.buffer.append_block_with_flush(block, should_persist_mmap)?;
                        if !appended_after_flush {
                            state.file_manager.append_log_bytes(
                                block,
                                state.max_file_size,
                                true,
                                false,
                            )?;
                        }
                    }

                    let threshold = state.buffer.capacity() / 3;
                    force_flush || state.buffer.len() >= threshold
                };

                if should_flush {
                    self.request_flush(false, true)?;
                }
            }
        }
        Ok(())
    }

    pub fn begin_async_pending(&self, header: &LogHeader) -> Result<(), AppenderEngineError> {
        if self.mode() != EngineMode::Async {
            return Err(AppenderEngineError::InvalidMode);
        }
        let mut state = self.state.lock().expect("state lock poisoned");
        state.async_pending_updates_since_persist =
            state.async_pending_updates_since_persist.saturating_add(1);
        let should_persist_mmap =
            state.async_pending_updates_since_persist >= ASYNC_PENDING_MMAP_PERSIST_EVERY_UPDATES;
        state
            .buffer
            .begin_pending_block_with_flush(header, should_persist_mmap)?;
        if should_persist_mmap {
            state.async_pending_updates_since_persist = 0;
        }
        state.last_async_buffer_mutation_at = Instant::now();
        Ok(())
    }

    pub fn append_async_chunk(
        &self,
        truncate_bytes: usize,
        chunk: &[u8],
        end_hour: u8,
        force_flush: bool,
    ) -> Result<(), AppenderEngineError> {
        if self.mode() != EngineMode::Async {
            return Err(AppenderEngineError::InvalidMode);
        }
        let should_flush = {
            let mut state = self.state.lock().expect("state lock poisoned");
            let threshold = state.buffer.capacity() / 3;
            let next_len = state
                .buffer
                .len()
                .saturating_sub(truncate_bytes)
                .saturating_add(chunk.len());
            let should_flush = force_flush || next_len >= threshold;

            state.async_pending_updates_since_persist =
                state.async_pending_updates_since_persist.saturating_add(1);
            let should_persist_mmap = should_flush
                || state.async_pending_updates_since_persist
                    >= ASYNC_PENDING_MMAP_PERSIST_EVERY_UPDATES;
            state
                .buffer
                .append_to_pending_with_flush(truncate_bytes, chunk, end_hour, should_persist_mmap)?;
            if should_persist_mmap {
                state.async_pending_updates_since_persist = 0;
            }
            state.last_async_buffer_mutation_at = Instant::now();
            should_flush
        };

        if should_flush {
            self.request_flush(false, true)?;
        }
        Ok(())
    }

    pub fn finalize_async_pending(
        &self,
        end_hour: u8,
        force_flush: bool,
    ) -> Result<(), AppenderEngineError> {
        if self.mode() != EngineMode::Async {
            return Err(AppenderEngineError::InvalidMode);
        }
        {
            let mut state = self.state.lock().expect("state lock poisoned");
            state
                .buffer
                .finalize_pending_block_with_flush(end_hour, true)?;
            state.async_pending_updates_since_persist = 0;
            state.last_async_buffer_mutation_at = Instant::now();
        }
        if force_flush {
            self.request_flush(false, true)?;
        }
        Ok(())
    }

    pub fn async_buffer_stats(&self) -> Option<(usize, usize)> {
        if self.mode() != EngineMode::Async {
            return None;
        }
        self.state
            .lock()
            .ok()
            .map(|s| (s.buffer.len(), s.buffer.capacity()))
    }

    pub fn async_buffer_snapshot(&self) -> Option<Vec<u8>> {
        if self.mode() != EngineMode::Async {
            return None;
        }
        self.state
            .lock()
            .ok()
            .map(|s| s.buffer.as_bytes().to_vec())
    }

    pub fn buffer_capacity(&self) -> usize {
        self.buffer_capacity
    }

    pub fn async_flush_epoch(&self) -> u64 {
        self.async_flush_epoch.load(Ordering::Acquire)
    }

    pub fn write_async_pending(
        &self,
        pending_bytes: &[u8],
        force_flush: bool,
    ) -> Result<(), AppenderEngineError> {
        if self.mode() != EngineMode::Async {
            return Err(AppenderEngineError::InvalidMode);
        }
        let should_flush = {
            let mut state = self.state.lock().expect("state lock poisoned");
            let threshold = state.buffer.capacity() / 3;
            let should_flush = force_flush || pending_bytes.len() >= threshold;

            state.async_pending_updates_since_persist =
                state.async_pending_updates_since_persist.saturating_add(1);
            let should_persist_mmap = should_flush
                || state.async_pending_updates_since_persist
                    >= ASYNC_PENDING_MMAP_PERSIST_EVERY_UPDATES;
            state
                .buffer
                .replace_bytes_with_flush(pending_bytes, should_persist_mmap)?;
            if should_persist_mmap {
                state.async_pending_updates_since_persist = 0;
            }
            state.last_async_buffer_mutation_at = Instant::now();
            should_flush
        };

        if should_flush {
            self.request_flush(false, true)?;
        }
        Ok(())
    }

    pub fn flush(&self, sync: bool) -> Result<(), AppenderEngineError> {
        if self.mode() == EngineMode::Sync {
            return Ok(());
        }
        self.request_flush(sync, !sync)
    }

    fn request_flush(&self, sync: bool, move_file: bool) -> Result<(), AppenderEngineError> {
        if sync {
            let (ack_tx, ack_rx) = bounded(1);
            self.tx
                .send(EngineCommand::Flush {
                    move_file,
                    ack: Some(ack_tx),
                })
                .map_err(|_| AppenderEngineError::ChannelClosed)?;
            ack_rx
                .recv()
                .map_err(|_| AppenderEngineError::ChannelClosed)?;
            return Ok(());
        }

        if self.pending_async_flush.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.tx
            .send(EngineCommand::Flush {
                move_file,
                ack: None,
            })
            .map_err(|_| {
                self.pending_async_flush.store(false, Ordering::Release);
                AppenderEngineError::ChannelClosed
            })
    }
}

impl Drop for AppenderEngine {
    fn drop(&mut self) {
        let Some(worker) = self.worker.lock().ok().and_then(|mut w| w.take()) else {
            return;
        };

        let (ack_tx, ack_rx) = bounded(1);
        let _ = self.tx.send(EngineCommand::Stop { ack: ack_tx });
        let _ = ack_rx.recv_timeout(Duration::from_secs(2));
        let _ = worker.join();
    }
}

fn run_worker_loop(
    state: Arc<Mutex<EngineState>>,
    rx: Receiver<EngineCommand>,
    pending_async_flush: Arc<AtomicBool>,
    async_flush_epoch: Arc<AtomicU64>,
    flush_timeout: Duration,
) {
    let poll_interval = flush_timeout.min(EXPIRED_SWEEP_INTERVAL);
    loop {
        match rx.recv_timeout(poll_interval) {
            Ok(EngineCommand::Flush { move_file, ack }) => {
                pending_async_flush.store(false, Ordering::Release);
                let flushed = state
                    .lock()
                    .map_err(|_| ())
                    .and_then(|mut s| {
                        flush_pending_locked(&mut s, move_file, false).map_err(|_| ())
                    })
                    .unwrap_or(false);
                if flushed {
                    async_flush_epoch.fetch_add(1, Ordering::AcqRel);
                }
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
            }
            Ok(EngineCommand::Stop { ack }) => {
                pending_async_flush.store(false, Ordering::Release);
                let flushed = state
                    .lock()
                    .map_err(|_| ())
                    .and_then(|mut s| {
                        let flushed = flush_pending_locked(&mut s, true, false).map_err(|_| ())?;
                        maybe_housekeep_locked(&mut s, true).map_err(|_| ())?;
                        Ok(flushed)
                    })
                    .unwrap_or(false);
                if flushed {
                    async_flush_epoch.fetch_add(1, Ordering::AcqRel);
                }
                let _ = ack.send(());
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                let flushed = state
                    .lock()
                    .map_err(|_| ())
                    .and_then(|mut s| handle_timeout_locked(&mut s, flush_timeout).map_err(|_| ()))
                    .unwrap_or(false);
                if flushed {
                    async_flush_epoch.fetch_add(1, Ordering::AcqRel);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn flush_pending_locked(
    state: &mut EngineState,
    move_file: bool,
    write_startup_mmap_tips: bool,
) -> Result<bool, AppenderEngineError> {
    let pending = state.buffer.take_all()?;
    state.async_pending_updates_since_persist = 0;
    let mut flushed = false;
    if !pending.is_empty() {
        let recovered = recover_blocks(&pending);
        if recovered.bytes.is_empty() {
            return Ok(false);
        }
        let sample_header = if recovered.bytes.len() >= HEADER_LEN {
            LogHeader::decode(&recovered.bytes[..HEADER_LEN]).ok()
        } else {
            None
        };
        if write_startup_mmap_tips {
            if let Some(begin) = build_sync_tip_block(sample_header, "~~~~~ begin of mmap ~~~~~\n")
            {
                state
                    .file_manager
                    .append_log_bytes(&begin, state.max_file_size, false, false)?;
            }
        }
        state
            .file_manager
            .append_log_bytes(&recovered.bytes, state.max_file_size, move_file, false)?;
        if write_startup_mmap_tips {
            let end = format!("~~~~~ end of mmap ~~~~~{}\n", current_mark_info());
            if let Some(end_block) = build_sync_tip_block(sample_header, &end) {
                state
                    .file_manager
                    .append_log_bytes(&end_block, state.max_file_size, false, false)?;
            }
        }
        flushed = true;
    }
    Ok(flushed)
}

fn handle_timeout_locked(
    state: &mut EngineState,
    flush_timeout: Duration,
) -> Result<bool, AppenderEngineError> {
    let mut flushed = false;
    if !state.buffer.is_empty() && state.last_async_buffer_mutation_at.elapsed() >= flush_timeout {
        flushed = flush_pending_locked(state, true, false)?;
    }
    maybe_housekeep_locked(state, false)?;
    Ok(flushed)
}

fn maybe_housekeep_locked(state: &mut EngineState, force: bool) -> Result<(), AppenderEngineError> {
    if force || state.last_cache_move_at.elapsed() >= CACHE_MOVE_INTERVAL {
        state
            .file_manager
            .move_old_cache_files(state.max_file_size)?;
        state.last_cache_move_at = Instant::now();
    }
    if force || state.last_expired_sweep_at.elapsed() >= EXPIRED_SWEEP_INTERVAL {
        state
            .file_manager
            .delete_expired_files(state.max_alive_time)?;
        state.last_expired_sweep_at = Instant::now();
    }
    Ok(())
}

fn mode_to_i32(mode: EngineMode) -> i32 {
    match mode {
        EngineMode::Async => 0,
        EngineMode::Sync => 1,
    }
}

fn i32_to_mode(v: i32) -> EngineMode {
    if v == 1 {
        EngineMode::Sync
    } else {
        EngineMode::Async
    }
}

fn magic_profile(magic: u8) -> Option<(CompressionKind, bool)> {
    match magic {
        MAGIC_SYNC_ZLIB_START | MAGIC_ASYNC_ZLIB_START => Some((CompressionKind::Zlib, true)),
        MAGIC_SYNC_NO_CRYPT_ZLIB_START | MAGIC_ASYNC_NO_CRYPT_ZLIB_START => {
            Some((CompressionKind::Zlib, false))
        }
        MAGIC_SYNC_ZSTD_START | MAGIC_ASYNC_ZSTD_START => Some((CompressionKind::Zstd, true)),
        MAGIC_SYNC_NO_CRYPT_ZSTD_START | MAGIC_ASYNC_NO_CRYPT_ZSTD_START => {
            Some((CompressionKind::Zstd, false))
        }
        _ => None,
    }
}

fn build_sync_tip_block(sample_header: Option<LogHeader>, tip: &str) -> Option<Vec<u8>> {
    let sample = sample_header?;
    let (compression, crypt) = magic_profile(sample.magic)?;
    let payload = tip.as_bytes();
    let now_hour = Local::now().hour() as u8;
    let header = LogHeader {
        magic: select_magic(compression, AppendMode::Sync, crypt),
        seq: 0,
        begin_hour: now_hour,
        end_hour: now_hour,
        len: u32::try_from(payload.len()).ok()?,
        client_pubkey: if crypt { sample.client_pubkey } else { [0; 64] },
    };
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len() + 1);
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(payload);
    out.push(MAGIC_END);
    Some(out)
}

fn current_mark_info() -> String {
    let now = Local::now();
    format!(
        "[{},{}][{}]",
        std::process::id(),
        current_tid(),
        now.format("%Y-%m-%d %z %H:%M:%S")
    )
}
