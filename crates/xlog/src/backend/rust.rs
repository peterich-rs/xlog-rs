use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::mpsc::{
    channel as std_channel, sync_channel, Receiver as StdReceiver, SendError, Sender as StdSender,
    SyncSender, TryRecvError, TrySendError,
};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::TimeZone;
use crossbeam_queue::ArrayQueue;
use mars_xlog_core::appender_engine::{
    AppenderEngine, AsyncFlushReason as EngineAsyncFlushReason, EngineMode,
};
use mars_xlog_core::buffer::{PersistentBuffer, DEFAULT_BUFFER_BLOCK_LEN};
use mars_xlog_core::compress::{ZlibLevel, ZstdLevel};
use mars_xlog_core::crypto::Crypt;
use mars_xlog_core::dump::{dump_to_file, memory_dump};
use mars_xlog_core::file_manager::{CacheDays, FileManager};
use mars_xlog_core::frame::FrameWriter;
use mars_xlog_core::oneshot::{oneshot_flush as core_oneshot_flush, OneshotError, OneshotOutcome};
use mars_xlog_core::platform_console::write_console_line;
#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos"
))]
use mars_xlog_core::platform_console::{set_apple_console_fun, AppleConsoleFun};
use mars_xlog_core::platform_tid::current_tid;
use mars_xlog_core::protocol::{CompressionKind, Hour, SeqGenerator, HEADER_LEN};
use mars_xlog_core::record::LogLevel as CoreLogLevel;
use mars_xlog_core::registry::InstanceRegistry;

use super::metrics::{
    record_async_block_send, record_async_dequeued, record_async_enqueued,
    record_async_flush_requeues, record_async_pending_block, record_async_queue_full,
    record_async_stage_sample, record_sync_stage_sample, AsyncBuildStage,
    AsyncPendingFinalizeReason, AsyncStageSample, AsyncWriteFrontProfile, SyncBuildStage,
    SyncStageSample, METRICS_ENABLED,
};
use crate::event::Event;
use crate::layout::{format_line, Layout};
use crate::{
    AppenderMode, CompressMode, FileIoAction, LogLevel, RawLogMeta, XlogConfig, XlogError,
};

#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos"
))]
use crate::ConsoleFun;

pub(crate) fn provider() -> &'static RustBackendProvider {
    static PROVIDER: RustBackendProvider = RustBackendProvider;
    &PROVIDER
}

pub(crate) struct RustBackendProvider;

pub(crate) struct RustBackend {
    id: usize,
    config: XlogConfig,
    layout: Layout,
    level: AtomicI32,
    console_open: AtomicBool,
    cipher: Crypt,
    engine: Arc<AppenderEngine>,
    async_frontend: AsyncFrontend,
    async_state: Mutex<AsyncStateSlot>,
    async_state_ready: Condvar,
}

struct AsyncFrontend {
    tx: SyncSender<AsyncFrontendCommand>,
    accepting: Arc<AtomicBool>,
    flush_queued: Arc<AtomicBool>,
    line_pools: Arc<[ArrayQueue<String>]>,
    full_retry_before_block: usize,
    worker: Mutex<Option<JoinHandle<()>>>,
}

enum AsyncFrontendCommand {
    Write(AsyncWriteCommand),
    Flush {
        sync: bool,
        ack: Option<StdSender<()>>,
        reason: AsyncFlushControlReason,
    },
    Stop {
        ack: StdSender<()>,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum AsyncFlushControlReason {
    // Keep the enum even though only `Explicit` exists today. The control
    // plane is expected to grow more flush reasons without rewriting the
    // worker protocol.
    Explicit,
}

struct AsyncWriteCommand {
    line: String,
    pool_shard: usize,
    now_hour: u8,
    force_flush: bool,
    profile: Option<AsyncWriteFrontProfile>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum MetaResolveMode {
    Category,
    Global,
}

struct AsyncStateSlot {
    pending: Option<FrameWriter>,
    busy: bool,
}

impl AsyncStateSlot {
    fn empty() -> Self {
        Self {
            pending: None,
            busy: false,
        }
    }
}

struct CheckedOutAsyncState<'a> {
    backend: &'a RustBackend,
    pending: Option<FrameWriter>,
    checkout_lock_ns: u64,
    checkout_wait_ns: u64,
}

impl CheckedOutAsyncState<'_> {
    fn pending(&self) -> Option<&FrameWriter> {
        self.pending.as_ref()
    }

    fn pending_mut(&mut self) -> Option<&mut FrameWriter> {
        self.pending.as_mut()
    }

    fn set_pending(&mut self, pending: Option<FrameWriter>) {
        self.pending = pending;
    }

    fn checkout_lock_ns(&self) -> u64 {
        self.checkout_lock_ns
    }

    fn checkout_wait_ns(&self) -> u64 {
        self.checkout_wait_ns
    }
}

impl Drop for CheckedOutAsyncState<'_> {
    fn drop(&mut self) {
        let mut guard = self
            .backend
            .async_state
            .lock()
            .expect("async state lock poisoned");
        debug_assert!(guard.busy);
        guard.pending = self.pending.take();
        guard.busy = false;
        self.backend.async_state_ready.notify_one();
    }
}

fn push_frame_line(
    writer: &mut FrameWriter,
    engine: &AppenderEngine,
    chunk: &[u8],
    encoded: &mut Vec<u8>,
    end_hour: Hour,
    force_flush: bool,
) -> bool {
    let Ok(truncate) = writer.push_line(chunk, end_hour, encoded) else {
        return false;
    };
    engine
        .write_async_parts(None, truncate, encoded, end_hour, false, force_flush)
        .is_ok()
}

fn finish_frame(
    writer: &mut FrameWriter,
    engine: &AppenderEngine,
    encoded: &mut Vec<u8>,
    end_hour: Hour,
    force_flush: bool,
) -> bool {
    let Ok(truncate) = writer.finish(end_hour, encoded) else {
        return false;
    };
    engine
        .write_async_parts(None, truncate, encoded, end_hour, true, force_flush)
        .is_ok()
}

impl AsyncFlushControlReason {
    fn profiler_reason(self) -> AsyncPendingFinalizeReason {
        match self {
            AsyncFlushControlReason::Explicit => AsyncPendingFinalizeReason::ExplicitFlush,
        }
    }

    fn engine_reason(self) -> EngineAsyncFlushReason {
        match self {
            AsyncFlushControlReason::Explicit => EngineAsyncFlushReason::Explicit,
        }
    }
}

fn profiler_reason_from_engine(reason: EngineAsyncFlushReason) -> AsyncPendingFinalizeReason {
    match reason {
        EngineAsyncFlushReason::Threshold => AsyncPendingFinalizeReason::Threshold,
        EngineAsyncFlushReason::Explicit => AsyncPendingFinalizeReason::ExplicitFlush,
        EngineAsyncFlushReason::Timeout => AsyncPendingFinalizeReason::Timeout,
        EngineAsyncFlushReason::Stop => AsyncPendingFinalizeReason::Stop,
        EngineAsyncFlushReason::Unknown => AsyncPendingFinalizeReason::Unknown,
    }
}

fn record_pending_block_profile(state: &FrameWriter, reason: AsyncPendingFinalizeReason) {
    record_async_pending_block(
        state.line_count(),
        state.raw_input_bytes(),
        state.payload_len() as u64,
        reason,
    );
}

fn discard_stale_pending_block(
    pending: &mut Option<FrameWriter>,
    engine_epoch: u64,
    engine_reason: EngineAsyncFlushReason,
) {
    let stale = pending
        .as_ref()
        .map(|state| state.flush_epoch() != engine_epoch)
        .unwrap_or(false);
    if !stale {
        return;
    }
    if let Some(state) = pending.as_ref() {
        record_pending_block_profile(state, profiler_reason_from_engine(engine_reason));
    }
    *pending = None;
}

struct HotPathScratch {
    line: String,
    block: Vec<u8>,
    compress: Vec<u8>,
}

impl HotPathScratch {
    fn new() -> Self {
        Self {
            line: String::with_capacity(16 * 1024),
            block: Vec::with_capacity(DEFAULT_BUFFER_BLOCK_LEN),
            compress: Vec::with_capacity(16 * 1024),
        }
    }
}

thread_local! {
    static HOT_PATH_SCRATCH: RefCell<HotPathScratch> = RefCell::new(HotPathScratch::new());
}

fn with_hot_path_scratch<R>(f: impl FnOnce(&mut HotPathScratch) -> R) -> R {
    HOT_PATH_SCRATCH.with(|scratch| match scratch.try_borrow_mut() {
        Ok(mut borrowed) => f(&mut borrowed),
        Err(_) => with_hot_path_scratch_fallback(f),
    })
}

#[cold]
fn with_hot_path_scratch_fallback<R>(f: impl FnOnce(&mut HotPathScratch) -> R) -> R {
    debug_assert!(false, "re-entrant logging detected");
    let mut fallback = HotPathScratch::new();
    f(&mut fallback)
}

#[derive(Default)]
struct HourCache {
    epoch_second: i64,
    hour: u8,
    valid: bool,
}

thread_local! {
    static HOUR_CACHE: RefCell<HourCache> = const { RefCell::new(HourCache {
        epoch_second: 0,
        hour: 0,
        valid: false,
    }) };
}

