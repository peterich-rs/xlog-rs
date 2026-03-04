//! UniFFI bindings for Mars Xlog.
//!
//! This crate exposes a high-level surface for Kotlin/Swift consumers and
//! mirrors the core `mars-xlog` capability set as closely as possible.
use mars_xlog as core;
use std::sync::OnceLock;
use tracing::info;
use tracing_subscriber::prelude::*;

uniffi::setup_scaffolding!("mars_xlog_uniffi");

/// Log levels exposed to UniFFI consumers.
#[derive(uniffi::Enum, Debug, Copy, Clone, PartialEq, Eq)]
pub enum LogLevel {
    Verbose,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
    None,
}

/// Appender mode exposed to UniFFI consumers.
#[derive(uniffi::Enum, Debug, Copy, Clone, PartialEq, Eq)]
pub enum AppenderMode {
    Async,
    Sync,
}

/// Compression mode exposed to UniFFI consumers.
#[derive(uniffi::Enum, Debug, Copy, Clone, PartialEq, Eq)]
pub enum CompressMode {
    Zlib,
    Zstd,
}

/// Result code surfaced for one-shot flush operations.
#[derive(uniffi::Enum, Debug, Copy, Clone, PartialEq, Eq)]
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

/// Raw metadata used by low-level wrapper paths.
#[derive(uniffi::Record, Debug, Copy, Clone, PartialEq, Eq)]
pub struct RawLogMeta {
    pub pid: i64,
    pub tid: i64,
    pub maintid: i64,
    pub trace_log: bool,
}

impl Default for RawLogMeta {
    fn default() -> Self {
        Self {
            pid: -1,
            tid: -1,
            maintid: -1,
            trace_log: false,
        }
    }
}

/// Configuration passed from foreign-language callers.
#[derive(uniffi::Record, Debug, Clone)]
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
    /// Compression level forwarded to the compressor.
    pub compress_level: i32,
}

/// Errors surfaced through UniFFI.
#[derive(uniffi::Error, thiserror::Error, Debug)]
pub enum XlogError {
    #[error("{details}")]
    Message { details: String },
}

/// Logger handle exposed to foreign-language callers.
#[derive(uniffi::Object)]
pub struct Logger {
    inner: core::Xlog,
}

static TRACING_INIT: OnceLock<()> = OnceLock::new();

fn to_core_level(level: LogLevel) -> core::LogLevel {
    match level {
        LogLevel::Verbose => core::LogLevel::Verbose,
        LogLevel::Debug => core::LogLevel::Debug,
        LogLevel::Info => core::LogLevel::Info,
        LogLevel::Warn => core::LogLevel::Warn,
        LogLevel::Error => core::LogLevel::Error,
        LogLevel::Fatal => core::LogLevel::Fatal,
        LogLevel::None => core::LogLevel::None,
    }
}

fn from_core_level(level: core::LogLevel) -> LogLevel {
    match level {
        core::LogLevel::Verbose => LogLevel::Verbose,
        core::LogLevel::Debug => LogLevel::Debug,
        core::LogLevel::Info => LogLevel::Info,
        core::LogLevel::Warn => LogLevel::Warn,
        core::LogLevel::Error => LogLevel::Error,
        core::LogLevel::Fatal => LogLevel::Fatal,
        core::LogLevel::None => LogLevel::None,
    }
}

fn to_core_appender_mode(mode: AppenderMode) -> core::AppenderMode {
    match mode {
        AppenderMode::Async => core::AppenderMode::Async,
        AppenderMode::Sync => core::AppenderMode::Sync,
    }
}

fn to_core_compress_mode(mode: CompressMode) -> core::CompressMode {
    match mode {
        CompressMode::Zlib => core::CompressMode::Zlib,
        CompressMode::Zstd => core::CompressMode::Zstd,
    }
}

