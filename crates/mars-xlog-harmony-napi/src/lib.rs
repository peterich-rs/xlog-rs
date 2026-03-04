//! N-API bindings for Harmony/ohos.
//!
//! This crate exposes a JS-friendly surface that mirrors the core `mars-xlog`
//! capability set, including global appender controls and metadata-aware writes.
use mars_xlog::{self, RawLogMeta, Xlog};
use napi_derive_ohos::napi;
use napi_ohos::bindgen_prelude::Buffer;

/// Simple smoke-test function to verify the binding works.
#[napi]
pub fn add(left: u32, right: u32) -> u32 {
    left + right
}

#[napi]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Level {
    Verbose,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
    None,
}

#[napi]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum AppenderMode {
    Async,
    Sync,
}

#[napi]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CompressMode {
    Zlib,
    Zstd,
}

#[napi]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum FileIoAction {
    None,
    Success,
    Unnecessary,
    OpenFailed,
    ReadFailed,
    WriteFailed,
    CloseFailed,
    RemoveFailed,
}

#[napi(constructor)]
#[derive(Debug, Clone)]
pub struct XlogConfig {
    /// Directory for log files.
    pub log_dir: String,
    /// Prefix for log file names and instance id.
    pub name_prefix: String,
    /// Public key for encrypted logs (empty string disables encryption).
    pub pub_key: String,
    /// Cache directory for mmap buffers and temporary logs.
    pub cache_dir: String,
    /// Days to keep cached logs before moving them.
    pub cache_days: i32,
    /// Appender mode.
    pub mode: AppenderMode,
    /// Compression mode.
    pub compress_mode: CompressMode,
    /// Compression level.
    pub compress_level: i32,
    /// Enable console logging.
    pub console: bool,
    /// Minimum log level.
    pub level: Level,
}

/// Plain object form used by global helper functions.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct XlogConfigInput {
    pub log_dir: String,
    pub name_prefix: String,
    pub pub_key: String,
    pub cache_dir: String,
    pub cache_days: i32,
    pub mode: AppenderMode,
    pub compress_mode: CompressMode,
    pub compress_level: i32,
}

fn to_core_level(level: Level) -> mars_xlog::LogLevel {
    match level {
        Level::Verbose => mars_xlog::LogLevel::Verbose,
        Level::Debug => mars_xlog::LogLevel::Debug,
        Level::Info => mars_xlog::LogLevel::Info,
        Level::Warn => mars_xlog::LogLevel::Warn,
        Level::Error => mars_xlog::LogLevel::Error,
        Level::Fatal => mars_xlog::LogLevel::Fatal,
        Level::None => mars_xlog::LogLevel::None,
    }
}

fn from_core_level(level: mars_xlog::LogLevel) -> Level {
    match level {
        mars_xlog::LogLevel::Verbose => Level::Verbose,
        mars_xlog::LogLevel::Debug => Level::Debug,
        mars_xlog::LogLevel::Info => Level::Info,
        mars_xlog::LogLevel::Warn => Level::Warn,
        mars_xlog::LogLevel::Error => Level::Error,
        mars_xlog::LogLevel::Fatal => Level::Fatal,
        mars_xlog::LogLevel::None => Level::None,
    }
}

fn to_core_appender_mode(mode: AppenderMode) -> mars_xlog::AppenderMode {
    match mode {
        AppenderMode::Async => mars_xlog::AppenderMode::Async,
        AppenderMode::Sync => mars_xlog::AppenderMode::Sync,
    }
}

fn to_core_compress_mode(mode: CompressMode) -> mars_xlog::CompressMode {
    match mode {
        CompressMode::Zlib => mars_xlog::CompressMode::Zlib,
        CompressMode::Zstd => mars_xlog::CompressMode::Zstd,
    }
}