static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

const ASYNC_WARNING_THRESHOLD_NUM: usize = 4;
const ASYNC_WARNING_THRESHOLD_DEN: usize = 5;
const ASYNC_FRONTEND_QUEUE_CAPACITY: usize = 65536;
const ASYNC_FRONTEND_FULL_RETRY_BEFORE_BLOCK_DEFAULT: usize = 4;
const ASYNC_FRONTEND_FULL_RETRY_BEFORE_BLOCK_ZSTD: usize = 1;
const ASYNC_FRONTEND_WRITE_BATCH_MAX: usize = 128;
const ASYNC_LINE_POOL_SHARDS: usize = 16;
const ASYNC_LINE_POOL_MAX_BUFFERS_PER_SHARD: usize = 256;
const ASYNC_LINE_POOL_MAX_CAPACITY: usize = 8 * 1024;
const ASYNC_LINE_BUFFER_INIT_CAPACITY: usize = 512;
const ASYNC_LINE_POOL_SHARD_SENTINEL: usize = usize::MAX;
// Keep the historical BUFFER_BLOCK_LENTH typo for compatibility with
// existing Mars/C++ log text and external grep patterns.
const ASYNC_HIGH_WATERMARK_WARNING_PREFIX: &str =
    "[F][ sg_buffer_async.Length() >= BUFFER_BLOCK_LENTH*4/5, len: ";

fn take_async_flush_control_reason(_sync: bool) -> AsyncFlushControlReason {
    AsyncFlushControlReason::Explicit
}

fn current_async_line_pool_shard() -> usize {
    thread_local! {
        static ASYNC_LINE_POOL_SHARD: Cell<usize> = const { Cell::new(usize::MAX) };
    }
    ASYNC_LINE_POOL_SHARD.with(|slot| {
        let cached = slot.get();
        if cached != usize::MAX {
            return cached;
        }
        let shard = (current_tid().unsigned_abs() as usize) % ASYNC_LINE_POOL_SHARDS;
        slot.set(shard);
        shard
    })
}

