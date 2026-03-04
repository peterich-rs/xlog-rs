use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use chrono::Local;
use mars_xlog_core::appender_engine::{AppenderEngine, AppenderEngineError, EngineMode};
use mars_xlog_core::buffer::{PersistentBuffer, DEFAULT_BUFFER_BLOCK_LEN};
use mars_xlog_core::compress::{StreamCompressor, ZlibStreamCompressor, ZstdStreamCompressor};
use mars_xlog_core::crypto::EcdhTeaCipher;
use mars_xlog_core::dump::{dump_to_file, memory_dump};
use mars_xlog_core::file_manager::FileManager;
use mars_xlog_core::oneshot::{
    oneshot_flush as core_oneshot_flush, FileIoAction as CoreFileIoAction,
};
#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos"
))]
use mars_xlog_core::platform_console::{set_apple_console_fun, AppleConsoleFun};
use mars_xlog_core::platform_console::{write_console_line, ConsoleLevel};
use mars_xlog_core::platform_tid::{current_tid, main_tid};
use mars_xlog_core::protocol::{
    select_magic, AppendMode, CompressionKind, LogHeader, SeqGenerator, HEADER_LEN, MAGIC_END,
};
use mars_xlog_core::record::{LogLevel as CoreLogLevel, LogRecord};
use mars_xlog_core::registry::InstanceRegistry;

use super::{XlogBackend, XlogBackendProvider};
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

pub(super) fn provider() -> &'static dyn XlogBackendProvider {
    static PROVIDER: RustBackendProvider = RustBackendProvider;
    &PROVIDER
}

struct RustBackendProvider;

struct RustBackend {
    id: usize,
    config: XlogConfig,
    level: AtomicI32,
    console_open: AtomicBool,
    cipher: EcdhTeaCipher,
    engine: AppenderEngine,
    async_state: Mutex<Option<AsyncPendingState>>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum MetaResolveMode {
    Category,
    Global,
}

enum AsyncCompressor {
    Zlib(ZlibStreamCompressor),
    Zstd(ZstdStreamCompressor),
}

impl AsyncCompressor {
    fn compress_chunk(&mut self, input: &[u8], out: &mut Vec<u8>) -> bool {
        match self {
            AsyncCompressor::Zlib(c) => c.compress_chunk(input, out).is_ok(),
            AsyncCompressor::Zstd(c) => c.compress_chunk(input, out).is_ok(),
        }
    }

    fn finish(&mut self, out: &mut Vec<u8>) -> bool {
        match self {
            AsyncCompressor::Zlib(c) => c.flush(out).is_ok(),
            AsyncCompressor::Zstd(c) => c.flush(out).is_ok(),
        }
    }
}

struct AsyncPendingState {
    header: LogHeader,
    payload: Vec<u8>,
    compressor: AsyncCompressor,
    crypt_tail: Vec<u8>,
    flush_epoch: u64,
}

impl AsyncPendingState {
    fn pending_bytes_without_tailer(&self) -> Option<Vec<u8>> {
        let mut header = self.header;
        header.len = u32::try_from(self.payload.len()).ok()?;
        let mut out = Vec::with_capacity(header.encode().len() + self.payload.len());
        out.extend_from_slice(&header.encode());
        out.extend_from_slice(&self.payload);
        Some(out)
    }

    fn append_chunk(&mut self, chunk: &[u8], cipher: &EcdhTeaCipher) -> bool {
        let mut compressed = Vec::new();
        if !self.compressor.compress_chunk(chunk, &mut compressed) {
            return false;
        }
        self.append_encrypted(&compressed, cipher)
    }

    fn finalize(mut self, cipher: &EcdhTeaCipher) -> Option<Vec<u8>> {
        let mut tail = Vec::new();
        if !self.compressor.finish(&mut tail) {
            return None;
        }
        if !self.append_encrypted(&tail, cipher) {
            return None;
        }
        let mut header = self.header;
        header.end_hour = chrono::Timelike::hour(&Local::now()) as u8;
        header.len = u32::try_from(self.payload.len()).ok()?;
        let mut out = Vec::with_capacity(header.encode().len() + self.payload.len() + 1);
        out.extend_from_slice(&header.encode());
        out.extend_from_slice(&self.payload);
        out.push(MAGIC_END);
        Some(out)
    }

