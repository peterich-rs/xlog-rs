use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{Local, Timelike};
use crossbeam_channel::{bounded, unbounded, Receiver, RecvTimeoutError, Sender};
use thiserror::Error;

use crate::buffer::{validate_block, BufferError, PersistentBuffer};
use crate::file_manager::{FileManager, FileManagerError};
use crate::platform_tid::current_tid;
use crate::protocol::{
    select_magic, AppendMode, CompressionKind, LogHeader, HEADER_LEN,
    MAGIC_ASYNC_NO_CRYPT_ZLIB_START, MAGIC_ASYNC_NO_CRYPT_ZSTD_START, MAGIC_ASYNC_ZLIB_START,
    MAGIC_ASYNC_ZSTD_START, MAGIC_END, MAGIC_SYNC_NO_CRYPT_ZLIB_START,
    MAGIC_SYNC_NO_CRYPT_ZSTD_START, MAGIC_SYNC_ZLIB_START, MAGIC_SYNC_ZSTD_START,
};

const DEFAULT_ASYNC_FLUSH_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const ASYNC_PENDING_MMAP_PERSIST_EVERY_UPDATES: u32 = 64;
const ASYNC_PENDING_MMAP_PERSIST_EVERY_BYTES: usize = 64 * 1024;
const ASYNC_PENDING_MMAP_PERSIST_INTERVAL: Duration = Duration::from_millis(500);
const ASYNC_FLUSH_RETRY_DELAY: Duration = Duration::from_micros(100);
const ASYNC_BUFFER_FLUSH_THRESHOLD_NUM: usize = 1;
const ASYNC_BUFFER_FLUSH_THRESHOLD_DEN: usize = 3;
const MIN_LOG_ALIVE_SECONDS: i64 = 24 * 60 * 60;
const EXPIRED_SWEEP_INTERVAL: Duration = Duration::from_secs(2 * 60);
const CACHE_MOVE_INTERVAL: Duration = Duration::from_secs(3 * 60);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum EngineMode {
    Async,
    Sync,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum AsyncFlushReason {
    Unknown,
    Threshold,
    Explicit,
    Timeout,
    Stop,
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
    async_pending_bytes_since_persist: usize,
    last_async_buffer_mutation_at: Instant,
    last_async_buffer_persist_at: Instant,
    last_expired_sweep_at: Instant,
    last_cache_move_at: Instant,
}

enum EngineCommand {
    Flush {
        move_file: bool,
        ack: Option<Sender<()>>,
        reason: AsyncFlushReason,
    },
    Stop {
        ack: Sender<()>,
    },
}

pub struct AppenderEngine {
    mode: AtomicI32,
    file_manager: FileManager,
    state: Arc<Mutex<EngineState>>,
    buffer_capacity: usize,
    max_file_size: AtomicU64,
    max_alive_time: AtomicI64,
    tx: Sender<EngineCommand>,
    pending_async_flush: Arc<AtomicBool>,
    async_flush_epoch: Arc<AtomicU64>,
    async_flush_reason: Arc<AtomicU8>,
    async_flush_requeue_count: Arc<AtomicU64>,
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
        let clamped_alive_time = max_alive_time.max(MIN_LOG_ALIVE_SECONDS);
        let state = Arc::new(Mutex::new(EngineState {
            file_manager: file_manager.clone(),
            buffer,
            max_file_size,
            max_alive_time: clamped_alive_time,
            async_pending_updates_since_persist: 0,
            async_pending_bytes_since_persist: 0,
            last_async_buffer_mutation_at: now,
            last_async_buffer_persist_at: now,
            last_expired_sweep_at: now,
            last_cache_move_at: now,
        }));
        if let Ok(mut state_guard) = state.lock() {
            // Keep parity with C++ appender startup behavior: drain recovered mmap data
            // into logfile immediately instead of waiting for next write/flush.
            let _ = flush_pending_locked(&mut state_guard, false, true);
        }
        let (tx, rx) = unbounded();
        let worker_tx = tx.clone();
        let pending_async_flush = Arc::new(AtomicBool::new(false));
        let async_flush_epoch = Arc::new(AtomicU64::new(0));
        let async_flush_reason = Arc::new(AtomicU8::new(async_flush_reason_to_u8(
            AsyncFlushReason::Unknown,
        )));
        let async_flush_requeue_count = Arc::new(AtomicU64::new(0));
        let worker_state = Arc::clone(&state);
        let worker_pending_flag = Arc::clone(&pending_async_flush);
        let worker_flush_epoch = Arc::clone(&async_flush_epoch);
        let worker_flush_reason = Arc::clone(&async_flush_reason);
        let worker_flush_requeue_count = Arc::clone(&async_flush_requeue_count);
        let worker = thread::Builder::new()
            .name("xlog-appender-engine".to_string())
            .spawn(move || {
                run_worker_loop(
                    worker_state,
                    rx,
                    worker_tx,
                    worker_pending_flag,
                    worker_flush_epoch,
                    worker_flush_reason,
                    worker_flush_requeue_count,
                    flush_timeout,
                )
            })
            .expect("spawn appender engine thread");

        Self {
            mode: AtomicI32::new(mode_to_i32(mode)),
            file_manager,
            state,
            buffer_capacity,
            max_file_size: AtomicU64::new(max_file_size),
            max_alive_time: AtomicI64::new(clamped_alive_time),
            tx,
            pending_async_flush,
            async_flush_epoch,
            async_flush_reason,
            async_flush_requeue_count,
            worker: Mutex::new(Some(worker)),
        }
    }

    pub fn mode(&self) -> EngineMode {
        i32_to_mode(self.mode.load(Ordering::Relaxed))
    }

    pub fn set_mode(&self, mode: EngineMode) -> Result<(), AppenderEngineError> {
        let old = i32_to_mode(self.mode.swap(mode_to_i32(mode), Ordering::Relaxed));
        if old == EngineMode::Async && mode == EngineMode::Sync {
            self.request_flush(true, true, AsyncFlushReason::Explicit)?;
        }
        Ok(())
    }

    pub fn set_max_file_size(&self, max_file_size: u64) {
        self.max_file_size.store(max_file_size, Ordering::Relaxed);
        if let Ok(mut state) = self.state.lock() {
            state.max_file_size = max_file_size;
        }
    }

    pub fn set_max_alive_time(&self, alive_seconds: i64) {
        if alive_seconds < MIN_LOG_ALIVE_SECONDS {
            return;
        }
        self.max_alive_time.store(alive_seconds, Ordering::Relaxed);
        if let Ok(mut state) = self.state.lock() {
            state.max_alive_time = alive_seconds;
        }
    }

    pub fn max_file_size(&self) -> u64 {
        self.max_file_size.load(Ordering::Relaxed)
    }

    pub fn log_dir(&self) -> Option<String> {
        Some(self.file_manager.log_dir().to_string_lossy().to_string())
    }

    pub fn cache_dir(&self) -> Option<String> {
        self.file_manager
            .cache_dir()
            .map(|p| p.to_string_lossy().to_string())
    }

    pub fn filepaths_from_timespan(&self, timespan: i32, prefix: &str) -> Vec<String> {
        self.file_manager.filepaths_from_timespan(timespan, prefix)
    }

    pub fn make_logfile_name(&self, timespan: i32, prefix: &str) -> Vec<String> {
        self.file_manager
            .make_logfile_name(timespan, prefix, self.max_file_size())
    }

    pub fn write_block(&self, block: &[u8], force_flush: bool) -> Result<(), AppenderEngineError> {
        validate_block(block)?;

        match self.mode() {
            EngineMode::Sync => {
                self.file_manager.append_log_bytes(
                    block,
                    self.max_file_size.load(Ordering::Relaxed),
                    false,
                    true,
                )?;
            }
            EngineMode::Async => {
                let should_flush = {
                    let mut state = self.state.lock().expect("state lock poisoned");
                    state.async_pending_updates_since_persist =
                        state.async_pending_updates_since_persist.saturating_add(1);
                    state.async_pending_bytes_since_persist = state
                        .async_pending_bytes_since_persist
                        .saturating_add(block.len());
                    let should_persist_mmap = should_persist_async_mmap(&state, force_flush);
                    let appended = state
                        .buffer
                        .append_block_with_flush(block, should_persist_mmap)?;
                    if should_persist_mmap {
                        mark_async_mmap_persisted(&mut state);
                    }
                    state.last_async_buffer_mutation_at = Instant::now();

                    if !appended {
                        let _ = flush_pending_locked(&mut state, true, false)?;
                        let appended_after_flush = state
                            .buffer
                            .append_block_with_flush(block, should_persist_mmap)?;
                        if !appended_after_flush {
                            state.file_manager.append_log_bytes(
                                block,
                                state.max_file_size,
                                true,
                                false,
                            )?;
                        }
                    }

                    let threshold = async_buffer_flush_threshold(state.buffer.capacity());
                    force_flush || state.buffer.len() >= threshold
                };

                if should_flush {
                    self.request_flush(false, true, AsyncFlushReason::Threshold)?;
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
        state.async_pending_bytes_since_persist = state
            .async_pending_bytes_since_persist
            .saturating_add(HEADER_LEN);
        let should_persist_mmap = should_persist_async_mmap(&state, false);
        state
            .buffer
            .begin_pending_block_with_flush(header, should_persist_mmap)?;
        if should_persist_mmap {
            mark_async_mmap_persisted(&mut state);
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
            let threshold = async_buffer_flush_threshold(state.buffer.capacity());
            let next_len = state
                .buffer
                .len()
                .saturating_sub(truncate_bytes)
                .saturating_add(chunk.len());
            let should_flush = force_flush || next_len >= threshold;

            state.async_pending_updates_since_persist =
                state.async_pending_updates_since_persist.saturating_add(1);
            let bytes_delta = chunk.len().saturating_sub(truncate_bytes);
            state.async_pending_bytes_since_persist = state
                .async_pending_bytes_since_persist
                .saturating_add(bytes_delta);
            let should_persist_mmap = should_persist_async_mmap(&state, force_flush);
            state.buffer.append_to_pending_with_flush(
                truncate_bytes,
                chunk,
                end_hour,
                should_persist_mmap,
            )?;
            if should_persist_mmap {
                mark_async_mmap_persisted(&mut state);
            }
            state.last_async_buffer_mutation_at = Instant::now();
            should_flush
        };

        if should_flush {
            self.request_flush(false, true, AsyncFlushReason::Threshold)?;
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
            state.async_pending_updates_since_persist =
                state.async_pending_updates_since_persist.saturating_add(1);
            state.async_pending_bytes_since_persist =
                state.async_pending_bytes_since_persist.saturating_add(1);
            let should_persist_mmap = should_persist_async_mmap(&state, force_flush);
            state
                .buffer
                .finalize_pending_block_with_flush(end_hour, should_persist_mmap)?;
            if should_persist_mmap {
                mark_async_mmap_persisted(&mut state);
            }
            state.last_async_buffer_mutation_at = Instant::now();
        }
        if force_flush {
            self.request_flush(false, true, AsyncFlushReason::Threshold)?;
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
        self.state.lock().ok().map(|s| s.buffer.as_bytes().to_vec())
    }

    pub fn buffer_capacity(&self) -> usize {
        self.buffer_capacity
    }

    pub fn async_flush_epoch(&self) -> u64 {
        self.async_flush_epoch.load(Ordering::Acquire)
    }

    pub fn async_flush_state(&self) -> (u64, AsyncFlushReason) {
        loop {
            let epoch_before = self.async_flush_epoch.load(Ordering::Acquire);
            let reason = u8_to_async_flush_reason(self.async_flush_reason.load(Ordering::Acquire));
            let epoch_after = self.async_flush_epoch.load(Ordering::Acquire);
            if epoch_before == epoch_after {
                return (epoch_after, reason);
            }
        }
    }

    pub fn take_async_flush_requeue_count(&self) -> u64 {
        self.async_flush_requeue_count.swap(0, Ordering::AcqRel)
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
            let threshold = async_buffer_flush_threshold(state.buffer.capacity());
            let should_flush = force_flush || pending_bytes.len() >= threshold;

            state.async_pending_updates_since_persist =
                state.async_pending_updates_since_persist.saturating_add(1);
            state.async_pending_bytes_since_persist = state
                .async_pending_bytes_since_persist
                .saturating_add(pending_bytes.len());
            let should_persist_mmap = should_persist_async_mmap(&state, force_flush);
            state
                .buffer
                .replace_bytes_with_flush(pending_bytes, should_persist_mmap)?;
            if should_persist_mmap {
                mark_async_mmap_persisted(&mut state);
            }
            state.last_async_buffer_mutation_at = Instant::now();
            should_flush
        };

        if should_flush {
            self.request_flush(false, true, AsyncFlushReason::Threshold)?;
        }
        Ok(())
    }

    pub fn flush(&self, sync: bool) -> Result<(), AppenderEngineError> {
        self.flush_with_reason(sync, AsyncFlushReason::Explicit)
    }

    pub fn flush_with_reason(
        &self,
        sync: bool,
        reason: AsyncFlushReason,
    ) -> Result<(), AppenderEngineError> {
        if self.mode() == EngineMode::Sync {
            if sync {
                self.file_manager.flush_active_file_buffer()?;
            }
            return Ok(());
        }
        self.request_flush(sync, !sync, reason)
    }

    fn request_flush(
        &self,
        sync: bool,
        move_file: bool,
        reason: AsyncFlushReason,
    ) -> Result<(), AppenderEngineError> {
        if sync {
            let (ack_tx, ack_rx) = bounded(1);
            self.tx
                .send(EngineCommand::Flush {
                    move_file,
                    ack: Some(ack_tx),
                    reason,
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
                reason,
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
    tx: Sender<EngineCommand>,
    pending_async_flush: Arc<AtomicBool>,
    async_flush_epoch: Arc<AtomicU64>,
    async_flush_reason: Arc<AtomicU8>,
    async_flush_requeue_count: Arc<AtomicU64>,
    flush_timeout: Duration,
) {
    let poll_interval = flush_timeout.min(EXPIRED_SWEEP_INTERVAL);
    loop {
        match rx.recv_timeout(poll_interval) {
            Ok(EngineCommand::Flush {
                move_file,
                ack,
                reason,
            }) => {
                let flushed = if ack.is_some() {
                    pending_async_flush.store(false, Ordering::Release);
                    state
                        .lock()
                        .map_err(|_| ())
                        .and_then(|mut s| {
                            flush_pending_locked(&mut s, move_file, false).map_err(|_| ())
                        })
                        .unwrap_or(false)
                } else {
                    match state.try_lock() {
                        Ok(mut s) => {
                            pending_async_flush.store(false, Ordering::Release);
                            flush_pending_locked(&mut s, move_file, false)
                                .map_err(|_| ())
                                .unwrap_or(false)
                        }
                        Err(_) => {
                            async_flush_requeue_count.fetch_add(1, Ordering::Relaxed);
                            thread::sleep(ASYNC_FLUSH_RETRY_DELAY);
                            let _ = tx.send(EngineCommand::Flush {
                                move_file,
                                ack: None,
                                reason,
                            });
                            continue;
                        }
                    }
                };
                if flushed {
                    async_flush_reason.store(async_flush_reason_to_u8(reason), Ordering::Release);
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
                    async_flush_reason.store(
                        async_flush_reason_to_u8(AsyncFlushReason::Stop),
                        Ordering::Release,
                    );
                    async_flush_epoch.fetch_add(1, Ordering::AcqRel);
                }
                let _ = ack.send(());
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                let flushed = state
                    .try_lock()
                    .map_err(|_| ())
                    .and_then(|mut s| handle_timeout_locked(&mut s, flush_timeout).map_err(|_| ()))
                    .unwrap_or(false);
                if flushed {
                    async_flush_reason.store(
                        async_flush_reason_to_u8(AsyncFlushReason::Timeout),
                        Ordering::Release,
                    );
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
    mark_async_mmap_persisted(state);
    let mut flushed = false;
    if !state.buffer.is_empty() {
        let scan = state.buffer.recovery_scan();
        if scan.valid_len == 0 {
            state.buffer.clear_used_with_flush(true)?;
            return Ok(false);
        }
        let keep_open = false;

        let sample_header = {
            let pending = state.buffer.as_bytes();
            if scan.valid_len >= HEADER_LEN {
                LogHeader::decode(&pending[..HEADER_LEN]).ok()
            } else {
                None
            }
        };
        if write_startup_mmap_tips {
            if let Some(begin) = build_sync_tip_block(sample_header, "~~~~~ begin of mmap ~~~~~\n")
            {
                state.file_manager.append_log_bytes(
                    &begin,
                    state.max_file_size,
                    false,
                    keep_open,
                )?;
            }
        }

        {
            let pending = state.buffer.as_bytes();
            if scan.recovered_pending_block {
                // Keep the recovered block contiguous so another process cannot
                // interleave between payload bytes and the repaired tail marker.
                let mut recovered = Vec::with_capacity(scan.valid_len.saturating_add(1));
                recovered.extend_from_slice(&pending[..scan.valid_len]);
                recovered.push(MAGIC_END);
                state.file_manager.append_log_bytes(
                    &recovered,
                    state.max_file_size,
                    move_file,
                    keep_open,
                )?;
            } else {
                state.file_manager.append_log_bytes(
                    &pending[..scan.valid_len],
                    state.max_file_size,
                    move_file,
                    keep_open,
                )?;
            }
        }
        state.buffer.clear_used_with_flush(true)?;

        if write_startup_mmap_tips {
            let end = format!("~~~~~ end of mmap ~~~~~{}\n", current_mark_info());
            if let Some(end_block) = build_sync_tip_block(sample_header, &end) {
                state.file_manager.append_log_bytes(
                    &end_block,
                    state.max_file_size,
                    false,
                    keep_open,
                )?;
            }
        }
        flushed = true;
    }
    Ok(flushed)
}

fn should_persist_async_mmap(state: &EngineState, force_flush: bool) -> bool {
    force_flush
        || state.async_pending_updates_since_persist >= ASYNC_PENDING_MMAP_PERSIST_EVERY_UPDATES
        || state.async_pending_bytes_since_persist >= ASYNC_PENDING_MMAP_PERSIST_EVERY_BYTES
        || state.last_async_buffer_persist_at.elapsed() >= ASYNC_PENDING_MMAP_PERSIST_INTERVAL
}

fn mark_async_mmap_persisted(state: &mut EngineState) {
    state.async_pending_updates_since_persist = 0;
    state.async_pending_bytes_since_persist = 0;
    state.last_async_buffer_persist_at = Instant::now();
}

fn async_buffer_flush_threshold(capacity: usize) -> usize {
    let threshold = capacity.saturating_mul(ASYNC_BUFFER_FLUSH_THRESHOLD_NUM)
        / ASYNC_BUFFER_FLUSH_THRESHOLD_DEN.max(1);
    threshold.max(1)
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

fn async_flush_reason_to_u8(reason: AsyncFlushReason) -> u8 {
    match reason {
        AsyncFlushReason::Unknown => 0,
        AsyncFlushReason::Threshold => 1,
        AsyncFlushReason::Explicit => 2,
        AsyncFlushReason::Timeout => 3,
        AsyncFlushReason::Stop => 4,
    }
}

fn u8_to_async_flush_reason(value: u8) -> AsyncFlushReason {
    match value {
        1 => AsyncFlushReason::Threshold,
        2 => AsyncFlushReason::Explicit,
        3 => AsyncFlushReason::Timeout,
        4 => AsyncFlushReason::Stop,
        _ => AsyncFlushReason::Unknown,
    }
}