impl AsyncFrontend {
    fn new(engine: Arc<AppenderEngine>, config: XlogConfig, cipher: Crypt) -> Self {
        let (tx, rx) = sync_channel::<AsyncFrontendCommand>(ASYNC_FRONTEND_QUEUE_CAPACITY);
        let accepting = Arc::new(AtomicBool::new(true));
        let flush_queued = Arc::new(AtomicBool::new(false));
        let full_retry_before_block = match config.compress_mode {
            CompressMode::Zstd => ASYNC_FRONTEND_FULL_RETRY_BEFORE_BLOCK_ZSTD,
            CompressMode::Zlib => ASYNC_FRONTEND_FULL_RETRY_BEFORE_BLOCK_DEFAULT,
        };
        let line_pools = Arc::<[ArrayQueue<String>]>::from(
            (0..ASYNC_LINE_POOL_SHARDS)
                .map(|_| ArrayQueue::new(ASYNC_LINE_POOL_MAX_BUFFERS_PER_SHARD))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        let worker_flush_queued = Arc::clone(&flush_queued);
        let worker_line_pools = Arc::clone(&line_pools);
        let worker = thread::Builder::new()
            .name("xlog-rust-async-frontend".to_string())
            .spawn(move || {
                run_async_frontend_worker(
                    rx,
                    worker_flush_queued,
                    worker_line_pools,
                    engine,
                    config,
                    cipher,
                );
            })
            .expect("spawn rust async frontend worker");
        Self {
            tx,
            accepting,
            flush_queued,
            line_pools,
            full_retry_before_block,
            worker: Mutex::new(Some(worker)),
        }
    }

    fn enqueue_write(&self, mut cmd: AsyncWriteCommand) -> Result<(), AsyncWriteCommand> {
        let enqueue_begin = cmd.profile.as_ref().map(|_| Instant::now());
        let mut full_retries = 0usize;
        loop {
            if !self.accepting.load(Ordering::Acquire) {
                return Err(cmd);
            }
            if let (Some(begin), Some(front)) = (enqueue_begin, cmd.profile.as_mut()) {
                front.enqueue_ns = begin.elapsed().as_nanos() as u64;
            }
            match self.tx.try_send(AsyncFrontendCommand::Write(cmd)) {
                Ok(()) => {
                    if enqueue_begin.is_some() {
                        record_async_enqueued(ASYNC_FRONTEND_QUEUE_CAPACITY);
                    }
                    return Ok(());
                }
                Err(TrySendError::Disconnected(AsyncFrontendCommand::Write(v))) => return Err(v),
                Err(TrySendError::Full(AsyncFrontendCommand::Write(v))) => {
                    if enqueue_begin.is_some() {
                        record_async_queue_full();
                    }
                    cmd = v;
                    full_retries = full_retries.saturating_add(1);
                    if full_retries >= self.full_retry_before_block {
                        if !self.accepting.load(Ordering::Acquire) {
                            return Err(cmd);
                        }
                        let block_begin = enqueue_begin.map(|_| Instant::now());
                        return match self.tx.send(AsyncFrontendCommand::Write(cmd)) {
                            Ok(()) => {
                                if let Some(begin) = block_begin {
                                    record_async_block_send(begin.elapsed().as_nanos() as u64);
                                    record_async_enqueued(ASYNC_FRONTEND_QUEUE_CAPACITY);
                                }
                                Ok(())
                            }
                            Err(SendError(AsyncFrontendCommand::Write(v))) => {
                                if let Some(begin) = block_begin {
                                    record_async_block_send(begin.elapsed().as_nanos() as u64);
                                }
                                Err(v)
                            }
                            Err(_) => unreachable!("unexpected async frontend command variant"),
                        };
                    }
                    thread::yield_now();
                }
                Err(_) => unreachable!("unexpected async frontend command variant"),
            }
        }
    }

    fn request_flush(&self, sync: bool, reason: AsyncFlushControlReason) -> bool {
        if sync {
            let (ack_tx, ack_rx) = std_channel::<()>();
            if self
                .tx
                .send(AsyncFrontendCommand::Flush {
                    sync: true,
                    ack: Some(ack_tx),
                    reason,
                })
                .is_err()
            {
                return false;
            }
            return ack_rx.recv().is_ok();
        }

        if self.flush_queued.swap(true, Ordering::AcqRel) {
            return true;
        }
        match self.tx.try_send(AsyncFrontendCommand::Flush {
            sync: false,
            ack: None,
            reason,
        }) {
            Ok(()) => true,
            Err(TrySendError::Disconnected(_)) => {
                self.flush_queued.store(false, Ordering::Release);
                false
            }
            Err(TrySendError::Full(_)) => {
                self.flush_queued.store(false, Ordering::Release);
                true
            }
        }
    }

    fn set_accepting(&self, enabled: bool) {
        self.accepting.store(enabled, Ordering::Release);
    }

    fn take_line_buffer(&self, shard: usize) -> String {
        debug_assert!(
            shard < self.line_pools.len(),
            "async line pool shard out of range: {shard}"
        );
        if let Some(mut line) = self.line_pools[shard].pop() {
            line.clear();
            return line;
        }
        String::with_capacity(ASYNC_LINE_BUFFER_INIT_CAPACITY)
    }

    fn recycle_line_buffer(&self, shard: usize, mut line: String) {
        if shard == ASYNC_LINE_POOL_SHARD_SENTINEL {
            return;
        }
        debug_assert!(
            shard < self.line_pools.len(),
            "async line pool shard out of range: {shard}"
        );
        if line.capacity() > ASYNC_LINE_POOL_MAX_CAPACITY {
            return;
        }
        line.clear();
        let _ = self.line_pools[shard].push(line);
    }
    fn shutdown(&self) {
        self.set_accepting(false);
        let (ack_tx, ack_rx) = std_channel::<()>();
        let _ = self.tx.send(AsyncFrontendCommand::Stop { ack: ack_tx });
        let _ = ack_rx.recv_timeout(Duration::from_secs(2));
        if let Ok(mut guard) = self.worker.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

fn run_async_frontend_worker(
    rx: StdReceiver<AsyncFrontendCommand>,
    flush_queued: Arc<AtomicBool>,
    line_pools: Arc<[ArrayQueue<String>]>,
    engine: Arc<AppenderEngine>,
    config: XlogConfig,
    cipher: Crypt,
) {
    let capacity = engine.buffer_capacity();
    let mut pending: Option<FrameWriter> = None;
    let mut compress_scratch = Vec::with_capacity(16 * 1024);
    let mut crypto_scratch = Vec::with_capacity(16 * 1024);
    let mut block_scratch = Vec::with_capacity(DEFAULT_BUFFER_BLOCK_LEN);

    loop {
        let first = match rx.recv() {
            Ok(cmd) => cmd,
            Err(_) => break,
        };
        match first {
            AsyncFrontendCommand::Write(cmd) => {
                let mut cmd = cmd;
                if cmd.profile.is_some() {
                    record_async_dequeued();
                }
                handle_async_frontend_write(
                    &mut cmd,
                    &engine,
                    &config,
                    &cipher,
                    capacity,
                    &mut pending,
                    &mut compress_scratch,
                    &mut crypto_scratch,
                    &mut block_scratch,
                );
                recycle_line_buffer_to_pool(line_pools.as_ref(), cmd.pool_shard, cmd.line);
                let mut pending_control: Option<AsyncFrontendCommand> = None;
                for _ in 0..ASYNC_FRONTEND_WRITE_BATCH_MAX.saturating_sub(1) {
                    match rx.try_recv() {
                        Ok(AsyncFrontendCommand::Write(next)) => {
                            let mut next = next;
                            if next.profile.is_some() {
                                record_async_dequeued();
                            }
                            handle_async_frontend_write(
                                &mut next,
                                &engine,
                                &config,
                                &cipher,
                                capacity,
                                &mut pending,
                                &mut compress_scratch,
                                &mut crypto_scratch,
                                &mut block_scratch,
                            );
                            recycle_line_buffer_to_pool(
                                line_pools.as_ref(),
                                next.pool_shard,
                                next.line,
                            );
                        }
                        Ok(control) => {
                            pending_control = Some(control);
                            break;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return,
                    }
                }
                if let Some(control) = pending_control {
                    if handle_async_frontend_control(
                        control,
                        &flush_queued,
                        &engine,
                        &cipher,
                        &mut pending,
                        &mut compress_scratch,
                        &mut crypto_scratch,
                    ) {
                        break;
                    }
                }
            }
            control => {
                if handle_async_frontend_control(
                    control,
                    &flush_queued,
                    &engine,
                    &cipher,
                    &mut pending,
                    &mut compress_scratch,
                    &mut crypto_scratch,
                ) {
                    break;
                }
            }
        }
    }
}

fn handle_async_frontend_control(
    cmd: AsyncFrontendCommand,
    flush_queued: &AtomicBool,
    engine: &AppenderEngine,
    _cipher: &Crypt,
    pending: &mut Option<FrameWriter>,
    compress_scratch: &mut Vec<u8>,
    _crypto_scratch: &mut Vec<u8>,
) -> bool {
    match cmd {
        AsyncFrontendCommand::Flush { sync, ack, reason } => {
            flush_queued.store(false, Ordering::Release);
            worker_finalize_pending(engine, pending, compress_scratch, reason.profiler_reason());
            let _ = engine.flush_with_reason(sync, reason.engine_reason());
            if METRICS_ENABLED {
                record_async_flush_requeues(engine.take_async_flush_requeue_count());
            }
            if let Some(ack) = ack {
                let _ = ack.send(());
            }
            false
        }
        AsyncFrontendCommand::Stop { ack } => {
            flush_queued.store(false, Ordering::Release);
            worker_finalize_pending(
                engine,
                pending,
                compress_scratch,
                AsyncPendingFinalizeReason::Stop,
            );
            let _ = engine.flush_with_reason(true, EngineAsyncFlushReason::Stop);
            if METRICS_ENABLED {
                record_async_flush_requeues(engine.take_async_flush_requeue_count());
            }
            let _ = ack.send(());
            true
        }
        AsyncFrontendCommand::Write(_) => false,
    }
}

fn recycle_line_buffer_to_pool(pools: &[ArrayQueue<String>], shard: usize, mut line: String) {
    if shard == ASYNC_LINE_POOL_SHARD_SENTINEL {
        return;
    }
    debug_assert!(
        shard < pools.len(),
        "async line pool shard out of range: {shard}"
    );
    if line.capacity() > ASYNC_LINE_POOL_MAX_CAPACITY {
        return;
    }
    line.clear();
    let _ = pools[shard].push(line);
}

#[allow(clippy::too_many_arguments)]
fn handle_async_frontend_write(
    cmd: &mut AsyncWriteCommand,
    engine: &AppenderEngine,
    config: &XlogConfig,
    cipher: &Crypt,
    capacity: usize,
    pending: &mut Option<FrameWriter>,
    compress_scratch: &mut Vec<u8>,
    _crypto_scratch: &mut Vec<u8>,
    block_scratch: &mut Vec<u8>,
) {
    let mut stage = AsyncBuildStage::default();
    let mut profile_enabled = false;
    if let Some(front) = cmd.profile.as_ref() {
        profile_enabled = true;
        stage.format_ns = front.format_ns;
        stage.checkout_ns = front.enqueue_ns;
        stage.checkout_wait_ns = 0;
    }

    if engine.mode() != EngineMode::Async {
        let append_begin = profile_enabled.then(Instant::now);
        if build_sync_block_from_formatted_line(
            config,
            cipher,
            cmd.now_hour,
            cmd.line.as_str(),
            block_scratch,
        ) {
            let _ = engine.write_sealed(block_scratch.as_slice(), cmd.force_flush);
        }
        if let Some(begin) = append_begin {
            stage.append_ns = begin.elapsed().as_nanos() as u64;
        }
        if profile_enabled {
            let total_ns = stage
                .format_ns
                .saturating_add(stage.checkout_ns)
                .saturating_add(stage.checkout_wait_ns)
                .saturating_add(stage.append_ns);
            record_async_stage_sample(AsyncStageSample {
                total_ns,
                format_ns: stage.format_ns,
                checkout_ns: stage.checkout_ns,
                checkout_lock_ns: 0,
                checkout_wait_ns: stage.checkout_wait_ns,
                begin_pending_ns: 0,
                append_ns: stage.append_ns,
                force_flush_ns: 0,
            });
        }
        return;
    }

    let (engine_epoch, engine_flush_reason) = engine.async_flush_state();
    discard_stale_pending_block(pending, engine_epoch, engine_flush_reason);

    let begin_pending_begin = profile_enabled.then(Instant::now);
    if !ensure_async_open(pending, config, *cipher, engine, cmd.now_hour, engine_epoch) {
        return;
    }
    if let Some(begin) = begin_pending_begin {
        if pending
            .as_ref()
            .is_some_and(|writer| writer.line_count() == 0)
        {
            stage.begin_pending_ns = begin.elapsed().as_nanos() as u64;
        }
    }

    let Some(state) = pending.as_mut() else {
        return;
    };

    let threshold =
        capacity.saturating_mul(ASYNC_WARNING_THRESHOLD_NUM) / ASYNC_WARNING_THRESHOLD_DEN;
    let current_len = HEADER_LEN + state.payload_len();
    let mut warning_line = None;
    if current_len >= threshold {
        warning_line = Some(format!(
            "{ASYNC_HIGH_WATERMARK_WARNING_PREFIX}{current_len}\n"
        ));
    }
    let line = warning_line.as_deref().unwrap_or(cmd.line.as_str());

    let append_begin = profile_enabled.then(Instant::now);
    let appended = push_frame_line(
        state,
        engine,
        line.as_bytes(),
        compress_scratch,
        hour_from_clock(cmd.now_hour),
        cmd.force_flush,
    );
    if let Some(begin) = append_begin {
        stage.append_ns = begin.elapsed().as_nanos() as u64;
    }
    if !appended {
        *pending = None;
        let force_flush_begin = profile_enabled.then(Instant::now);
        let _ = engine.flush_with_reason(true, EngineAsyncFlushReason::Explicit);
        if let Some(begin) = force_flush_begin {
            stage.force_flush_ns = begin.elapsed().as_nanos() as u64;
        }
    }

    if profile_enabled {
        let total_ns = stage
            .format_ns
            .saturating_add(stage.checkout_ns)
            .saturating_add(stage.checkout_wait_ns)
            .saturating_add(stage.begin_pending_ns)
            .saturating_add(stage.append_ns)
            .saturating_add(stage.force_flush_ns);
        record_async_stage_sample(AsyncStageSample {
            total_ns,
            format_ns: stage.format_ns,
            checkout_ns: stage.checkout_ns,
            checkout_lock_ns: 0,
            checkout_wait_ns: stage.checkout_wait_ns,
            begin_pending_ns: stage.begin_pending_ns,
            append_ns: stage.append_ns,
            force_flush_ns: stage.force_flush_ns,
        });
    }
}

fn worker_finalize_pending(
    engine: &AppenderEngine,
    pending: &mut Option<FrameWriter>,
    encoded: &mut Vec<u8>,
    reason: AsyncPendingFinalizeReason,
) {
    let Some(state) = pending.as_mut() else {
        return;
    };
    let now_hour = hour_from_clock(local_hour_from_timestamp(std::time::SystemTime::now()));
    let finalized = finish_frame(state, engine, encoded, now_hour, false);
    if finalized {
        record_pending_block_profile(state, reason);
    }
    *pending = None;
    if !finalized && engine.mode() == EngineMode::Async {
        let _ = engine.flush_with_reason(true, EngineAsyncFlushReason::Explicit);
    }
}

fn new_async_writer(config: &XlogConfig, cipher: Crypt) -> Option<FrameWriter> {
    match config.compress_mode {
        CompressMode::Zlib => {
            let level = ZlibLevel::try_from(config.compress_level).ok()?;
            Some(FrameWriter::async_zlib(level, cipher))
        }
        CompressMode::Zstd => {
            let level = ZstdLevel::try_from(config.compress_level).ok()?;
            Some(FrameWriter::async_zstd(level, cipher))
        }
    }
}

fn open_async_writer(
    writer: &mut FrameWriter,
    engine: &AppenderEngine,
    hour: Hour,
    flush_epoch: u64,
) -> bool {
    let seq = global_async_seq().next_async();
    let Ok(header) = writer.open(hour, seq, flush_epoch) else {
        return false;
    };
    engine
        .write_async_parts(Some(&header), 0, &[], hour, false, false)
        .is_ok()
}

fn ensure_async_open(
    pending: &mut Option<FrameWriter>,
    config: &XlogConfig,
    cipher: Crypt,
    engine: &AppenderEngine,
    hour: u8,
    flush_epoch: u64,
) -> bool {
    if pending.as_ref().is_some_and(FrameWriter::is_open) {
        return true;
    }
    let Some(mut writer) = new_async_writer(config, cipher) else {
        return false;
    };
    if !open_async_writer(&mut writer, engine, hour_from_clock(hour), flush_epoch) {
        return false;
    }
    *pending = Some(writer);
    true
}

fn compression_family(config: &XlogConfig) -> CompressionKind {
    CompressionKind::from(config.compress_mode)
}

fn hour_from_clock(hour: u8) -> Hour {
    Hour::try_from(hour).expect("local clock hour is in 0..=23")
}

fn build_sync_block_from_formatted_line(
    config: &XlogConfig,
    cipher: &Crypt,
    hour: u8,
    line: &str,
    block: &mut Vec<u8>,
) -> bool {
    FrameWriter::sync(compression_family(config), *cipher)
        .seal_sync_line(line.as_bytes(), hour_from_clock(hour), block)
        .is_ok()
}

fn local_hour_from_timestamp(timestamp: SystemTime) -> u8 {
    if let Ok(since_epoch) = timestamp.duration_since(UNIX_EPOCH) {
        let epoch_secs = since_epoch.as_secs();
        if epoch_secs <= i64::MAX as u64 {
            let epoch_second = epoch_secs as i64;
            return HOUR_CACHE.with(|cache_cell| {
                let mut cache = cache_cell.borrow_mut();
                // Cache by epoch-second to keep the hot path cheap. This can
                // miss same-second local timezone changes, which is acceptable
                // for hour bucketing.
                if !cache.valid || cache.epoch_second != epoch_second {
                    if let Some(dt) = chrono::Local.timestamp_opt(epoch_second, 0).single() {
                        cache.epoch_second = epoch_second;
                        cache.hour = chrono::Timelike::hour(&dt) as u8;
                        cache.valid = true;
                    } else {
                        return chrono::Timelike::hour(&chrono::Local::now()) as u8;
                    }
                }
                cache.hour
            });
        }
    }
    chrono::Timelike::hour(&chrono::Local::now()) as u8
}

fn registry() -> &'static InstanceRegistry<RustBackend> {
    static REGISTRY: OnceLock<InstanceRegistry<RustBackend>> = OnceLock::new();
    REGISTRY.get_or_init(InstanceRegistry::new)
}

fn validate_compress_level(config: &XlogConfig) -> Result<(), XlogError> {
    let level = config.compress_level;
    let accepted = match config.compress_mode {
        CompressMode::Zlib => ZlibLevel::try_from(level).is_ok(),
        CompressMode::Zstd => ZstdLevel::try_from(level).is_ok(),
    };
    if accepted {
        Ok(())
    } else {
        Err(XlogError::InvalidCompressLevel {
            level,
            mode: config.compress_mode,
        })
    }
}

fn paths_to_strings(paths: Vec<std::path::PathBuf>) -> Vec<String> {
    paths
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn file_io_action_from_oneshot(result: Result<OneshotOutcome, OneshotError>) -> FileIoAction {
    match result {
        Ok(OneshotOutcome::Success) => FileIoAction::Success,
        Ok(OneshotOutcome::Unnecessary) => FileIoAction::Unnecessary,
        Err(OneshotError::Open(_)) => FileIoAction::OpenFailed,
        Err(OneshotError::Read(_) | OneshotError::LengthMismatch { .. }) => {
            FileIoAction::ReadFailed
        }
        Err(OneshotError::Write(_)) => FileIoAction::WriteFailed,
        Err(OneshotError::Remove(_)) => FileIoAction::RemoveFailed,
    }
}

fn global_async_seq() -> &'static SeqGenerator {
    static SEQ: OnceLock<SeqGenerator> = OnceLock::new();
    SEQ.get_or_init(SeqGenerator::default)
}

impl RustBackend {
    fn new(config: XlogConfig, level: LogLevel) -> Result<Self, XlogError> {
        Self::new_with_layout(config, level, Layout::mars())
    }

    fn new_with_layout(
        config: XlogConfig,
        level: LogLevel,
        layout: Layout,
    ) -> Result<Self, XlogError> {
        if config.log_dir.is_empty() || config.name_prefix.is_empty() {
            return Err(XlogError::InvalidConfig);
        }

        let cipher = match config.pub_key.as_deref() {
            Some(key) if !key.is_empty() => match Crypt::open(key) {
                Ok(cipher) => cipher,
                Err(_) => {
                    // Keep parity with C++: invalid pubkey falls back to no-crypt.
                    Crypt::off()
                }
            },
            _ => Crypt::off(),
        };
        let cache_days =
            CacheDays::try_from(config.cache_days).map_err(|_| XlogError::InvalidCacheDays)?;
        validate_compress_level(&config)?;

        let file_manager = FileManager::new(
            &config.log_dir,
            config.cache_dir.as_deref().map(std::path::PathBuf::from),
            &config.name_prefix,
            cache_days,
        )
        .map_err(|_| XlogError::InitFailed)?;
        let buffer = PersistentBuffer::open_with_capacity(
            file_manager.mmap_path(),
            DEFAULT_BUFFER_BLOCK_LEN,
        )
        .map_err(|_| XlogError::InitFailed)?;

        let engine = Arc::new(AppenderEngine::new(
            file_manager,
            buffer,
            appender_to_engine_mode(config.mode),
            0,
            10 * 24 * 60 * 60,
        ));
        let async_frontend = AsyncFrontend::new(Arc::clone(&engine), config.clone(), cipher);
        async_frontend.set_accepting(config.mode == AppenderMode::Async);

        Ok(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            console_open: AtomicBool::new(false),
            level: AtomicI32::new(level_to_i32(level)),
            config,
            layout,
            cipher,
            engine,
            async_frontend,
            async_state: Mutex::new(AsyncStateSlot::empty()),
            async_state_ready: Condvar::new(),
        })
    }

    fn checkout_async_state(&self, profile_enabled: bool) -> CheckedOutAsyncState<'_> {
        if let Ok(mut guard) = self.async_state.try_lock() {
            if !guard.busy {
                guard.busy = true;
                return CheckedOutAsyncState {
                    backend: self,
                    pending: guard.pending.take(),
                    checkout_lock_ns: 0,
                    checkout_wait_ns: 0,
                };
            }
        }
        let lock_begin = profile_enabled.then(Instant::now);
        let mut guard = self.async_state.lock().expect("async state lock poisoned");
        let checkout_lock_ns = lock_begin
            .map(|b| b.elapsed().as_nanos() as u64)
            .unwrap_or(0);
        let mut checkout_wait_ns = 0u64;
        while guard.busy {
            if profile_enabled {
                let wait_begin = Instant::now();
                guard = self
                    .async_state_ready
                    .wait(guard)
                    .expect("async state lock poisoned");
                checkout_wait_ns =
                    checkout_wait_ns.saturating_add(wait_begin.elapsed().as_nanos() as u64);
            } else {
                guard = self
                    .async_state_ready
                    .wait(guard)
                    .expect("async state lock poisoned");
            }
        }
        guard.busy = true;
        CheckedOutAsyncState {
            backend: self,
            pending: guard.pending.take(),
            checkout_lock_ns,
            checkout_wait_ns,
        }
    }

    fn build_sync_block_into<'a>(
        &self,
        scratch: &'a mut HotPathScratch,
        event: &Event<'_>,
        profile: Option<&mut SyncBuildStage>,
    ) -> Option<&'a [u8]> {
        let mut profile = profile;
        let format_begin = profile.as_ref().map(|_| Instant::now());
        let hour = local_hour_from_timestamp(event.timestamp);
        format_line(&self.layout, &mut scratch.line, event);
        if let (Some(begin), Some(stage)) = (format_begin, profile.as_mut()) {
            stage.format_ns = begin.elapsed().as_nanos() as u64;
        }