    fn append_encrypted(&mut self, input: &[u8], cipher: &EcdhTeaCipher) -> bool {
        if !cipher.enabled() {
            self.payload.extend_from_slice(input);
            return true;
        }
        if !self.crypt_tail.is_empty() {
            let trim = self.crypt_tail.len().min(self.payload.len());
            self.payload.truncate(self.payload.len() - trim);
        }

        let mut merged = Vec::with_capacity(self.crypt_tail.len() + input.len());
        merged.extend_from_slice(&self.crypt_tail);
        merged.extend_from_slice(input);
        let full_len = merged.len() / 8 * 8;
        if full_len > 0 {
            let encrypted = cipher.encrypt_async(&merged[..full_len]);
            self.payload.extend_from_slice(&encrypted);
        }

        self.crypt_tail.clear();
        self.crypt_tail.extend_from_slice(&merged[full_len..]);
        self.payload.extend_from_slice(&self.crypt_tail);
        true
    }
}

static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
static GLOBAL_MAX_FILE_SIZE: AtomicU64 = AtomicU64::new(0);
static GLOBAL_MAX_ALIVE_TIME: AtomicI64 = AtomicI64::new(0);
static GLOBAL_CONSOLE_OPEN: AtomicBool = AtomicBool::new(false);

const ASYNC_WARNING_THRESHOLD_NUM: usize = 4;
const ASYNC_WARNING_THRESHOLD_DEN: usize = 5;

fn registry() -> &'static InstanceRegistry<RustBackend> {
    static REGISTRY: OnceLock<InstanceRegistry<RustBackend>> = OnceLock::new();
    REGISTRY.get_or_init(InstanceRegistry::new)
}

fn global_async_seq() -> &'static SeqGenerator {
    static SEQ: OnceLock<SeqGenerator> = OnceLock::new();
    SEQ.get_or_init(SeqGenerator::default)
}

