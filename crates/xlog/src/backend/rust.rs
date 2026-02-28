use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use chrono::Local;
use mars_xlog_core::buffer::{PersistentBuffer, DEFAULT_BUFFER_BLOCK_LEN};
use mars_xlog_core::compress::{StreamCompressor, ZlibStreamCompressor, ZstdChunkCompressor};
use mars_xlog_core::crypto::EcdhTeaCipher;
use mars_xlog_core::file_manager::FileManager;
use mars_xlog_core::formatter::format_record;
use mars_xlog_core::oneshot::{
    oneshot_flush as core_oneshot_flush, FileIoAction as CoreFileIoAction,
};
use mars_xlog_core::protocol::{
    select_magic, AppendMode, CompressionKind, LogHeader, SeqGenerator, MAGIC_END,
};
use mars_xlog_core::record::{LogLevel as CoreLogLevel, LogRecord};

use super::{XlogBackend, XlogBackendProvider};
use crate::{AppenderMode, CompressMode, FileIoAction, LogLevel, XlogConfig, XlogError};

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

struct BackendRuntime {
    file_manager: FileManager,
    buffer: PersistentBuffer,
}

struct RustBackend {
    id: usize,
    config: XlogConfig,
    level: AtomicI32,
    appender_mode: AtomicI32,
    console_open: AtomicBool,
    max_file_size: AtomicI64,
    max_alive_time: AtomicI64,
    seq: SeqGenerator,
    cipher: EcdhTeaCipher,
    runtime: Mutex<BackendRuntime>,
}

static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

fn instances() -> &'static Mutex<HashMap<String, Weak<RustBackend>>> {
    static INSTANCES: OnceLock<Mutex<HashMap<String, Weak<RustBackend>>>> = OnceLock::new();
    INSTANCES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn default_backend() -> &'static Mutex<Option<Arc<RustBackend>>> {
    static DEFAULT: OnceLock<Mutex<Option<Arc<RustBackend>>>> = OnceLock::new();
    DEFAULT.get_or_init(|| Mutex::new(None))
}