        let block_begin = profile.as_ref().map(|_| Instant::now());
        if FrameWriter::sync(compression_family(&self.config), self.cipher)
            .seal_sync_line(
                scratch.line.as_bytes(),
                hour_from_clock(hour),
                &mut scratch.block,
            )
            .is_err()
        {
            return None;
        }
        if let (Some(begin), Some(stage)) = (block_begin, profile.as_mut()) {
            stage.block_ns = begin.elapsed().as_nanos() as u64;
        }
        Some(scratch.block.as_slice())
    }

    fn resolve_record_meta(
        &self,
        raw_meta: RawLogMeta,
        mode: MetaResolveMode,
    ) -> (Option<i64>, Option<i64>, Option<i64>) {
        match mode {
            // Category path (`XloggerWrite(instance_ptr != 0)`): C++ fills only
            // when all 3 fields are -1, otherwise keeps user-provided values.
            MetaResolveMode::Category => {
                if raw_meta.pid == -1 && raw_meta.tid == -1 && raw_meta.maintid == -1 {
                    (None, None, None)
                } else {
                    (
                        Some(raw_meta.pid),
                        Some(raw_meta.tid),
                        Some(raw_meta.maintid),
                    )
                }
            }
            // Global path (`XloggerWrite(instance_ptr == 0)`): C++ fills each
            // field independently when that field equals -1.
            MetaResolveMode::Global => (
                if raw_meta.pid == -1 {
                    None
                } else {
                    Some(raw_meta.pid)
                },
                if raw_meta.tid == -1 {
                    None
                } else {
                    Some(raw_meta.tid)
                },
                if raw_meta.maintid == -1 {
                    None
                } else {
                    Some(raw_meta.maintid)
                },
            ),
        }
    }

    pub(crate) fn emit(&self, event: &Event<'_>, trace_log: bool) {
        if !self.is_enabled(event.level) {
            return;
        }

        #[cfg(target_os = "android")]
        let trace_console_bypass = trace_log;
        #[cfg(not(target_os = "android"))]
        let trace_console_bypass = {
            let _ = trace_log;
            false
        };

        if self.console_open.load(Ordering::Relaxed) || trace_console_bypass {
            write_console_line(
                to_core_level(event.level),
                event.tag,
                event.file,
                event.func,
                event.line,
                event.message,
            );
        }

        if self.engine.mode() == EngineMode::Async {
            self.write_async_line(event);
            return;
        }
        self.write_sync_line(event);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn emit_category(
        &self,
        level: LogLevel,
        tag: Option<&str>,
        file: &str,
        func: &str,
        line: u32,
        msg: &str,
        raw_meta: RawLogMeta,
    ) {
        let (pid, tid, maintid) = self.resolve_record_meta(raw_meta, MetaResolveMode::Category);
        let event = Event {
            level,
            tag: tag.unwrap_or(&self.config.name_prefix),
            file,
            func,
            line,
            message: msg,
            pid,
            tid,
            maintid,
            timestamp: SystemTime::now(),
        };
        self.emit(&event, raw_meta.trace_log);
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_global(
        &self,
        level: LogLevel,
        tag: &str,
        file: &str,
        func: &str,
        line: u32,
        msg: &str,
        raw_meta: RawLogMeta,
    ) {
        let (pid, tid, maintid) = self.resolve_record_meta(raw_meta, MetaResolveMode::Global);
        let event = Event {
            level,
            tag,
            file,
            func,
            line,
            message: msg,
            pid,
            tid,
            maintid,
            timestamp: SystemTime::now(),
        };
        self.emit(&event, raw_meta.trace_log);
    }

    fn write_sync_line(&self, event: &Event<'_>) {
        if METRICS_ENABLED {
            with_hot_path_scratch(|scratch| {
                let total_begin = Instant::now();
                let mut stage = SyncBuildStage::default();
                let Some(block) = self.build_sync_block_into(scratch, event, Some(&mut stage))
                else {
                    return;
                };
                let engine_begin = Instant::now();
                let _ = self
                    .engine
                    .write_sealed(block, event.level == LogLevel::Fatal);
                let engine_write_ns = engine_begin.elapsed().as_nanos() as u64;
                record_sync_stage_sample(SyncStageSample {
                    total_ns: total_begin.elapsed().as_nanos() as u64,
                    format_ns: stage.format_ns,
                    block_ns: stage.block_ns,
                    engine_write_ns,
                });
            });
        } else {
            with_hot_path_scratch(|scratch| {
                let Some(block) = self.build_sync_block_into(scratch, event, None) else {
                    return;
                };
                let _ = self
                    .engine
                    .write_sealed(block, event.level == LogLevel::Fatal);
            });
        }
    }

    #[cfg(test)]
    fn open_test_async_writer(&self, hour: u8, flush_epoch: u64) -> Option<FrameWriter> {
        let mut writer = new_async_writer(&self.config, self.cipher)?;
        if !open_async_writer(
            &mut writer,
            &self.engine,
            hour_from_clock(hour),
            flush_epoch,
        ) {
            return None;
        }
        Some(writer)
    }

    fn write_async_line(&self, event: &Event<'_>) {
        #[cfg(test)]
        let force_inline = self
            .async_state
            .lock()
            .ok()
            .map(|s| s.pending.is_some())
            .unwrap_or(false);
        #[cfg(not(test))]
        let force_inline = false;

        if force_inline || !self.async_frontend.accepting.load(Ordering::Acquire) {
            self.write_async_line_inline(event);
            return;
        }

        let now_hour = local_hour_from_timestamp(event.timestamp);
        let profile_enabled = METRICS_ENABLED;
        let pool_shard = current_async_line_pool_shard();
        let mut line_buf = self.async_frontend.take_line_buffer(pool_shard);
        let format_begin = profile_enabled.then(Instant::now);
        format_line(&self.layout, &mut line_buf, event);
        let format_ns = format_begin
            .map(|begin| begin.elapsed().as_nanos() as u64)
            .unwrap_or(0);
        let cmd = AsyncWriteCommand {
            line: line_buf,
            pool_shard,
            now_hour,
            force_flush: event.level == LogLevel::Fatal,
            profile: profile_enabled.then_some(AsyncWriteFrontProfile {
                format_ns,
                enqueue_ns: 0,
            }),
        };

        if let Err(cmd) = self.async_frontend.enqueue_write(cmd) {
            self.async_frontend
                .recycle_line_buffer(cmd.pool_shard, cmd.line);
            self.write_async_line_inline(event);
        }
    }

    fn write_async_line_inline(&self, event: &Event<'_>) {
        let timestamp = event.timestamp;
        let now_hour = local_hour_from_timestamp(timestamp);
        let (engine_epoch, engine_flush_reason) = self.engine.async_flush_state();
        let capacity = self.engine.buffer_capacity();
        let profile_enabled = METRICS_ENABLED;

        with_hot_path_scratch(|scratch| {
            let total_begin = if profile_enabled {
                Some(Instant::now())
            } else {
                None
            };
            let mut stage = AsyncBuildStage::default();

            (|| {
                let format_begin = if profile_enabled {
                    Some(Instant::now())
                } else {
                    None
                };
                format_line(&self.layout, &mut scratch.line, event);
                if let Some(begin) = format_begin {
                    stage.format_ns = begin.elapsed().as_nanos() as u64;
                }

                let checkout_begin = if profile_enabled {
                    Some(Instant::now())
                } else {
                    None
                };
                let mut checked_out = self.checkout_async_state(profile_enabled);
                if let Some(begin) = checkout_begin {
                    stage.checkout_ns = begin.elapsed().as_nanos() as u64;
                }
                stage.checkout_lock_ns = checked_out.checkout_lock_ns();
                stage.checkout_wait_ns = checked_out.checkout_wait_ns();
                discard_stale_pending_block(
                    &mut checked_out.pending,
                    engine_epoch,
                    engine_flush_reason,
                );
                let begin_pending_begin = if profile_enabled {
                    Some(Instant::now())
                } else {
                    None
                };
                if !ensure_async_open(
                    &mut checked_out.pending,
                    &self.config,
                    self.cipher,
                    &self.engine,
                    now_hour,
                    engine_epoch,
                ) {
                    return;
                }
                if let Some(begin) = begin_pending_begin {
                    if checked_out
                        .pending()
                        .is_some_and(|writer| writer.line_count() == 0)
                    {
                        stage.begin_pending_ns = begin.elapsed().as_nanos() as u64;
                    }
                }
                let Some(state) = checked_out.pending_mut() else {
                    return;
                };

                let threshold = capacity.saturating_mul(ASYNC_WARNING_THRESHOLD_NUM)
                    / ASYNC_WARNING_THRESHOLD_DEN;
                let current_len = HEADER_LEN + state.payload_len();
                if current_len >= threshold {
                    scratch.line.clear();
                    let _ = writeln!(
                        scratch.line,
                        "{ASYNC_HIGH_WATERMARK_WARNING_PREFIX}{current_len}"
                    );
                }

                let append_begin = if profile_enabled {
                    Some(Instant::now())
                } else {
                    None
                };
                let appended = push_frame_line(
                    state,
                    &self.engine,
                    scratch.line.as_bytes(),
                    &mut scratch.compress,
                    hour_from_clock(now_hour),
                    event.level == LogLevel::Fatal,
                );
                if let Some(begin) = append_begin {
                    stage.append_ns = begin.elapsed().as_nanos() as u64;
                }
                if !appended {
                    checked_out.set_pending(None);
                    drop(checked_out);
                    let force_flush_begin = if profile_enabled {
                        Some(Instant::now())
                    } else {
                        None
                    };
                    let _ = self
                        .engine
                        .flush_with_reason(true, EngineAsyncFlushReason::Explicit);
                    if let Some(begin) = force_flush_begin {
                        stage.force_flush_ns = begin.elapsed().as_nanos() as u64;
                    }
                }
            })();

            if let Some(begin) = total_begin {
                record_async_stage_sample(AsyncStageSample {
                    total_ns: begin.elapsed().as_nanos() as u64,
                    format_ns: stage.format_ns,
                    checkout_ns: stage.checkout_ns,
                    checkout_lock_ns: stage.checkout_lock_ns,
                    checkout_wait_ns: stage.checkout_wait_ns,
                    begin_pending_ns: stage.begin_pending_ns,
                    append_ns: stage.append_ns,
                    force_flush_ns: stage.force_flush_ns,
                });
            }
        });
    }

    fn finalize_async_pending(&self, reason: AsyncPendingFinalizeReason) {
        let now_hour = local_hour_from_timestamp(std::time::SystemTime::now());
        with_hot_path_scratch(|scratch| {
            let mut checked_out = self.checkout_async_state(false);
            let Some(state) = checked_out.pending_mut() else {
                return;
            };
            let finalized = finish_frame(
                state,
                &self.engine,
                &mut scratch.compress,
                hour_from_clock(now_hour),
                false,
            );
            if finalized {
                record_pending_block_profile(state, reason);
            }
            checked_out.set_pending(None);
            let needs_force_flush = !finalized && self.engine.mode() == EngineMode::Async;
            drop(checked_out);
            if needs_force_flush {
                let _ = self
                    .engine
                    .flush_with_reason(true, EngineAsyncFlushReason::Explicit);
            }
        });
    }

    fn make_logfile_name_impl(&self, timespan: i32, prefix: &str) -> Vec<String> {
        paths_to_strings(self.engine.make_logfile_name(timespan, prefix))
    }

    fn filepaths_from_timespan_impl(&self, timespan: i32, prefix: &str) -> Vec<String> {
        paths_to_strings(self.engine.filepaths_from_timespan(timespan, prefix))
    }
}

impl RustBackendProvider {
    pub(crate) fn new_instance_with_layout(
        &self,
        config: &XlogConfig,
        level: LogLevel,
        layout: Layout,
    ) -> Result<Arc<RustBackend>, XlogError> {
        let backend = registry().get_or_try_insert_with(&config.name_prefix, || {
            Ok::<_, XlogError>(Arc::new(RustBackend::new_with_layout(
                config.clone(),
                level,
                layout,
            )?))
        })?;
        if backend.config != *config {
            return Err(XlogError::ConfigConflict {
                name_prefix: config.name_prefix.clone(),
            });
        }
        backend.set_level(level);
        Ok(backend)
    }

    pub(crate) fn get_instance(&self, name_prefix: &str) -> Option<Arc<RustBackend>> {
        registry().get(name_prefix)
    }

    pub(crate) fn appender_open(
        &self,
        config: &XlogConfig,
        level: LogLevel,
    ) -> Result<(), XlogError> {
        if let Some(default) = registry().default_instance() {
            if default.config != *config {
                return Err(XlogError::ConfigConflict {
                    name_prefix: default.config.name_prefix.clone(),
                });
            }
            default.set_level(level);
            return Ok(());
        }
        let backend = Arc::new(RustBackend::new(config.clone(), level)?);
        registry().set_default(backend);
        Ok(())
    }

    pub(crate) fn appender_close(&self) {
        registry().clear_default();
    }

    pub(crate) fn flush_all(&self, sync: bool) {
        let mut default_id = None;
        if let Some(default) = registry().default_instance() {
            default_id = Some(default.id);
            default.flush(sync);
        }
        registry().for_each_live(|backend| {
            if default_id == Some(backend.id) {
                return;
            }
            backend.flush(sync);
        });
    }

    pub(crate) fn global_is_enabled(&self, level: LogLevel) -> bool {
        registry()
            .default_instance()
            .map(|b| b.is_enabled(level))
            .unwrap_or(false)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_global_with_meta(
        &self,
        level: LogLevel,
        tag: &str,
        file: &str,
        func: &str,
        line: u32,
        msg: &str,
        raw_meta: RawLogMeta,
    ) {
        let Some(default) = registry().default_instance() else {
            return;
        };
        default.emit_global(level, tag, file, func, line, msg, raw_meta);
    }

    #[cfg(any(
        target_os = "ios",
        target_os = "macos",
        target_os = "tvos",
        target_os = "watchos"
    ))]
    pub(crate) fn set_console_fun(&self, fun: ConsoleFun) {
        let core_fun = match fun {
            ConsoleFun::Printf => AppleConsoleFun::Printf,
            ConsoleFun::NSLog => AppleConsoleFun::NsLog,
            ConsoleFun::OSLog => AppleConsoleFun::OsLog,
        };
        set_apple_console_fun(core_fun);
    }

    pub(crate) fn current_log_path(&self) -> Option<String> {
        registry()
            .default_instance()
            .map(|b| b.engine.log_dir().to_string_lossy().into_owned())
    }

    pub(crate) fn current_log_cache_path(&self) -> Option<String> {
        registry().default_instance().and_then(|b| {
            b.engine
                .cache_dir()
                .map(|path| path.to_string_lossy().into_owned())
        })
    }

    pub(crate) fn filepaths_from_timespan(&self, timespan: i32, prefix: &str) -> Vec<String> {
        registry()
            .default_instance()
            .map(|b| b.filepaths_from_timespan_impl(timespan, prefix))
            .unwrap_or_default()
    }

    pub(crate) fn make_logfile_name(&self, timespan: i32, prefix: &str) -> Vec<String> {
        registry()
            .default_instance()
            .map(|b| b.make_logfile_name_impl(timespan, prefix))
            .unwrap_or_default()
    }

    pub(crate) fn oneshot_flush(&self, config: &XlogConfig) -> Result<FileIoAction, XlogError> {
        if config.log_dir.is_empty() || config.name_prefix.is_empty() {
            return Err(XlogError::InvalidConfig);
        }

        let cache_days =
            CacheDays::try_from(config.cache_days).map_err(|_| XlogError::InvalidCacheDays)?;
        let file_manager = FileManager::new(
            &config.log_dir,
            config.cache_dir.as_deref().map(std::path::PathBuf::from),
            &config.name_prefix,
            cache_days,
        )
        .map_err(|_| XlogError::InitFailed)?;

        let max_file_size = registry()
            .get(&config.name_prefix)
            .or_else(|| registry().default_instance())
            .map(|b| b.engine.max_file_size())
            .unwrap_or(0);

        Ok(file_io_action_from_oneshot(core_oneshot_flush(
            &file_manager,
            DEFAULT_BUFFER_BLOCK_LEN,
            max_file_size,
        )))
    }

    pub(crate) fn dump(&self, buffer: &[u8]) -> String {
        let Some(default) = registry().default_instance() else {
            return String::new();
        };
        dump_to_file(&default.engine.log_dir().to_string_lossy(), buffer)
    }

    pub(crate) fn memory_dump(&self, buffer: &[u8]) -> String {
        memory_dump(buffer)
    }
}