impl RustBackend {
    fn new(config: XlogConfig, level: LogLevel) -> Result<Self, XlogError> {
        if config.log_dir.is_empty() || config.name_prefix.is_empty() {
            return Err(XlogError::InvalidConfig);
        }

        let cipher = match config.pub_key.as_deref() {
            Some(key) if !key.is_empty() => EcdhTeaCipher::new(key).unwrap_or_else(|_| {
                // Keep parity with C++: invalid pubkey falls back to no-crypt.
                EcdhTeaCipher::disabled()
            }),
            _ => EcdhTeaCipher::disabled(),
        };

        let file_manager = FileManager::new(
            config.log_dir.clone().into(),
            config.cache_dir.clone().map(Into::into),
            config.name_prefix.clone(),
            config.cache_days,
        )
        .map_err(|_| XlogError::InitFailed)?;
        let buffer = PersistentBuffer::open_with_capacity(
            file_manager.mmap_path(),
            DEFAULT_BUFFER_BLOCK_LEN,
        )
        .map_err(|_| XlogError::InitFailed)?;

        let engine = AppenderEngine::new(
            file_manager,
            buffer,
            appender_to_engine_mode(config.mode),
            0,
            10 * 24 * 60 * 60,
        );

        Ok(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            console_open: AtomicBool::new(false),
            level: AtomicI32::new(level_to_i32(level)),
            config,
            cipher,
            engine,
            async_state: Mutex::new(None),
        })
    }

    fn build_block(
        &self,
        level: LogLevel,
        tag: &str,
        file: &str,
        func: &str,
        line: u32,
        msg: &str,
        pid: i64,
        tid: i64,
        maintid: i64,
    ) -> Option<Vec<u8>> {
        let mode = engine_to_appender_mode(self.engine.mode());
        let compress = self.config.compress_mode;

        let now = Local::now();
        let is_crypt = self.cipher.enabled();

        let record = LogRecord {
            level: to_core_level(level),
            tag: tag.to_string(),
            filename: file.to_string(),
            func_name: func.to_string(),
            line: line as i32,
            timestamp: std::time::SystemTime::now(),
            pid,
            tid,
            maintid,
        };
        let line = mars_xlog_core::formatter::format_record(&record, msg);

        let mut payload = match mode {
            AppenderMode::Sync => line.into_bytes(),
            AppenderMode::Async => match compress {
                CompressMode::Zlib => {
                    let mut c = ZlibStreamCompressor::default();
                    let mut out = Vec::new();
                    c.compress_chunk(line.as_bytes(), &mut out).ok()?;
                    c.flush(&mut out).ok()?;
                    out
                }
                CompressMode::Zstd => {
                    let mut c = ZstdStreamCompressor::new(self.config.compress_level).ok()?;
                    let mut out = Vec::new();
                    c.compress_chunk(line.as_bytes(), &mut out).ok()?;
                    c.flush(&mut out).ok()?;
                    out
                }
            },
        };

        if is_crypt {
            payload = match mode {
                AppenderMode::Sync => self.cipher.encrypt_sync(&payload),
                AppenderMode::Async => self.cipher.encrypt_async(&payload),
            };
        }

        let compression_kind = match compress {
            CompressMode::Zlib => CompressionKind::Zlib,
            CompressMode::Zstd => CompressionKind::Zstd,
        };
        let append_mode = match mode {
            AppenderMode::Sync => AppendMode::Sync,
            AppenderMode::Async => AppendMode::Async,
        };

        let header = LogHeader {
            magic: select_magic(compression_kind, append_mode, is_crypt),
            seq: match mode {
                AppenderMode::Sync => SeqGenerator::sync_seq(),
                AppenderMode::Async => global_async_seq().next_async(),
            },
            begin_hour: chrono::Timelike::hour(&now) as u8,
            end_hour: chrono::Timelike::hour(&now) as u8,
            len: u32::try_from(payload.len()).ok()?,
            client_pubkey: if is_crypt {
                self.cipher.client_pubkey()
            } else {
                [0; 64]
            },
        };

        let mut block = Vec::with_capacity(73 + payload.len() + 1);
        block.extend_from_slice(&header.encode());
        block.extend_from_slice(&payload);
        block.push(MAGIC_END);
        Some(block)
    }

    fn format_record_line(
        &self,
        level: LogLevel,
        tag: &str,
        file: &str,
        func: &str,
        line: u32,
        msg: &str,
        pid: i64,
        tid: i64,
        maintid: i64,
    ) -> String {
        let record = LogRecord {
            level: to_core_level(level),
            tag: tag.to_string(),
            filename: file.to_string(),
            func_name: func.to_string(),
            line: line as i32,
            timestamp: std::time::SystemTime::now(),
            pid,
            tid,
            maintid,
        };
        mars_xlog_core::formatter::format_record(&record, msg)
    }

    fn resolve_record_meta(&self, raw_meta: RawLogMeta, mode: MetaResolveMode) -> (i64, i64, i64) {
        let runtime_pid = std::process::id() as i64;
        let runtime_tid = current_tid();
        let runtime_maintid = main_tid();
        match mode {
            // Category path (`XloggerWrite(instance_ptr != 0)`): C++ fills only
            // when all 3 fields are -1, otherwise keeps user-provided values.
            MetaResolveMode::Category => {
                if raw_meta.pid == -1 && raw_meta.tid == -1 && raw_meta.maintid == -1 {
                    (runtime_pid, runtime_tid, runtime_maintid)
                } else {
                    (raw_meta.pid, raw_meta.tid, raw_meta.maintid)
                }
            }
            // Global path (`XloggerWrite(instance_ptr == 0)`): C++ fills each
            // field independently when that field equals -1.
            MetaResolveMode::Global => (
                if raw_meta.pid == -1 {
                    runtime_pid
                } else {
                    raw_meta.pid
                },
                if raw_meta.tid == -1 {
                    runtime_tid
                } else {
                    raw_meta.tid
                },
                if raw_meta.maintid == -1 {
                    runtime_maintid
                } else {
                    raw_meta.maintid
                },
            ),
        }
    }

    fn write_with_meta_internal(
        &self,
        level: LogLevel,
        tag: &str,
        file: &str,
        func: &str,
        line: u32,
        msg: &str,
        raw_meta: RawLogMeta,
        resolve_mode: MetaResolveMode,
    ) {
        if !self.is_enabled(level) {
            return;
        }

        #[cfg(target_os = "android")]
        let trace_console_bypass = raw_meta.trace_log;
        #[cfg(not(target_os = "android"))]
        let trace_console_bypass = false;

        if self.console_open.load(Ordering::Relaxed) || trace_console_bypass {
            write_console_line(to_console_level(level), tag, file, func, line, msg);
        }

        let (pid, tid, maintid) = self.resolve_record_meta(raw_meta, resolve_mode);

        if self.engine.mode() == EngineMode::Async {
            self.write_async_line(level, tag, file, func, line, msg, pid, tid, maintid);
            return;
        }

        let Some(block) = self.build_block(level, tag, file, func, line, msg, pid, tid, maintid)
        else {
            return;
        };
        let _ = self.engine.write_block(&block, level == LogLevel::Fatal);
    }

    fn new_async_pending_state(&self, hour: u8, flush_epoch: u64) -> Option<AsyncPendingState> {
        let compression_kind = match self.config.compress_mode {
            CompressMode::Zlib => CompressionKind::Zlib,
            CompressMode::Zstd => CompressionKind::Zstd,
        };
        let compressor = match self.config.compress_mode {
            CompressMode::Zlib => AsyncCompressor::Zlib(ZlibStreamCompressor::default()),
            CompressMode::Zstd => {
                AsyncCompressor::Zstd(ZstdStreamCompressor::new(self.config.compress_level).ok()?)
            }
        };
        Some(AsyncPendingState {
            header: LogHeader {
                magic: select_magic(compression_kind, AppendMode::Async, self.cipher.enabled()),
                seq: global_async_seq().next_async(),
                begin_hour: hour,
                end_hour: hour,
                len: 0,
                client_pubkey: if self.cipher.enabled() {
                    self.cipher.client_pubkey()
                } else {
                    [0; 64]
                },
            },
            payload: Vec::new(),
            compressor,
            crypt_tail: Vec::new(),
            flush_epoch,
        })
    }

    fn write_async_line(
        &self,
        level: LogLevel,
        tag: &str,
        file: &str,
        func: &str,
        line: u32,
        msg: &str,
        pid: i64,
        tid: i64,
        maintid: i64,
    ) {
        let line = self.format_record_line(level, tag, file, func, line, msg, pid, tid, maintid);
        let now_hour = chrono::Timelike::hour(&Local::now()) as u8;
        let engine_epoch = self.engine.async_flush_epoch();
        let capacity = self
            .engine
            .async_buffer_stats()
            .map(|(_, cap)| cap)
            .unwrap_or(DEFAULT_BUFFER_BLOCK_LEN);

        let mut state_guard = self.async_state.lock().expect("async state lock poisoned");
        let stale = state_guard
            .as_ref()
            .map(|s| s.flush_epoch != engine_epoch)
            .unwrap_or(false);
        if stale {
            *state_guard = None;
        }
        if state_guard.is_none() {
            *state_guard = self.new_async_pending_state(now_hour, engine_epoch);
        }
        let Some(state) = state_guard.as_mut() else {
            return;
        };

        let threshold =
            capacity.saturating_mul(ASYNC_WARNING_THRESHOLD_NUM) / ASYNC_WARNING_THRESHOLD_DEN;
        let current_len = HEADER_LEN + state.payload.len();
        let input = if current_len >= threshold {
            format!("[F][ sg_buffer_async.Length() >= BUFFER_BLOCK_LENTH*4/5, len: {current_len}\n")
                .into_bytes()
        } else {
            line.into_bytes()
        };

        state.header.end_hour = now_hour;
        if !state.append_chunk(&input, &self.cipher) {
            *state_guard = None;
            return;
        }
        let Some(pending) = state.pending_bytes_without_tailer() else {
            *state_guard = None;
            return;
        };

        if self
            .engine
            .write_async_pending(&pending, level == LogLevel::Fatal)
            .is_err()
        {
            if let Some(block) = state_guard.take().and_then(|s| s.finalize(&self.cipher)) {
                self.persist_finalized_async_block(&block, level == LogLevel::Fatal);
                let _ = self.engine.flush(true);
            }
        }
    }

    fn finalize_async_pending(&self) {
        let pending_block = {
            let mut state_guard = self.async_state.lock().expect("async state lock poisoned");
            state_guard.take().and_then(|s| s.finalize(&self.cipher))
        };
        if let Some(block) = pending_block {
            self.persist_finalized_async_block(&block, false);
        }
    }

    fn persist_finalized_async_block(&self, block: &[u8], force_flush: bool) {
        match self.engine.write_async_pending(block, force_flush) {
            Ok(()) => {}
            Err(AppenderEngineError::InvalidMode) => {
                if self.engine.write_block(block, force_flush).is_err() {
                    let _ = self.engine.flush(true);
                }
            }
            Err(_) => {
                if self.engine.write_block(block, force_flush).is_err() {
                    let _ = self.engine.flush(true);
                }
            }
        }
    }

    fn make_logfile_name_impl(&self, timespan: i32, prefix: &str) -> Vec<String> {
        self.engine.make_logfile_name(timespan, prefix)
    }

    fn filepaths_from_timespan_impl(&self, timespan: i32, prefix: &str) -> Vec<String> {
        self.engine.filepaths_from_timespan(timespan, prefix)
    }
}