fn from_core_file_io_action(action: mars_xlog::FileIoAction) -> FileIoAction {
    match action {
        mars_xlog::FileIoAction::None => FileIoAction::None,
        mars_xlog::FileIoAction::Success => FileIoAction::Success,
        mars_xlog::FileIoAction::Unnecessary => FileIoAction::Unnecessary,
        mars_xlog::FileIoAction::OpenFailed => FileIoAction::OpenFailed,
        mars_xlog::FileIoAction::ReadFailed => FileIoAction::ReadFailed,
        mars_xlog::FileIoAction::WriteFailed => FileIoAction::WriteFailed,
        mars_xlog::FileIoAction::CloseFailed => FileIoAction::CloseFailed,
        mars_xlog::FileIoAction::RemoveFailed => FileIoAction::RemoveFailed,
    }
}

fn to_core_config(cfg: &XlogConfig) -> mars_xlog::XlogConfig {
    let mut xlog_config = mars_xlog::XlogConfig::new(cfg.log_dir.clone(), cfg.name_prefix.clone())
        .cache_days(cfg.cache_days)
        .mode(to_core_appender_mode(cfg.mode))
        .compress_mode(to_core_compress_mode(cfg.compress_mode))
        .compress_level(cfg.compress_level);

    if !cfg.pub_key.is_empty() {
        xlog_config = xlog_config.pub_key(cfg.pub_key.clone());
    }
    if !cfg.cache_dir.is_empty() {
        xlog_config = xlog_config.cache_dir(cfg.cache_dir.clone());
    }

    xlog_config
}

fn to_core_config_input(cfg: &XlogConfigInput) -> mars_xlog::XlogConfig {
    let mut xlog_config = mars_xlog::XlogConfig::new(cfg.log_dir.clone(), cfg.name_prefix.clone())
        .cache_days(cfg.cache_days)
        .mode(to_core_appender_mode(cfg.mode))
        .compress_mode(to_core_compress_mode(cfg.compress_mode))
        .compress_level(cfg.compress_level);

    if !cfg.pub_key.is_empty() {
        xlog_config = xlog_config.pub_key(cfg.pub_key.clone());
    }
    if !cfg.cache_dir.is_empty() {
        xlog_config = xlog_config.cache_dir(cfg.cache_dir.clone());
    }

    xlog_config
}

fn to_u32_line(line: i32) -> u32 {
    if line < 0 {
        0
    } else {
        line as u32
    }
}

#[napi]
impl XlogConfig {
    /// Build a logger from the provided config.
    #[napi]
    pub fn build(&self) -> Logger {
        let xlog = Xlog::init(to_core_config(self), to_core_level(self.level))
            .unwrap_or_else(|e| panic!("init xlog failed: {e}"));
        xlog.set_console_log_open(self.console);
        Logger { backend: xlog }
    }
}

#[napi]
pub struct Logger {
    backend: Xlog,
}

#[napi]
impl Logger {
    /// Return whether logs at `level` are enabled.
    #[napi]
    pub fn is_enabled(&self, level: Level) -> bool {
        self.backend.is_enabled(to_core_level(level))
    }

    /// Return current logger level.
    #[napi]
    pub fn level(&self) -> Level {
        from_core_level(self.backend.level())
    }

    /// Set logger level.
    #[napi]
    pub fn set_level(&self, level: Level) {
        self.backend.set_level(to_core_level(level));
    }

    /// Set appender mode.
    #[napi]
    pub fn set_appender_mode(&self, mode: AppenderMode) {
        self.backend.set_appender_mode(to_core_appender_mode(mode));
    }

    /// Flush buffered logs.
    #[napi]
    pub fn flush(&self, sync: bool) {
        self.backend.flush(sync);
    }

    /// Toggle console logging.
    #[napi]
    pub fn set_console_log_open(&self, open: bool) {
        self.backend.set_console_log_open(open);
    }

    /// Set max file size in bytes.
    #[napi]
    pub fn set_max_file_size(&self, max_bytes: i64) {
        self.backend.set_max_file_size(max_bytes);
    }