impl RustBackend {
    pub(crate) fn instance(&self) -> usize {
        self.id
    }

    pub(crate) fn is_enabled(&self, level: LogLevel) -> bool {
        level_to_i32(level) >= self.level.load(Ordering::Relaxed)
    }

    pub(crate) fn level(&self) -> LogLevel {
        i32_to_level(self.level.load(Ordering::Relaxed))
    }

    pub(crate) fn set_level(&self, level: LogLevel) {
        self.level.store(level_to_i32(level), Ordering::Relaxed);
    }

    pub(crate) fn set_appender_mode(&self, mode: AppenderMode) {
        let current = self.engine.mode();
        match (current, mode) {
            (EngineMode::Async, AppenderMode::Sync) => {
                self.async_frontend.set_accepting(false);
                let _ = self
                    .async_frontend
                    .request_flush(true, AsyncFlushControlReason::Explicit);
                self.finalize_async_pending(AsyncPendingFinalizeReason::ExplicitFlush);
                let _ = self.engine.set_mode(EngineMode::Sync);
            }
            (EngineMode::Sync, AppenderMode::Async) => {
                let _ = self.engine.set_mode(EngineMode::Async);
                self.async_frontend.set_accepting(true);
            }
            _ => {
                let _ = self.engine.set_mode(appender_to_engine_mode(mode));
                self.async_frontend
                    .set_accepting(mode == AppenderMode::Async);
            }
        }
    }

