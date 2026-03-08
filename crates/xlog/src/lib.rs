//! Safe Rust wrapper for the Tencent Mars Xlog logging library.
//!
//! This crate owns the high-level API used by the platform bindings. It wraps
//! the raw FFI in `mars-xlog-sys` and provides an ergonomic `Xlog` handle plus
//! helpers for `tracing`.
//!
//! # Quick start
//! ```
//! use mars_xlog::{LogLevel, Xlog, XlogConfig};
//!
//! let cfg = XlogConfig::new("/tmp/xlog", "demo");
//! let logger = Xlog::init(cfg, LogLevel::Info).expect("init xlog");
//! logger.log(LogLevel::Info, None, "hello from rust");
//! logger.flush(true);
//! ```
//!
//! # Feature flags
//! - `macros`: `xlog!` and level helpers that capture file/module/line.
//! - `tracing`: `XlogLayer` for `tracing-subscriber`.
//! - `bench-internals`: benchmark-only profiling helpers kept out of the
//!   default release API surface.
use libc::c_int;
use std::sync::Arc;

mod backend;
#[cfg(feature = "bench-internals")]
pub mod bench;

#[cfg(feature = "tracing")]
mod tracing_layer;

#[cfg(feature = "tracing")]
pub use tracing_layer::{XlogLayer, XlogLayerConfig, XlogLayerHandle};

/// Log severity levels supported by Mars Xlog.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LogLevel {
    Verbose,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
    None,
}

/// Controls whether logs are appended asynchronously or synchronously.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum AppenderMode {
    Async,
    Sync,
}

/// Compression algorithm used for log buffers/files.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CompressMode {
    Zlib,
    Zstd,
}

/// Result code returned by `Xlog::oneshot_flush`.
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

impl From<c_int> for FileIoAction {
    fn from(value: c_int) -> Self {
        match value {
            1 => FileIoAction::Success,
            2 => FileIoAction::Unnecessary,
            3 => FileIoAction::OpenFailed,
            4 => FileIoAction::ReadFailed,
            5 => FileIoAction::WriteFailed,
            6 => FileIoAction::CloseFailed,
            7 => FileIoAction::RemoveFailed,
            _ => FileIoAction::None,
        }
    }
}

/// Raw metadata carried by low-level wrappers (JNI/FFI parity path).
///
/// Semantics match Mars `XLoggerInfo`:
/// - `pid/tid/maintid = -1` means "let backend fill runtime value".
/// - `trace_log = true` enables Android console bypass behavior.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
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

impl RawLogMeta {
    /// Build explicit pid/tid/maintid metadata.
    pub const fn new(pid: i64, tid: i64, maintid: i64) -> Self {
        Self {
            pid,
            tid,
            maintid,
            trace_log: false,
        }
    }

    /// Enable Android `traceLog` console bypass for this entry.
    pub const fn with_trace_log(mut self, trace_log: bool) -> Self {
        self.trace_log = trace_log;
        self
    }
}

/// Errors returned by Xlog initialization helpers.
#[derive(Debug, thiserror::Error)]
pub enum XlogError {
    #[error("log_dir and name_prefix must be non-empty")]
    InvalidConfig,
    #[error("xlog initialization failed")]
    InitFailed,
}

/// Configuration used to create an Xlog instance or open the global appender.
#[derive(Debug, Clone)]
pub struct XlogConfig {
    /// Directory for log files. Must be non-empty.
    pub log_dir: String,
    /// Prefix for log file names and the instance name. Must be non-empty.
    pub name_prefix: String,
    /// Optional public key (hex string, 128 chars) enabling log encryption.
    pub pub_key: Option<String>,
    /// Optional cache directory for mmap buffers and temporary logs.
    pub cache_dir: Option<String>,
    /// Days to keep cached logs before moving them to `log_dir`.
    pub cache_days: i32,
    /// Appender mode (async or sync).
    pub mode: AppenderMode,
    /// Compression algorithm for log buffers/files.
    pub compress_mode: CompressMode,
    /// Compression level forwarded to the compressor.
    pub compress_level: i32,
}

impl XlogConfig {
    /// Create a config with required fields and sensible defaults.
    pub fn new(log_dir: impl Into<String>, name_prefix: impl Into<String>) -> Self {
        Self {
            log_dir: log_dir.into(),
            name_prefix: name_prefix.into(),
            pub_key: None,
            cache_dir: None,
            cache_days: 0,
            mode: AppenderMode::Async,
            compress_mode: CompressMode::Zlib,
            compress_level: 6,
        }
    }

    /// Set the public key used to encrypt logs.
    pub fn pub_key(mut self, key: impl Into<String>) -> Self {
        self.pub_key = Some(key.into());
        self
    }

    /// Set the optional cache directory for mmap buffers and temp files.
    pub fn cache_dir(mut self, dir: impl Into<String>) -> Self {
        self.cache_dir = Some(dir.into());
        self
    }