    /// Set max alive time in seconds.
    #[napi]
    pub fn set_max_alive_time(&self, alive_seconds: i64) {
        self.backend.set_max_alive_time(alive_seconds);
    }

    /// Log a message with a tag.
    #[napi]
    pub fn log(&self, level: Level, tag: String, message: String) {
        self.backend
            .write(to_core_level(level), Some(&tag), &message);
    }

    /// Log with explicit metadata.
    #[napi]
    pub fn log_with_meta(
        &self,
        level: Level,
        tag: String,
        file: String,
        func: String,
        line: i32,
        message: String,
    ) {
        self.backend.write_with_meta(
            to_core_level(level),
            Some(&tag),
            &file,
            &func,
            to_u32_line(line),
            &message,
        );
    }

    /// Log with explicit metadata and raw pid/tid/trace flags.
    #[napi]
    pub fn log_with_raw_meta(
        &self,
        level: Level,
        tag: String,
        file: String,
        func: String,
        line: i32,
        pid: i64,
        tid: i64,
        maintid: i64,
        trace_log: bool,
        message: String,
    ) {
        let raw_meta = RawLogMeta::new(pid, tid, maintid).with_trace_log(trace_log);
        self.backend.write_with_meta_raw(
            to_core_level(level),
            Some(&tag),
            &file,
            &func,
            to_u32_line(line),
            &message,
            raw_meta,
        );
    }
}

/// Get an existing logger by name prefix.
#[napi]
pub fn get_logger(name_prefix: String) -> Option<Logger> {
    Xlog::get(&name_prefix).map(|backend| Logger { backend })
}

/// Open global/default appender.
#[napi]
pub fn open_appender(config: XlogConfigInput, level: Level) -> bool {
    Xlog::appender_open(to_core_config_input(&config), to_core_level(level)).is_ok()
}

/// Close global/default appender.
#[napi]
pub fn close_appender() {
    Xlog::appender_close();
}

/// Flush all instances.
#[napi]
pub fn flush_all(sync: bool) {
    Xlog::flush_all(sync);
}

/// Write to global/default appender with raw metadata.
#[napi]
pub fn appender_write_with_raw_meta(
    level: Level,
    tag: String,
    file: String,
    func: String,
    line: i32,
    pid: i64,
    tid: i64,
    maintid: i64,
    trace_log: bool,
    message: String,
) {
    let raw_meta = RawLogMeta::new(pid, tid, maintid).with_trace_log(trace_log);
    Xlog::appender_write_with_meta_raw(
        to_core_level(level),
        Some(&tag),
        &file,
        &func,
        to_u32_line(line),
        &message,
        raw_meta,
    );
}

/// Get current global log path.
#[napi]
pub fn current_log_path() -> String {
    Xlog::current_log_path().unwrap_or_default()
}

/// Get current global cache log path.
#[napi]
pub fn current_log_cache_path() -> String {
    Xlog::current_log_cache_path().unwrap_or_default()
}

/// List log files from a timespan.
#[napi]
pub fn filepaths_from_timespan(timespan: i32, prefix: String) -> Vec<String> {
    Xlog::filepaths_from_timespan(timespan, &prefix)
}

/// Build expected log file names for a timespan.
#[napi]
pub fn make_logfile_name(timespan: i32, prefix: String) -> Vec<String> {
    Xlog::make_logfile_name(timespan, &prefix)
}

/// Flush once and return file I/O action.
#[napi]
pub fn oneshot_flush(config: XlogConfigInput) -> FileIoAction {
    match Xlog::oneshot_flush(to_core_config_input(&config)) {
        Ok(action) => from_core_file_io_action(action),
        Err(_) => FileIoAction::None,
    }
}

/// Decode a raw xlog block buffer.
#[napi]
pub fn dump(buffer: Buffer) -> String {
    Xlog::dump(buffer.as_ref())
}

/// Decode a raw xlog block buffer from memory.
#[napi]
pub fn memory_dump(buffer: Buffer) -> String {
    Xlog::memory_dump(buffer.as_ref())
}