    pub(crate) fn flush(&self, sync: bool) {
        let control_reason = take_async_flush_control_reason(sync);
        if self.engine.mode() == EngineMode::Async {
            if self.async_frontend.request_flush(sync, control_reason) {
                return;
            }
            self.finalize_async_pending(control_reason.profiler_reason());
        }
        let _ = self
            .engine
            .flush_with_reason(sync, control_reason.engine_reason());
    }

    pub(crate) fn set_console_log_open(&self, open: bool) {
        self.console_open.store(open, Ordering::Relaxed);
    }

    pub(crate) fn set_max_file_size(&self, max_bytes: i64) {
        let v = max_bytes.max(0) as u64;
        self.engine.set_max_file_size(v);
    }

    pub(crate) fn set_max_alive_time(&self, alive_seconds: i64) -> Result<(), XlogError> {
        self.engine
            .set_max_alive_time(alive_seconds)
            .map_err(|err| match err {
                mars_xlog_core::appender_engine::AppenderEngineError::AliveTimeTooSmall {
                    alive_seconds,
                    minimum,
                } => XlogError::AliveTimeTooSmall {
                    alive_seconds,
                    minimum,
                },
                _ => XlogError::InitFailed,
            })
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_with_meta(
        &self,
        level: LogLevel,
        tag: &str,
        file: &str,
        func: &str,
        line: u32,
        msg: &str,
        raw_meta: RawLogMeta,
    ) {
        self.emit_category(level, Some(tag), file, func, line, msg, raw_meta);
    }
}

impl Drop for RustBackend {
    fn drop(&mut self) {
        self.async_frontend.shutdown();
    }
}

fn level_to_i32(level: LogLevel) -> i32 {
    match level {
        LogLevel::Verbose => 0,
        LogLevel::Debug => 1,
        LogLevel::Info => 2,
        LogLevel::Warn => 3,
        LogLevel::Error => 4,
        LogLevel::Fatal => 5,
        LogLevel::None => 6,
    }
}

fn i32_to_level(v: i32) -> LogLevel {
    match v {
        0 => LogLevel::Verbose,
        1 => LogLevel::Debug,
        2 => LogLevel::Info,
        3 => LogLevel::Warn,
        4 => LogLevel::Error,
        5 => LogLevel::Fatal,
        _ => LogLevel::None,
    }
}

fn to_core_level(level: LogLevel) -> CoreLogLevel {
    CoreLogLevel::from(level)
}

impl From<AppenderMode> for EngineMode {
    fn from(mode: AppenderMode) -> Self {
        match mode {
            AppenderMode::Async => EngineMode::Async,
            AppenderMode::Sync => EngineMode::Sync,
        }
    }
}

impl From<CompressMode> for CompressionKind {
    fn from(mode: CompressMode) -> Self {
        match mode {
            CompressMode::Zlib => CompressionKind::Zlib,
            CompressMode::Zstd => CompressionKind::Zstd,
        }
    }
}

fn appender_to_engine_mode(mode: AppenderMode) -> EngineMode {
    EngineMode::from(mode)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use mars_xlog_core::buffer::DEFAULT_BUFFER_BLOCK_LEN;
    use mars_xlog_core::compress::{decompress_raw_zlib, decompress_zstd_frames};
    use mars_xlog_core::crypto::{tea_decrypt_in_place, Crypt, PrivateKey};
    use mars_xlog_core::protocol::{
        AppendMode, BlockMagic, CompressionKind, LogHeader, HEADER_LEN, MAGIC_END,
        MAGIC_SYNC_ZLIB_START, TAILER_LEN,
    };

    use mars_xlog_core::platform_tid::main_tid;

    use super::RustBackend;
    use crate::{AppenderMode, LogLevel, RawLogMeta, XlogConfig};

    const TEST_SERVER_PUBKEY_HEX: &str = concat!(
        "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
        "483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8"
    );
    const TEST_SERVER_PRIVKEY_ONE: [u8; 32] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 1,
    ];