    /// Set the number of days to keep cached logs before moving them.
    pub fn cache_days(mut self, days: i32) -> Self {
        self.cache_days = days;
        self
    }

    /// Set the appender mode.
    pub fn mode(mut self, mode: AppenderMode) -> Self {
        self.mode = mode;
        self
    }

    /// Set the compression algorithm.
    pub fn compress_mode(mut self, mode: CompressMode) -> Self {
        self.compress_mode = mode;
        self
    }

    /// Set the compression level forwarded to the compressor.
    pub fn compress_level(mut self, level: i32) -> Self {
        self.compress_level = level;
        self
    }
}

/// Handle to a Mars Xlog instance.
///
/// Cloning the handle is cheap; the underlying instance is reference-counted
/// and released when the last handle is dropped.
#[derive(Clone)]
pub struct Xlog {
    inner: Arc<Inner>,
}

struct Inner {
    backend: Arc<dyn backend::XlogBackend>,
    name_prefix: String,
}

impl Xlog {
    /// Initialize a new Xlog instance (recommended entrypoint).
    pub fn init(config: XlogConfig, level: LogLevel) -> Result<Self, XlogError> {
        Self::new(config, level)
    }

    #[doc(hidden)]
    pub fn new(config: XlogConfig, level: LogLevel) -> Result<Self, XlogError> {
        let backend = backend::provider().new_instance(&config, level)?;
        Ok(Self {
            inner: Arc::new(Inner {
                backend,
                name_prefix: config.name_prefix,
            }),
        })
    }

    /// Look up an existing instance by name prefix.
    pub fn get(name_prefix: &str) -> Option<Self> {
        let backend = backend::provider().get_instance(name_prefix)?;
        Some(Self {
            inner: Arc::new(Inner {
                backend,
                name_prefix: name_prefix.to_string(),
            }),
        })
    }

    #[doc(hidden)]
    pub fn appender_open(config: XlogConfig, level: LogLevel) -> Result<(), XlogError> {
        backend::provider().appender_open(&config, level)
    }

    #[doc(hidden)]
    pub fn appender_close() {
        backend::provider().appender_close();
    }

    #[doc(hidden)]
    pub fn flush_all(sync: bool) {
        backend::provider().flush_all(sync);
    }