impl XlogBackendProvider for RustBackendProvider {
    fn new_instance(
        &self,
        config: &XlogConfig,
        level: LogLevel,
    ) -> Result<Arc<dyn XlogBackend>, XlogError> {
        let backend = registry().get_or_try_insert_with(&config.name_prefix, || {
            Ok::<_, XlogError>(Arc::new(RustBackend::new(config.clone(), level)?))
        })?;
        Ok(backend)
    }

    fn get_instance(&self, name_prefix: &str) -> Option<Arc<dyn XlogBackend>> {
        registry()
            .get(name_prefix)
            .map(|v| v as Arc<dyn XlogBackend>)
    }

    fn appender_open(&self, config: &XlogConfig, level: LogLevel) -> Result<(), XlogError> {
        if let Some(default) = registry().default_instance() {
            default.set_level(level);
            return Ok(());
        }
        let backend = Arc::new(RustBackend::new(config.clone(), level)?);
        let max_file_size = GLOBAL_MAX_FILE_SIZE.load(Ordering::Relaxed) as i64;
        let max_alive_time = GLOBAL_MAX_ALIVE_TIME.load(Ordering::Relaxed);
        let console_open = GLOBAL_CONSOLE_OPEN.load(Ordering::Relaxed);
        backend.set_max_file_size(max_file_size);
        backend.set_max_alive_time(max_alive_time);
        backend.set_console_log_open(console_open);
        registry().set_default(backend);
        Ok(())
    }