impl RustBackend {
    fn new(config: XlogConfig, level: LogLevel) -> Result<Self, XlogError> {
        if config.log_dir.is_empty() || config.name_prefix.is_empty() {
            return Err(XlogError::InvalidConfig);
        }

        let cipher = match config.pub_key.as_deref() {
            Some(key) if !key.is_empty() => {
                EcdhTeaCipher::new(key).map_err(|_| XlogError::InitFailed)?
            }
            _ => EcdhTeaCipher::disabled(),
        };

        let file_manager = FileManager::new(
            PathBuf::from(&config.log_dir),
            config.cache_dir.as_ref().map(PathBuf::from),
            config.name_prefix.clone(),
            config.cache_days,
        )
        .map_err(|_| XlogError::InitFailed)?;
        let mmap_path = file_manager.mmap_path();
        let buffer = PersistentBuffer::open_with_capacity(mmap_path, DEFAULT_BUFFER_BLOCK_LEN)
            .map_err(|_| XlogError::InitFailed)?;

        Ok(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            max_file_size: AtomicI64::new(0),
            max_alive_time: AtomicI64::new(10 * 24 * 60 * 60),
            appender_mode: AtomicI32::new(mode_to_i32(config.mode)),
            console_open: AtomicBool::new(false),
            level: AtomicI32::new(level_to_i32(level)),
            config,
            seq: SeqGenerator::default(),
            cipher,
            runtime: Mutex::new(BackendRuntime {
                file_manager,
                buffer,
            }),
        })
    }

    fn max_file_size_u64(&self) -> u64 {
        self.max_file_size.load(Ordering::Relaxed).max(0) as u64
    }

    fn max_alive_time_i64(&self) -> i64 {
        self.max_alive_time.load(Ordering::Relaxed)
    }

    fn build_block(
        &self,
        level: LogLevel,
        tag: &str,
        file: &str,
        func: &str,
        line: u32,
        msg: &str,
    ) -> Vec<u8> {
        let mode = i32_to_mode(self.appender_mode.load(Ordering::Relaxed));
        let compress = self.config.compress_mode;

        let now = Local::now();
        let pid = std::process::id() as i64;
        let tid = current_tid();

        let record = LogRecord {
            level: to_core_level(level),
            tag: tag.to_string(),
            filename: file.to_string(),
            func_name: func.to_string(),
            line: line as i32,
            timestamp: std::time::SystemTime::now(),
            pid,
            tid,
            maintid: pid,
        };
        let line = format_record(&record, msg);

        let mut payload = match mode {
            AppenderMode::Sync => line.into_bytes(),
            AppenderMode::Async => match compress {
                CompressMode::Zlib => {
                    let mut c = ZlibStreamCompressor::default();
                    let mut out = Vec::new();
                    let _ = c.compress_chunk(line.as_bytes(), &mut out);
                    let _ = c.flush(&mut out);
                    out
                }
                CompressMode::Zstd => {
                    let mut c = ZstdChunkCompressor::new(self.config.compress_level);
                    let mut out = Vec::new();
                    let _ = c.compress_chunk(line.as_bytes(), &mut out);
                    let _ = c.flush(&mut out);
                    out
                }
            },
        };

        if self.cipher.enabled() {
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
            magic: select_magic(compression_kind, append_mode, self.cipher.enabled()),
            seq: match mode {
                AppenderMode::Sync => SeqGenerator::sync_seq(),
                AppenderMode::Async => self.seq.next_async(),
            },
            begin_hour: chrono::Timelike::hour(&now) as u8,
            end_hour: chrono::Timelike::hour(&now) as u8,
            len: payload.len() as u32,
            client_pubkey: self.cipher.client_pubkey(),
        };

        let mut block = Vec::with_capacity(73 + payload.len() + 1);
        block.extend_from_slice(&header.encode());
        block.extend_from_slice(&payload);
        block.push(MAGIC_END);
        block
    }

    fn flush_pending_locked(
        &self,
        runtime: &mut BackendRuntime,
        move_file: bool,
    ) -> Result<(), io::Error> {
        let pending = runtime
            .buffer
            .take_all()
            .map_err(|e| io::Error::other(e.to_string()))?;
        if pending.is_empty() {
            return Ok(());
        }
        runtime
            .file_manager
            .append_log_bytes(&pending, self.max_file_size_u64(), move_file)
            .map_err(|e| io::Error::other(e.to_string()))
    }

    fn housekeeping_locked(&self, runtime: &BackendRuntime) -> Result<(), io::Error> {
        runtime
            .file_manager
            .move_old_cache_files(self.max_file_size_u64())
            .map_err(|e| io::Error::other(e.to_string()))?;
        runtime
            .file_manager
            .delete_expired_files(self.max_alive_time_i64())
            .map_err(|e| io::Error::other(e.to_string()))
    }

    fn make_logfile_name_impl(&self, timespan: i32, prefix: &str) -> Vec<String> {
        let Ok(runtime) = self.runtime.lock() else {
            return Vec::new();
        };
        runtime
            .file_manager
            .make_logfile_name(timespan, prefix, self.max_file_size_u64())
    }

    fn filepaths_from_timespan_impl(&self, timespan: i32, prefix: &str) -> Vec<String> {
        let Ok(runtime) = self.runtime.lock() else {
            return Vec::new();
        };
        runtime
            .file_manager
            .filepaths_from_timespan(timespan, prefix)
    }

    fn write_impl(
        &self,
        level: LogLevel,
        tag: &str,
        file: &str,
        func: &str,
        line: u32,
        msg: &str,
    ) -> Result<(), io::Error> {
        let block = self.build_block(level, tag, file, func, line, msg);
        let mode = i32_to_mode(self.appender_mode.load(Ordering::Relaxed));

        let mut runtime = self.runtime.lock().expect("runtime lock poisoned");
        match mode {
            AppenderMode::Sync => {
                runtime
                    .file_manager
                    .append_log_bytes(&block, self.max_file_size_u64(), false)
                    .map_err(|e| io::Error::other(e.to_string()))?;
            }
            AppenderMode::Async => {
                let appended = runtime
                    .buffer
                    .append_block(&block)
                    .map_err(|e| io::Error::other(e.to_string()))?;

                if !appended {
                    self.flush_pending_locked(&mut runtime, true)?;
                    let appended_after_flush = runtime
                        .buffer
                        .append_block(&block)
                        .map_err(|e| io::Error::other(e.to_string()))?;
                    if !appended_after_flush {
                        runtime
                            .file_manager
                            .append_log_bytes(&block, self.max_file_size_u64(), true)
                            .map_err(|e| io::Error::other(e.to_string()))?;
                    }
                }

                let threshold = runtime.buffer.capacity() / 3;
                if runtime.buffer.len() >= threshold || level == LogLevel::Fatal {
                    self.flush_pending_locked(&mut runtime, true)?;
                }
            }
        }

        self.housekeeping_locked(&runtime)
    }
}