    #[cfg(any(
        target_os = "ios",
        target_os = "macos",
        target_os = "tvos",
        target_os = "watchos"
    ))]
    #[doc(hidden)]
    pub fn set_console_fun(fun: ConsoleFun) {
        backend::provider().set_console_fun(fun);
    }

    /// Returns the raw instance handle used by the underlying C++ library.
    pub fn instance(&self) -> usize {
        self.inner.backend.instance()
    }

    /// Returns `true` if logs at `level` are enabled for this instance.
    pub fn is_enabled(&self, level: LogLevel) -> bool {
        self.inner.backend.is_enabled(level)
    }

    /// Get the current log level for this instance.
    pub fn level(&self) -> LogLevel {
        self.inner.backend.level()
    }

    /// Set the minimum log level for this instance.
    pub fn set_level(&self, level: LogLevel) {
        self.inner.backend.set_level(level);
    }

    /// Switch between async and sync appender modes.
    pub fn set_appender_mode(&self, mode: AppenderMode) {
        self.inner.backend.set_appender_mode(mode);
    }

    /// Flush buffered logs for this instance.
    pub fn flush(&self, sync: bool) {
        self.inner.backend.flush(sync);
    }

    /// Enable or disable console logging (platform dependent).
    pub fn set_console_log_open(&self, open: bool) {
        self.inner.backend.set_console_log_open(open);
    }

    /// Set the max log file size in bytes (0 disables splitting).
    pub fn set_max_file_size(&self, max_bytes: i64) {
        self.inner.backend.set_max_file_size(max_bytes);
    }

    /// Set the max log file age in seconds before deletion/rotation.
    pub fn set_max_alive_time(&self, alive_seconds: i64) {
        self.inner.backend.set_max_alive_time(alive_seconds);
    }

    /// Log a message with caller file/line captured via `#[track_caller]`.
    ///
    /// Note: function name is not available here; use `xlog!` macro or
    /// `write_with_meta` when you need full metadata.
    #[track_caller]
    pub fn log(&self, level: LogLevel, tag: Option<&str>, msg: impl AsRef<str>) {
        if !self.is_enabled(level) {
            return;
        }
        let loc = std::panic::Location::caller();
        self.write_with_meta(level, tag, loc.file(), "", loc.line(), msg.as_ref());
    }

    /// Compatibility wrapper for older APIs. Prefer `log` or the macros.
    #[track_caller]
    pub fn write(&self, level: LogLevel, tag: Option<&str>, msg: &str) {
        if !self.is_enabled(level) {
            return;
        }
        self.write_with_meta(level, tag, "", "", 0, msg);
    }

    /// Log with explicit metadata (file, function, line).
    ///
    /// Use this when callers already provide metadata (for example from JNI).
    pub fn write_with_meta(
        &self,
        level: LogLevel,
        tag: Option<&str>,
        file: &str,
        func: &str,
        line: u32,
        msg: &str,
    ) {
        self.write_with_meta_raw(level, tag, file, func, line, msg, RawLogMeta::default());
    }

    /// Log with explicit metadata and raw pid/tid/trace flags.
    ///
    /// This is mainly for low-level platform wrappers that already own thread
    /// metadata (for example JNI side thread ids).
    pub fn write_with_meta_raw(
        &self,
        level: LogLevel,
        tag: Option<&str>,
        file: &str,
        func: &str,
        line: u32,
        msg: &str,
        raw_meta: RawLogMeta,
    ) {
        if !self.is_enabled(level) {
            return;
        }
        self.inner.backend.write_with_meta(
            level,
            tag.unwrap_or(&self.inner.name_prefix),
            file,
            func,
            line,
            msg,
            raw_meta,
        );
    }

    /// Write via the global/default appender with raw metadata.
    ///
    /// This mirrors the C++ `XloggerWrite(instance_ptr == 0, ...)` path.
    #[doc(hidden)]
    pub fn appender_write_with_meta_raw(
        level: LogLevel,
        tag: Option<&str>,
        file: &str,
        func: &str,
        line: u32,
        msg: &str,
        raw_meta: RawLogMeta,
    ) {
        if !backend::provider().global_is_enabled(level) {
            return;
        }
        backend::provider().write_global_with_meta(
            level,
            tag.unwrap_or(""),
            file,
            func,
            line,
            msg,
            raw_meta,
        );
    }

    #[doc(hidden)]
    pub fn current_log_path() -> Option<String> {
        backend::provider().current_log_path()
    }

    #[doc(hidden)]
    pub fn current_log_cache_path() -> Option<String> {
        backend::provider().current_log_cache_path()
    }

    #[doc(hidden)]
    pub fn filepaths_from_timespan(timespan: i32, prefix: &str) -> Vec<String> {
        backend::provider().filepaths_from_timespan(timespan, prefix)
    }

    #[doc(hidden)]
    pub fn make_logfile_name(timespan: i32, prefix: &str) -> Vec<String> {
        backend::provider().make_logfile_name(timespan, prefix)
    }

    #[doc(hidden)]
    pub fn oneshot_flush(config: XlogConfig) -> Result<FileIoAction, XlogError> {
        backend::provider().oneshot_flush(&config)
    }

    #[doc(hidden)]
    pub fn dump(buffer: &[u8]) -> String {
        backend::provider().dump(buffer)
    }

    #[doc(hidden)]
    pub fn memory_dump(buffer: &[u8]) -> String {
        backend::provider().memory_dump(buffer)
    }
}

#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos"
))]
#[doc(hidden)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ConsoleFun {
    Printf = 0,
    NSLog = 1,
    OSLog = 2,
}

/// Log with explicit metadata captured by the macro call site.
#[cfg(feature = "macros")]
#[macro_export]
macro_rules! xlog {
    ($logger:expr, $level:expr, $tag:expr, $($arg:tt)+) => {{
        let logger_ref = $logger;
        let level = $level;
        if logger_ref.is_enabled(level) {
            let msg = format!($($arg)+);
            logger_ref.write_with_meta(level, Some($tag), file!(), module_path!(), line!(), &msg);
        }
    }};
}

/// Convenience macro for `LogLevel::Debug`.
#[cfg(feature = "macros")]
#[macro_export]
macro_rules! xlog_debug {
    ($logger:expr, $tag:expr, $($arg:tt)+) => {{
        $crate::xlog!($logger, $crate::LogLevel::Debug, $tag, $($arg)+)
    }};
}

/// Convenience macro for `LogLevel::Info`.
#[cfg(feature = "macros")]
#[macro_export]
macro_rules! xlog_info {
    ($logger:expr, $tag:expr, $($arg:tt)+) => {{
        $crate::xlog!($logger, $crate::LogLevel::Info, $tag, $($arg)+)
    }};
}

/// Convenience macro for `LogLevel::Warn`.
#[cfg(feature = "macros")]
#[macro_export]
macro_rules! xlog_warn {
    ($logger:expr, $tag:expr, $($arg:tt)+) => {{
        $crate::xlog!($logger, $crate::LogLevel::Warn, $tag, $($arg)+)
    }};
}

/// Convenience macro for `LogLevel::Error`.
#[cfg(feature = "macros")]
#[macro_export]
macro_rules! xlog_error {
    ($logger:expr, $tag:expr, $($arg:tt)+) => {{
        $crate::xlog!($logger, $crate::LogLevel::Error, $tag, $($arg)+)
    }};
}