    fn appender_close(&self) {
        registry().clear_default();
    }

    fn flush_all(&self, sync: bool) {
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

    fn global_is_enabled(&self, level: LogLevel) -> bool {
        registry()
            .default_instance()
            .map(|b| b.is_enabled(level))
            .unwrap_or(false)
    }

    fn write_global_with_meta(
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
        default.write_with_meta_internal(
            level,
            tag,
            file,
            func,
            line,
            msg,
            raw_meta,
            MetaResolveMode::Global,
        );
    }

    #[cfg(any(
        target_os = "ios",
        target_os = "macos",
        target_os = "tvos",
        target_os = "watchos"
    ))]
    fn set_console_fun(&self, fun: ConsoleFun) {
        let core_fun = match fun {
            ConsoleFun::Printf => AppleConsoleFun::Printf,
            ConsoleFun::NSLog => AppleConsoleFun::NsLog,
            ConsoleFun::OSLog => AppleConsoleFun::OsLog,
        };
        set_apple_console_fun(core_fun);
    }

    fn current_log_path(&self) -> Option<String> {
        registry()
            .default_instance()
            .and_then(|b| b.engine.log_dir())
    }

    fn current_log_cache_path(&self) -> Option<String> {
        registry()
            .default_instance()
            .and_then(|b| b.engine.cache_dir())
    }

    fn filepaths_from_timespan(&self, timespan: i32, prefix: &str) -> Vec<String> {
        registry()
            .default_instance()
            .map(|b| b.filepaths_from_timespan_impl(timespan, prefix))
            .unwrap_or_default()
    }

    fn make_logfile_name(&self, timespan: i32, prefix: &str) -> Vec<String> {
        registry()
            .default_instance()
            .map(|b| b.make_logfile_name_impl(timespan, prefix))
            .unwrap_or_default()
    }

    fn oneshot_flush(&self, config: &XlogConfig) -> Result<FileIoAction, XlogError> {
        if config.log_dir.is_empty() || config.name_prefix.is_empty() {
            return Err(XlogError::InvalidConfig);
        }

        let file_manager = FileManager::new(
            config.log_dir.clone().into(),
            config.cache_dir.clone().map(Into::into),
            config.name_prefix.clone(),
            config.cache_days,
        )
        .map_err(|_| XlogError::InitFailed)?;

        let max_file_size = registry()
            .get(&config.name_prefix)
            .or_else(|| registry().default_instance())
            .map(|b| b.engine.max_file_size())
            .unwrap_or_else(|| GLOBAL_MAX_FILE_SIZE.load(Ordering::Relaxed));

        let action = core_oneshot_flush(&file_manager, DEFAULT_BUFFER_BLOCK_LEN, max_file_size);
        Ok(match action {
            CoreFileIoAction::None => FileIoAction::None,
            CoreFileIoAction::Success => FileIoAction::Success,
            CoreFileIoAction::Unnecessary => FileIoAction::Unnecessary,
            CoreFileIoAction::OpenFailed => FileIoAction::OpenFailed,
            CoreFileIoAction::ReadFailed => FileIoAction::ReadFailed,
            CoreFileIoAction::WriteFailed => FileIoAction::WriteFailed,
            CoreFileIoAction::CloseFailed => FileIoAction::CloseFailed,
            CoreFileIoAction::RemoveFailed => FileIoAction::RemoveFailed,
        })
    }

    fn dump(&self, buffer: &[u8]) -> String {
        let Some(default) = registry().default_instance() else {
            return String::new();
        };
        let Some(log_dir) = default.engine.log_dir() else {
            return String::new();
        };
        dump_to_file(&log_dir, buffer)
    }

    fn memory_dump(&self, buffer: &[u8]) -> String {
        memory_dump(buffer)
    }
}