fn to_core_config(cfg: XlogConfig) -> core::XlogConfig {
    let mut config = core::XlogConfig::new(cfg.log_dir, cfg.name_prefix)
        .cache_days(cfg.cache_days)
        .mode(to_core_appender_mode(cfg.mode))
        .compress_mode(to_core_compress_mode(cfg.compress_mode))
        .compress_level(cfg.compress_level);

    if !cfg.pub_key.is_empty() {
        config = config.pub_key(cfg.pub_key);
    }
    if !cfg.cache_dir.is_empty() {
        config = config.cache_dir(cfg.cache_dir);
    }
    config
}

fn to_core_raw_meta(meta: RawLogMeta) -> core::RawLogMeta {
    core::RawLogMeta::new(meta.pid, meta.tid, meta.maintid).with_trace_log(meta.trace_log)
}

fn from_core_file_io_action(action: core::FileIoAction) -> FileIoAction {
    match action {
        core::FileIoAction::None => FileIoAction::None,
        core::FileIoAction::Success => FileIoAction::Success,
        core::FileIoAction::Unnecessary => FileIoAction::Unnecessary,
        core::FileIoAction::OpenFailed => FileIoAction::OpenFailed,
        core::FileIoAction::ReadFailed => FileIoAction::ReadFailed,
        core::FileIoAction::WriteFailed => FileIoAction::WriteFailed,
        core::FileIoAction::CloseFailed => FileIoAction::CloseFailed,
        core::FileIoAction::RemoveFailed => FileIoAction::RemoveFailed,
    }
}

fn to_error(details: impl Into<String>) -> XlogError {
    XlogError::Message {
        details: details.into(),
    }
}

