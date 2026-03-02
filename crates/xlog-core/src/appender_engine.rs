use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{bounded, unbounded, Receiver, RecvTimeoutError, Sender};
use thiserror::Error;

use crate::buffer::{recover_blocks, validate_block, BufferError, PersistentBuffer};
use crate::file_manager::{FileManager, FileManagerError};

const DEFAULT_ASYNC_FLUSH_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const ASYNC_PENDING_MMAP_PERSIST_EVERY_UPDATES: u32 = 8;
const MIN_LOG_ALIVE_SECONDS: i64 = 24 * 60 * 60;

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
        let state = Arc::new(Mutex::new(EngineState {
            file_manager,
            buffer,
            max_file_size,
            max_alive_time: max_alive_time.max(MIN_LOG_ALIVE_SECONDS),
            async_pending_updates_since_persist: 0,
        }));
        if let Ok(mut state_guard) = state.lock() {
            // Keep parity with C++ appender startup behavior: drain recovered mmap data
            // into logfile immediately instead of waiting for next write/flush.
            let _ = flush_pending_locked(&mut state_guard, false);
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
                let mut state = self.state.lock().expect("state lock poisoned");
                state
                    .file_manager
                    .append_log_bytes(block, state.max_file_size, false)?;
                housekeep_locked(&mut state)?;
            }
            EngineMode::Async => {
                let should_flush = {
                    let mut state = self.state.lock().expect("state lock poisoned");
                    let appended = state.buffer.append_block(block)?;

                    if !appended {
                        let _ = flush_pending_locked(&mut state, true)?;
                        let appended_after_flush = state.buffer.append_block(block)?;
                        if !appended_after_flush {
                            state.file_manager.append_log_bytes(
                                block,
                                state.max_file_size,
                                true,
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

    pub fn async_buffer_stats(&self) -> Option<(usize, usize)> {
        if self.mode() != EngineMode::Async {
            return None;
        }
        self.state
            .lock()
            .ok()
            .map(|s| (s.buffer.len(), s.buffer.capacity()))
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
            should_flush
        };

        if should_flush {
            self.request_flush(false, true)?;
        }
        Ok(())
    }

    pub fn flush(&self, sync: bool) -> Result<(), AppenderEngineError> {
        if self.mode() == EngineMode::Sync {
            let mut state = self.state.lock().expect("state lock poisoned");
            housekeep_locked(&mut state)?;
            return Ok(());
        }
        self.request_flush(sync, true)
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
    loop {
        match rx.recv_timeout(flush_timeout) {
            Ok(EngineCommand::Flush { move_file, ack }) => {
                pending_async_flush.store(false, Ordering::Release);
                let flushed = state
                    .lock()
                    .map_err(|_| ())
                    .and_then(|mut s| flush_pending_locked(&mut s, move_file).map_err(|_| ()))
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
                    .and_then(|mut s| flush_pending_locked(&mut s, true).map_err(|_| ()))
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
                    .and_then(|mut s| flush_pending_locked(&mut s, true).map_err(|_| ()))
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
) -> Result<bool, AppenderEngineError> {
    let pending = state.buffer.take_all()?;
    state.async_pending_updates_since_persist = 0;
    let mut flushed = false;
    if !pending.is_empty() {
        let recovered = recover_blocks(&pending);
        if recovered.bytes.is_empty() {
            return housekeep_locked(state).map(|_| false);
        }
        state
            .file_manager
            .append_log_bytes(&recovered.bytes, state.max_file_size, move_file)?;
        flushed = true;
    }
    housekeep_locked(state)?;
    Ok(flushed)
}

fn housekeep_locked(state: &mut EngineState) -> Result<(), AppenderEngineError> {
    state
        .file_manager
        .move_old_cache_files(state.max_file_size)?;
    state
        .file_manager
        .delete_expired_files(state.max_alive_time)?;
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