impl XlogBackend for RustBackend {
    fn instance(&self) -> usize {
        self.id
    }

    fn is_enabled(&self, level: LogLevel) -> bool {
        level_to_i32(level) >= self.level.load(Ordering::Relaxed)
    }

    fn level(&self) -> LogLevel {
        i32_to_level(self.level.load(Ordering::Relaxed))
    }

    fn set_level(&self, level: LogLevel) {
        self.level.store(level_to_i32(level), Ordering::Relaxed);
    }

    fn set_appender_mode(&self, mode: AppenderMode) {
        if mode == AppenderMode::Sync && self.engine.mode() == EngineMode::Async {
            self.finalize_async_pending();
        }
        let _ = self.engine.set_mode(appender_to_engine_mode(mode));
    }

    fn flush(&self, sync: bool) {
        if self.engine.mode() == EngineMode::Async {
            self.finalize_async_pending();
        }
        let _ = self.engine.flush(sync);
    }

    fn set_console_log_open(&self, open: bool) {
        self.console_open.store(open, Ordering::Relaxed);
        GLOBAL_CONSOLE_OPEN.store(open, Ordering::Relaxed);
    }

    fn set_max_file_size(&self, max_bytes: i64) {
        let v = max_bytes.max(0) as u64;
        self.engine.set_max_file_size(v);
        GLOBAL_MAX_FILE_SIZE.store(v, Ordering::Relaxed);
    }