fn init_tracing(logger: core::Xlog, level: core::LogLevel) {
    let _ = TRACING_INIT.get_or_init(|| {
        let (layer, _handle) =
            core::XlogLayer::with_config(logger, core::XlogLayerConfig::new(level).enabled(true));
        let subscriber = tracing_subscriber::registry().with(layer);
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

#[uniffi::export]
impl Logger {
    /// Create a new logger instance and configure tracing.
    #[uniffi::constructor]
    pub fn new(config: XlogConfig, level: LogLevel) -> Result<Self, XlogError> {
        let cfg = to_core_config(config);
        let level = to_core_level(level);
        let logger = core::Xlog::init(cfg, level).map_err(|e| to_error(e.to_string()))?;
        logger.set_console_log_open(true);
        init_tracing(logger.clone(), level);
        info!("Initialized logger successfully");
        Ok(Self { inner: logger })
    }

    /// Return whether logs at `level` are enabled.
    pub fn is_enabled(&self, level: LogLevel) -> bool {
        self.inner.is_enabled(to_core_level(level))
    }

    /// Return the current instance log level.
    pub fn level(&self) -> LogLevel {
        from_core_level(self.inner.level())
    }

    /// Set the instance log level.
    pub fn set_level(&self, level: LogLevel) {
        self.inner.set_level(to_core_level(level));
    }

    /// Set appender mode for this instance.
    pub fn set_appender_mode(&self, mode: AppenderMode) {
        self.inner.set_appender_mode(to_core_appender_mode(mode));
    }

    /// Flush buffered logs.
    pub fn flush(&self, sync: bool) {
        self.inner.flush(sync);
    }

    /// Toggle console output for this instance.
    pub fn set_console_log_open(&self, open: bool) {
        self.inner.set_console_log_open(open);
    }

    /// Set max file size in bytes.
    pub fn set_max_file_size(&self, max_bytes: i64) {
        self.inner.set_max_file_size(max_bytes);
    }

    /// Set max alive time in seconds.
    pub fn set_max_alive_time(&self, alive_seconds: i64) {
        self.inner.set_max_alive_time(alive_seconds);
    }

    /// Log a message without file/function metadata.
    pub fn log(&self, level: LogLevel, tag: String, message: String) {
        self.inner.write(to_core_level(level), Some(&tag), &message);
    }

    /// Log a message with explicit metadata from the caller.
    pub fn log_with_meta(
        &self,
        level: LogLevel,
        tag: String,
        file: String,
        func: String,
        line: i32,
        message: String,
    ) {
        let line = if line < 0 { 0 } else { line as u32 };
        self.inner.write_with_meta(
            to_core_level(level),
            Some(&tag),
            &file,
            &func,
            line,
            &message,
        );
    }

    /// Log a message with explicit metadata and raw pid/tid/trace flags.
    pub fn log_with_raw_meta(
        &self,
        level: LogLevel,
        tag: String,
        file: String,
        func: String,
        line: i32,
        raw_meta: RawLogMeta,
        message: String,
    ) {
        let line = if line < 0 { 0 } else { line as u32 };
        self.inner.write_with_meta_raw(
            to_core_level(level),
            Some(&tag),
            &file,
            &func,
            line,
            &message,
            to_core_raw_meta(raw_meta),
        );
    }
}

/// Look up an existing logger by `name_prefix`.
#[uniffi::export]
pub fn get_logger(name_prefix: String) -> Result<Logger, XlogError> {
    core::Xlog::get(&name_prefix)
        .map(|logger| Logger { inner: logger })
        .ok_or_else(|| to_error(format!("logger not found: {name_prefix}")))
}

/// Open the global/default appender.
#[uniffi::export]
pub fn open_appender(config: XlogConfig, level: LogLevel) -> Result<(), XlogError> {
    core::Xlog::appender_open(to_core_config(config), to_core_level(level))
        .map_err(|e| to_error(e.to_string()))
}

/// Close the global/default appender.
#[uniffi::export]
pub fn close_appender() {
    core::Xlog::appender_close();
}

/// Flush all registered instances.
#[uniffi::export]
pub fn flush_all(sync: bool) {
    core::Xlog::flush_all(sync);
}

/// Write via global/default appender with raw metadata.
#[uniffi::export]
pub fn appender_write_with_raw_meta(
    level: LogLevel,
    tag: String,
    file: String,
    func: String,
    line: i32,
    raw_meta: RawLogMeta,
    message: String,
) {
    let line = if line < 0 { 0 } else { line as u32 };
    core::Xlog::appender_write_with_meta_raw(
        to_core_level(level),
        Some(&tag),
        &file,
        &func,
        line,
        &message,
        to_core_raw_meta(raw_meta),
    );
}

/// Get current global log path.
#[uniffi::export]
pub fn current_log_path() -> Option<String> {
    core::Xlog::current_log_path()
}

/// Get current global cache log path.
#[uniffi::export]
pub fn current_log_cache_path() -> Option<String> {
    core::Xlog::current_log_cache_path()
}

/// List log files from a timespan.
#[uniffi::export]
pub fn filepaths_from_timespan(timespan: i32, prefix: String) -> Vec<String> {
    core::Xlog::filepaths_from_timespan(timespan, &prefix)
}

/// Build expected log file names for a timespan.
#[uniffi::export]
pub fn make_logfile_name(timespan: i32, prefix: String) -> Vec<String> {
    core::Xlog::make_logfile_name(timespan, &prefix)
}

/// Flush once and return file I/O action.
#[uniffi::export]
pub fn oneshot_flush(config: XlogConfig) -> Result<FileIoAction, XlogError> {
    core::Xlog::oneshot_flush(to_core_config(config))
        .map(from_core_file_io_action)
        .map_err(|e| to_error(e.to_string()))
}

/// Decode a raw xlog block buffer.
#[uniffi::export]
pub fn dump(buffer: Vec<u8>) -> String {
    core::Xlog::dump(&buffer)
}

/// Decode a raw xlog block buffer from memory.
#[uniffi::export]
pub fn memory_dump(buffer: Vec<u8>) -> String {
    core::Xlog::memory_dump(&buffer)
}