    fn decrypt_async_payload(header: &LogHeader, payload: &[u8]) -> Vec<u8> {
        if header.client_pubkey == [0; 64] {
            return payload.to_vec();
        }
        let cipher = Crypt::from_server_secret(
            header.client_pubkey,
            PrivateKey::try_from(TEST_SERVER_PRIVKEY_ONE).unwrap(),
        )
        .unwrap();

        let mut out = payload.to_vec();
        let block_end = out.len() / 8 * 8;
        tea_decrypt_in_place(&mut out[..block_end], &cipher.tea_key().unwrap());
        out
    }

    fn parse_blocks(bytes: &[u8]) -> Vec<(LogHeader, Vec<u8>)> {
        let mut out = Vec::new();
        let mut offset = 0usize;
        while offset + HEADER_LEN + TAILER_LEN <= bytes.len() {
            let Ok(header) = LogHeader::decode(&bytes[offset..offset + HEADER_LEN]) else {
                break;
            };
            let payload_len = header.len as usize;
            let payload_start = offset + HEADER_LEN;
            let payload_end = payload_start + payload_len;
            if payload_end + TAILER_LEN > bytes.len() {
                break;
            }
            if bytes[payload_end] != MAGIC_END {
                break;
            }
            out.push((header, bytes[payload_start..payload_end].to_vec()));
            offset = payload_end + TAILER_LEN;
        }
        out
    }

    fn decode_block_payload(header: &LogHeader, payload: &[u8]) -> Vec<u8> {
        if header.magic.mode() != AppendMode::Async {
            return payload.to_vec();
        }

        let raw = if header.magic.encrypted() {
            decrypt_async_payload(header, payload)
        } else {
            payload.to_vec()
        };

        match header.magic.family() {
            CompressionKind::Zlib => decompress_raw_zlib(&raw).unwrap(),
            CompressionKind::Zstd => decompress_zstd_frames(&raw).unwrap(),
        }
    }

    fn collect_decoded_text(root: &Path) -> String {
        let mut files: Vec<_> = fs::read_dir(root)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("xlog"))
            .collect();
        files.sort();