impl XlogBackendProvider for RustBackendProvider {
    fn new_instance(
        &self,
        config: &XlogConfig,
        level: LogLevel,
    ) -> Result<Arc<dyn XlogBackend>, XlogError> {
        let mut map = instances().lock().expect("instances lock poisoned");
        if let Some(existing) = map.get(&config.name_prefix).and_then(Weak::upgrade) {
            return Ok(existing);
        }

        let backend = Arc::new(RustBackend::new(config.clone(), level)?);
        map.insert(config.name_prefix.clone(), Arc::downgrade(&backend));
        Ok(backend)
    }

    fn get_instance(&self, name_prefix: &str) -> Option<Arc<dyn XlogBackend>> {
        let map = instances().lock().ok()?;
        let backend = map.get(name_prefix)?.upgrade()?;
        Some(backend)
    }

    fn appender_open(&self, config: &XlogConfig, level: LogLevel) -> Result<(), XlogError> {
        let backend = Arc::new(RustBackend::new(config.clone(), level)?);
        let mut d = default_backend()
            .lock()
            .expect("default backend lock poisoned");
        *d = Some(backend);
        Ok(())
    }

    fn appender_close(&self) {
        let mut d = default_backend()
            .lock()
            .expect("default backend lock poisoned");
        *d = None;
    }

    fn flush_all(&self, sync: bool) {
        if let Some(d) = default_backend()
            .lock()
            .expect("default backend lock poisoned")
            .clone()
        {
            d.flush(sync);
        }

        let map = instances().lock().expect("instances lock poisoned");
        for backend in map.values().filter_map(Weak::upgrade) {
            backend.flush(sync);
        }
    }