    fn set_max_alive_time(&self, alive_seconds: i64) {
        self.engine.set_max_alive_time(alive_seconds);
        GLOBAL_MAX_ALIVE_TIME.store(alive_seconds, Ordering::Relaxed);
    }

    fn write_with_meta(
        &self,
        level: LogLevel,
        tag: &str,
        file: &str,
        func: &str,
        line: u32,
        msg: &str,
        raw_meta: RawLogMeta,
    ) {
        self.write_with_meta_internal(
            level,
            tag,
            file,
            func,
            line,
            msg,
            raw_meta,
            MetaResolveMode::Category,
        );
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
    match level {
        LogLevel::Verbose => CoreLogLevel::Verbose,
        LogLevel::Debug => CoreLogLevel::Debug,
        LogLevel::Info => CoreLogLevel::Info,
        LogLevel::Warn => CoreLogLevel::Warn,
        LogLevel::Error => CoreLogLevel::Error,
        LogLevel::Fatal => CoreLogLevel::Fatal,
        LogLevel::None => CoreLogLevel::None,
    }
}

fn appender_to_engine_mode(mode: AppenderMode) -> EngineMode {
    match mode {
        AppenderMode::Async => EngineMode::Async,
        AppenderMode::Sync => EngineMode::Sync,
    }
}

fn engine_to_appender_mode(mode: EngineMode) -> AppenderMode {
    match mode {
        EngineMode::Async => AppenderMode::Async,
        EngineMode::Sync => AppenderMode::Sync,
    }
}

fn to_console_level(level: LogLevel) -> ConsoleLevel {
    match level {
        LogLevel::Verbose => ConsoleLevel::Verbose,
        LogLevel::Debug => ConsoleLevel::Debug,
        LogLevel::Info => ConsoleLevel::Info,
        LogLevel::Warn => ConsoleLevel::Warn,
        LogLevel::Error => ConsoleLevel::Error,
        LogLevel::Fatal => ConsoleLevel::Fatal,
        LogLevel::None => ConsoleLevel::None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use mars_xlog_core::buffer::DEFAULT_BUFFER_BLOCK_LEN;
    use mars_xlog_core::compress::{decompress_raw_zlib, decompress_zstd_frames};
    use mars_xlog_core::crypto::{tea_decrypt_in_place, EcdhTeaCipher};
    use mars_xlog_core::protocol::{
        LogHeader, HEADER_LEN, MAGIC_ASYNC_ZLIB_START, MAGIC_ASYNC_ZSTD_START, MAGIC_END,
        MAGIC_SYNC_ZLIB_START, TAILER_LEN,
    };

    use super::RustBackend;
    use crate::backend::XlogBackend;
    use crate::{AppenderMode, LogLevel, RawLogMeta, XlogConfig};

    const TEST_SERVER_PUBKEY_HEX: &str = concat!(
        "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
        "483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8"
    );
    const TEST_SERVER_PRIVKEY_ONE: [u8; 32] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 1,
    ];

    fn bytes_to_hex(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
        out
    }

    fn decrypt_async_payload(header: &LogHeader, payload: &[u8]) -> Vec<u8> {
        if header.client_pubkey == [0; 64] {
            return payload.to_vec();
        }
        let client_pub_hex = bytes_to_hex(&header.client_pubkey);
        let cipher =
            EcdhTeaCipher::new_with_private_key(&client_pub_hex, TEST_SERVER_PRIVKEY_ONE).unwrap();

        let mut out = payload.to_vec();
        let block_end = out.len() / 8 * 8;
        tea_decrypt_in_place(&mut out[..block_end], &cipher.tea_key_words());
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
        let is_async = matches!(header.magic, 0x07 | 0x09 | 0x0C | 0x0D);
        if !is_async {
            return payload.to_vec();
        }

        let raw = if matches!(header.magic, 0x07 | 0x0C) {
            decrypt_async_payload(header, payload)
        } else {
            payload.to_vec()
        };

        match header.magic {
            0x07 | 0x09 => decompress_raw_zlib(&raw).unwrap(),
            0x0C | 0x0D => decompress_zstd_frames(&raw).unwrap(),
            _ => raw,
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

        let block = backend
            .build_block(
                LogLevel::Info,
                "tag",
                "main.rs",
                "f",
                7,
                "plain-sync",
                std::process::id() as i64,
                super::current_tid(),
                super::main_tid(),
            )
            .unwrap();
        let (header, payload) = parse_block_payload(&block);
        assert_eq!(header.magic, MAGIC_SYNC_ZLIB_START);
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
        let block = backend
            .build_block(
                LogLevel::Info,
                "tag",
                "main.rs",
                "f",
                9,
                "maintid",
                std::process::id() as i64,
                super::current_tid(),
                super::main_tid(),
            )
            .unwrap();
        let (_header, payload) = parse_block_payload(&block);
        let line = std::str::from_utf8(payload).unwrap();

        if super::current_tid() == super::main_tid() {
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
        assert_eq!(pid, std::process::id() as i64);
        assert_eq!(tid, super::current_tid());
        assert_eq!(maintid, super::main_tid());

        let (pid, tid, maintid) = backend.resolve_record_meta(
            RawLogMeta::new(123, -1, -1),
            super::MetaResolveMode::Category,
        );
        assert_eq!(pid, 123);
        assert_eq!(tid, -1);
        assert_eq!(maintid, -1);

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
        assert_eq!(pid, 321);
        assert_eq!(tid, super::current_tid());
        assert_eq!(maintid, super::main_tid());

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
        assert_eq!(header.magic, MAGIC_ASYNC_ZLIB_START);
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
        assert_eq!(header.magic, MAGIC_ASYNC_ZSTD_START);
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
            let mut state = backend.new_async_pending_state(1, engine_epoch).unwrap();
            while HEADER_LEN + state.payload.len() < threshold {
                assert!(state.append_chunk(b"fill", &backend.cipher));
            }
            *guard = Some(state);
        }

        backend.write_async_line(
            LogLevel::Info,
            "tag",
            "watermark.rs",
            "f",
            10,
            "ORIGINAL-LINE-SHOULD-BE-DROPPED",
            std::process::id() as i64,
            super::current_tid(),
            super::main_tid(),
        );

        let pending = {
            let guard = backend
                .async_state
                .lock()
                .expect("async state lock poisoned");
            guard
                .as_ref()
                .and_then(|s| s.pending_bytes_without_tailer())
                .unwrap()
        };
        let header = LogHeader::decode(&pending[..HEADER_LEN]).unwrap();
        let payload = &pending[HEADER_LEN..];
        let plain = decode_block_payload(&header, payload);
        let text = std::str::from_utf8(&plain).unwrap();
        assert!(text.contains("sg_buffer_async.Length() >= BUFFER_BLOCK_LENTH*4/5"));
        assert!(!text.contains("ORIGINAL-LINE-SHOULD-BE-DROPPED"));
        let _ = fs::remove_dir_all(&root);
    }
}