        let mut merged = String::new();
        for file in files {
            let bytes = fs::read(file).unwrap();
            for (header, payload) in parse_blocks(&bytes) {
                let plain = decode_block_payload(&header, &payload);
                merged.push_str(std::str::from_utf8(&plain).unwrap());
            }
        }
        merged
    }

    fn parse_block_payload(block: &[u8]) -> (LogHeader, &[u8]) {
        let header = LogHeader::decode(&block[..HEADER_LEN]).unwrap();
        let payload_len = header.len as usize;
        let payload_start = HEADER_LEN;
        let payload_end = HEADER_LEN + payload_len;
        assert_eq!(block.len(), payload_end + TAILER_LEN);
        (header, &block[payload_start..payload_end])
    }

    #[test]
    fn rust_backend_writes_xlog_block() {
        let root = std::env::temp_dir().join(format!("xlog-rust-backend-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let cfg = crate::XlogConfig::new(root.to_string_lossy().to_string(), "demo");
        let backend = RustBackend::new(cfg, LogLevel::Info).unwrap();

        backend.write_with_meta(
            LogLevel::Info,
            "demo",
            "main.rs",
            "f",
            1,
            "hello",
            RawLogMeta::default(),
        );
        backend.flush(true);

        let mut found = false;
        for entry in fs::read_dir(&root).unwrap().flatten() {
            let p = entry.path();
            if p.extension().and_then(|x| x.to_str()) == Some("xlog") {
                let bytes = fs::read(&p).unwrap();
                assert!(!bytes.is_empty());
                found = true;
            }
        }

        assert!(found, "expected at least one xlog output file");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sync_mode_with_pubkey_uses_crypt_magic_and_plain_payload() {
        let root = std::env::temp_dir().join(format!(
            "xlog-rust-backend-sync-crypt-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let cfg = XlogConfig::new(root.to_string_lossy().to_string(), "demo-sync")
            .mode(AppenderMode::Sync)
            .pub_key(TEST_SERVER_PUBKEY_HEX);
        let backend = RustBackend::new(cfg, LogLevel::Info).unwrap();

        let event = crate::Event {
            file: "main.rs",
            func: "f",
            line: 7,
            pid: Some(std::process::id() as i64),
            tid: Some(super::current_tid()),
            maintid: Some(main_tid()),
            ..crate::Event::new(LogLevel::Info, "tag", "plain-sync")
        };
        let block = super::with_hot_path_scratch(|scratch| {
            backend
                .build_sync_block_into(scratch, &event, None)
                .unwrap()
                .to_vec()
        });
        let (header, payload) = parse_block_payload(&block);
        assert_eq!(u8::from(header.magic), MAGIC_SYNC_ZLIB_START);
        assert_ne!(header.client_pubkey, [0; 64]);

        let line = std::str::from_utf8(payload).unwrap();
        assert!(line.contains("plain-sync"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn formatted_line_marks_main_thread_when_tid_matches() {
        let root =
            std::env::temp_dir().join(format!("xlog-rust-backend-maintid-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let cfg = XlogConfig::new(root.to_string_lossy().to_string(), "demo-maintid")
            .mode(AppenderMode::Sync);
        let backend = RustBackend::new(cfg, LogLevel::Info).unwrap();
        let event = crate::Event {
            file: "main.rs",
            func: "f",
            line: 9,
            pid: Some(std::process::id() as i64),
            tid: Some(super::current_tid()),
            maintid: Some(main_tid()),
            ..crate::Event::new(LogLevel::Info, "tag", "maintid")
        };
        let block = super::with_hot_path_scratch(|scratch| {
            backend
                .build_sync_block_into(scratch, &event, None)
                .unwrap()
                .to_vec()
        });
        let (_header, payload) = parse_block_payload(&block);
        let line = std::str::from_utf8(payload).unwrap();

        if super::current_tid() == main_tid() {
            assert!(line.contains('*'));
        } else {
            assert!(!line.contains('*'));
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn category_mode_only_fills_when_all_meta_are_minus_one() {
        let root = std::env::temp_dir().join(format!(
            "xlog-rust-backend-category-meta-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let cfg = XlogConfig::new(root.to_string_lossy().to_string(), "demo-category-meta")
            .mode(AppenderMode::Sync);
        let backend = RustBackend::new(cfg, LogLevel::Info).unwrap();

        let (pid, tid, maintid) =
            backend.resolve_record_meta(RawLogMeta::default(), super::MetaResolveMode::Category);
        assert_eq!(pid, None);
        assert_eq!(tid, None);
        assert_eq!(maintid, None);

        let (pid, tid, maintid) = backend.resolve_record_meta(
            RawLogMeta::new(123, -1, -1),
            super::MetaResolveMode::Category,
        );
        assert_eq!(pid, Some(123));
        assert_eq!(tid, Some(-1));
        assert_eq!(maintid, Some(-1));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn global_mode_fills_each_missing_meta_field_independently() {
        let root = std::env::temp_dir().join(format!(
            "xlog-rust-backend-global-meta-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let cfg = XlogConfig::new(root.to_string_lossy().to_string(), "demo-global-meta")
            .mode(AppenderMode::Sync);
        let backend = RustBackend::new(cfg, LogLevel::Info).unwrap();

        let (pid, tid, maintid) = backend
            .resolve_record_meta(RawLogMeta::new(321, -1, -1), super::MetaResolveMode::Global);
        assert_eq!(pid, Some(321));
        assert_eq!(tid, None);
        assert_eq!(maintid, None);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn async_mode_streams_multiple_lines_into_one_zlib_block() {
        let root = std::env::temp_dir().join(format!(
            "xlog-rust-backend-async-stream-zlib-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let cfg = XlogConfig::new(root.to_string_lossy().to_string(), "demo-async");
        let backend = RustBackend::new(cfg, LogLevel::Info).unwrap();
        backend.write_with_meta(
            LogLevel::Info,
            "tag",
            "f1.rs",
            "f1",
            1,
            "one",
            RawLogMeta::default(),
        );
        backend.write_with_meta(
            LogLevel::Info,
            "tag",
            "f2.rs",
            "f2",
            2,
            "two",
            RawLogMeta::default(),
        );
        backend.flush(true);

        let xlog = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|x| x.to_str()) == Some("xlog"))
            .unwrap();
        let bytes = fs::read(xlog).unwrap();
        let (header, payload) = parse_block_payload(&bytes);
        assert!(header.len > 0);
        let plain = decompress_raw_zlib(payload).unwrap();
        let text = std::str::from_utf8(&plain).unwrap();
        assert!(text.contains("one"));
        assert!(text.contains("two"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn async_mode_streams_multiple_lines_into_one_zstd_block() {
        let root = std::env::temp_dir().join(format!(
            "xlog-rust-backend-async-stream-zstd-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let cfg = XlogConfig::new(root.to_string_lossy().to_string(), "demo-async-zstd")
            .compress_mode(crate::CompressMode::Zstd);
        let backend = RustBackend::new(cfg, LogLevel::Info).unwrap();
        backend.write_with_meta(
            LogLevel::Info,
            "tag",
            "f1.rs",
            "f1",
            1,
            "alpha",
            RawLogMeta::default(),
        );
        backend.write_with_meta(
            LogLevel::Info,
            "tag",
            "f2.rs",
            "f2",
            2,
            "beta",
            RawLogMeta::default(),
        );
        backend.flush(true);

        let xlog = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|x| x.to_str()) == Some("xlog"))
            .unwrap();
        let bytes = fs::read(xlog).unwrap();
        let (header, payload) = parse_block_payload(&bytes);
        assert!(header.len > 0);
        let plain = decompress_zstd_frames(payload).unwrap();
        let text = std::str::from_utf8(&plain).unwrap();
        assert!(text.contains("alpha"));
        assert!(text.contains("beta"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn async_mode_crypt_zlib_is_decodable() {
        let root = std::env::temp_dir().join(format!(
            "xlog-rust-backend-async-crypt-zlib-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let cfg = XlogConfig::new(root.to_string_lossy().to_string(), "demo-async-crypt-zlib")
            .pub_key(TEST_SERVER_PUBKEY_HEX);
        let backend = RustBackend::new(cfg, LogLevel::Info).unwrap();
        backend.write_with_meta(
            LogLevel::Info,
            "tag",
            "f1.rs",
            "f1",
            1,
            "gamma",
            RawLogMeta::default(),
        );
        backend.write_with_meta(
            LogLevel::Info,
            "tag",
            "f2.rs",
            "f2",
            2,
            "delta",
            RawLogMeta::default(),
        );
        backend.flush(true);

        let xlog = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|x| x.to_str()) == Some("xlog"))
            .unwrap();
        let bytes = fs::read(xlog).unwrap();
        let (header, payload) = parse_block_payload(&bytes);
        assert_eq!(header.magic, BlockMagic::AsyncZlib { encrypted: true });
        let decrypted = decrypt_async_payload(&header, payload);
        let plain = decompress_raw_zlib(&decrypted).unwrap();
        let text = std::str::from_utf8(&plain).unwrap();
        assert!(text.contains("gamma"));
        assert!(text.contains("delta"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn async_mode_crypt_zstd_is_decodable() {
        let root = std::env::temp_dir().join(format!(
            "xlog-rust-backend-async-crypt-zstd-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let cfg = XlogConfig::new(root.to_string_lossy().to_string(), "demo-async-crypt-zstd")
            .compress_mode(crate::CompressMode::Zstd)
            .pub_key(TEST_SERVER_PUBKEY_HEX);
        let backend = RustBackend::new(cfg, LogLevel::Info).unwrap();
        backend.write_with_meta(
            LogLevel::Info,
            "tag",
            "f1.rs",
            "f1",
            1,
            "theta",
            RawLogMeta::default(),
        );
        backend.write_with_meta(
            LogLevel::Info,
            "tag",
            "f2.rs",
            "f2",
            2,
            "lambda",
            RawLogMeta::default(),
        );
        backend.flush(true);

        let xlog = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|x| x.to_str()) == Some("xlog"))
            .unwrap();
        let bytes = fs::read(xlog).unwrap();
        let (header, payload) = parse_block_payload(&bytes);
        assert_eq!(header.magic, BlockMagic::AsyncZstd { encrypted: true });
        let decrypted = decrypt_async_payload(&header, payload);
        let plain = decompress_zstd_frames(&decrypted).unwrap();
        let text = std::str::from_utf8(&plain).unwrap();
        assert!(text.contains("theta"));
        assert!(text.contains("lambda"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn async_to_sync_switch_keeps_pending_logs() {
        let root = std::env::temp_dir().join(format!(
            "xlog-rust-backend-async-to-sync-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let cfg = XlogConfig::new(root.to_string_lossy().to_string(), "demo-switch");
        let backend = RustBackend::new(cfg, LogLevel::Info).unwrap();
        backend.write_with_meta(
            LogLevel::Info,
            "tag",
            "switch.rs",
            "before",
            10,
            "before-switch",
            RawLogMeta::default(),
        );
        backend.set_appender_mode(AppenderMode::Sync);
        backend.write_with_meta(
            LogLevel::Info,
            "tag",
            "switch.rs",
            "after",
            11,
            "after-switch",
            RawLogMeta::default(),
        );
        backend.flush(true);

        let mut merged = String::new();
        for _ in 0..20 {
            merged = collect_decoded_text(&root);
            if merged.contains("before-switch") && merged.contains("after-switch") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(merged.contains("before-switch"));
        assert!(merged.contains("after-switch"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn async_high_watermark_replaces_current_line_with_warning() {
        let root = std::env::temp_dir().join(format!(
            "xlog-rust-backend-high-watermark-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let cfg = XlogConfig::new(root.to_string_lossy().to_string(), "demo-watermark");
        let backend = RustBackend::new(cfg, LogLevel::Info).unwrap();

        let threshold = DEFAULT_BUFFER_BLOCK_LEN * 4 / 5;
        let engine_epoch = backend.engine.async_flush_epoch();
        {
            let mut guard = backend
                .async_state
                .lock()
                .expect("async state lock poisoned");
            let mut state = backend.open_test_async_writer(1, engine_epoch).unwrap();
            state.set_payload_len(threshold.saturating_sub(HEADER_LEN));
            guard.pending = Some(state);
        }

        let event = crate::Event {
            file: "watermark.rs",
            func: "f",
            line: 10,
            pid: Some(std::process::id() as i64),
            tid: Some(super::current_tid()),
            maintid: Some(main_tid()),
            ..crate::Event::new(LogLevel::Info, "tag", "ORIGINAL-LINE-SHOULD-BE-DROPPED")
        };
        backend.write_async_line(&event);

        let pending = backend.engine.async_buffer_snapshot().unwrap();
        let header = LogHeader::decode(&pending[..HEADER_LEN]).unwrap();
        let payload = &pending[HEADER_LEN..];
        let plain = decode_block_payload(&header, payload);
        let text = std::str::from_utf8(&plain).unwrap();
        assert!(text.contains(super::ASYNC_HIGH_WATERMARK_WARNING_PREFIX));
        assert!(!text.contains("ORIGINAL-LINE-SHOULD-BE-DROPPED"));
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "async line pool shard out of range")]
    fn recycle_line_buffer_rejects_invalid_shard_in_debug() {
        let root = std::env::temp_dir().join(format!(
            "xlog-rust-backend-invalid-shard-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let cfg = XlogConfig::new(root.to_string_lossy().to_string(), "demo-invalid-shard");
        let backend = RustBackend::new(cfg, LogLevel::Info).unwrap();
        backend
            .async_frontend
            .recycle_line_buffer(super::ASYNC_LINE_POOL_SHARDS, String::new());
    }
}