    #[cfg(any(
        target_os = "ios",
        target_os = "macos",
        target_os = "tvos",
        target_os = "watchos"
    ))]
    fn set_console_fun(&self, _fun: ConsoleFun) {
        // no-op in rust backend for now
    }

    fn current_log_path(&self) -> Option<String> {
        default_backend()
            .lock()
            .ok()?
            .as_ref()
            .map(|b| b.config.log_dir.clone())
    }

    fn current_log_cache_path(&self) -> Option<String> {
        default_backend()
            .lock()
            .ok()?
            .as_ref()?
            .config
            .cache_dir
            .clone()
    }

    fn filepaths_from_timespan(&self, timespan: i32, prefix: &str) -> Vec<String> {
        default_backend()
            .lock()
            .ok()
            .and_then(|d| d.clone())
            .map(|b| b.filepaths_from_timespan_impl(timespan, prefix))
            .unwrap_or_default()
    }

    fn make_logfile_name(&self, timespan: i32, prefix: &str) -> Vec<String> {
        default_backend()
            .lock()
            .ok()
            .and_then(|d| d.clone())
            .map(|b| b.make_logfile_name_impl(timespan, prefix))
            .unwrap_or_default()
    }

    fn oneshot_flush(&self, config: &XlogConfig) -> Result<FileIoAction, XlogError> {
        if config.log_dir.is_empty() || config.name_prefix.is_empty() {
            return Err(XlogError::InvalidConfig);
        }

        let file_manager = FileManager::new(
            PathBuf::from(&config.log_dir),
            config.cache_dir.as_ref().map(PathBuf::from),
            config.name_prefix.clone(),
            config.cache_days,
        )
        .map_err(|_| XlogError::InitFailed)?;

        let max_file_size = default_backend()
            .lock()
            .ok()
            .and_then(|d| d.as_ref().map(|b| b.max_file_size_u64()))
            .unwrap_or(0);

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
        memory_dump_impl(buffer)
    }

    fn memory_dump(&self, buffer: &[u8]) -> String {
        memory_dump_impl(buffer)
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
        self.appender_mode
            .store(mode_to_i32(mode), Ordering::Relaxed);
    }

    fn flush(&self, _sync: bool) {
        let mut runtime = match self.runtime.lock() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let _ = self.flush_pending_locked(&mut runtime, true);
        let _ = self.housekeeping_locked(&runtime);
    }

    fn set_console_log_open(&self, open: bool) {
        self.console_open.store(open, Ordering::Relaxed);
    }

    fn set_max_file_size(&self, max_bytes: i64) {
        self.max_file_size.store(max_bytes, Ordering::Relaxed);
    }

    fn set_max_alive_time(&self, alive_seconds: i64) {
        if alive_seconds >= 24 * 60 * 60 {
            self.max_alive_time.store(alive_seconds, Ordering::Relaxed);
        }
    }

    fn write_with_meta(
        &self,
        level: LogLevel,
        tag: &str,
        file: &str,
        func: &str,
        line: u32,
        msg: &str,
    ) {
        if !self.is_enabled(level) {
            return;
        }

        let _ = self.write_impl(level, tag, file, func, line, msg);
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

fn mode_to_i32(mode: AppenderMode) -> i32 {
    match mode {
        AppenderMode::Async => 0,
        AppenderMode::Sync => 1,
    }
}

fn i32_to_mode(v: i32) -> AppenderMode {
    if v == 1 {
        AppenderMode::Sync
    } else {
        AppenderMode::Async
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

fn current_tid() -> i64 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        unsafe { libc::syscall(libc::SYS_gettid) as i64 }
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos"
    ))]
    {
        let mut tid: u64 = 0;
        unsafe {
            libc::pthread_threadid_np(0, &mut tid);
        }
        tid as i64
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos"
    )))]
    {
        -1
    }
}

fn memory_dump_impl(buffer: &[u8]) -> String {
    if buffer.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push('\n');
    out.push_str(&format!("{} bytes:\n", buffer.len()));

    let mut offset = 0usize;
    while offset < buffer.len() && out.len() < 4096 {
        let end = std::cmp::min(offset + 16, buffer.len());
        let slice = &buffer[offset..end];
        for b in slice {
            out.push_str(&format!("{:02x} ", b));
        }
        out.push('\n');
        for b in slice {
            let c = if (*b as char).is_ascii_graphic() {
                *b as char
            } else {
                ' '
            };
            out.push(c);
            out.push_str("  ");
        }
        out.push_str("\n\n");
        offset += slice.len();
    }

    out
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::RustBackend;
    use crate::backend::XlogBackend;
    use crate::LogLevel;

    #[test]
    fn rust_backend_writes_xlog_block() {
        let root = std::env::temp_dir().join(format!("xlog-rust-backend-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let cfg = crate::XlogConfig::new(root.to_string_lossy().to_string(), "demo");
        let backend = RustBackend::new(cfg, LogLevel::Info).unwrap();

        backend.write_with_meta(LogLevel::Info, "demo", "main.rs", "f", 1, "hello");
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
}
