//! Safe Rust wrapper for the Tencent Mars Xlog logging library.
//!
//! This crate owns the high-level Rust API used by platform bindings and
//! direct Rust integrations. The default release surface is pure Rust and
//! built on top of `mars-xlog-core`; optional metrics hooks stay feature-gated
//! and out of the default public API.
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
//! - `metrics`: emits structured runtime metrics via the `metrics` crate.
use libc::c_int;
use std::sync::Arc;

mod backend;
mod event;
mod layout;
#[cfg(feature = "tracing")]
mod tracing_layer;

pub use event::Event;
pub use layout::{Layout, Piece, WriteLine};

#[cfg(feature = "tracing")]
pub use tracing_layer::{XlogLayer, XlogLayerConfig, XlogLayerHandle};

/// Log severity levels supported by Mars Xlog.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LogLevel {
    /// Verbose diagnostic output.
    Verbose,
    /// Debug output for development and troubleshooting.
    Debug,
    /// Informational output for normal events.
    Info,
    /// Warning output for recoverable issues.
    Warn,
    /// Error output for failures that do not immediately abort the process.
    Error,
    /// Fatal output for unrecoverable failures.
    Fatal,
    /// Logging disabled.
    None,
}

#[cfg(feature = "rust-backend")]
impl From<LogLevel> for mars_xlog_core::record::LogLevel {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Verbose => Self::Verbose,
            LogLevel::Debug => Self::Debug,
            LogLevel::Info => Self::Info,
            LogLevel::Warn => Self::Warn,
            LogLevel::Error => Self::Error,
            LogLevel::Fatal => Self::Fatal,
            LogLevel::None => Self::None,
        }
    }
}

#[cfg(feature = "rust-backend")]
impl From<mars_xlog_core::record::LogLevel> for LogLevel {
    fn from(level: mars_xlog_core::record::LogLevel) -> Self {
        match level {
            mars_xlog_core::record::LogLevel::Verbose => Self::Verbose,
            mars_xlog_core::record::LogLevel::Debug => Self::Debug,
            mars_xlog_core::record::LogLevel::Info => Self::Info,
            mars_xlog_core::record::LogLevel::Warn => Self::Warn,
            mars_xlog_core::record::LogLevel::Error => Self::Error,
            mars_xlog_core::record::LogLevel::Fatal => Self::Fatal,
            mars_xlog_core::record::LogLevel::None => Self::None,
        }
    }
}

/// Controls whether logs are appended asynchronously or synchronously.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum AppenderMode {
    /// Queue writes and persist them from the async worker path.
    Async,
    /// Write through the sync path owned by the caller.
    Sync,
}

/// Compression algorithm used for log buffers/files.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CompressMode {
    /// Use zlib framing compatible with the historical xlog format.
    Zlib,
    /// Use zstd framing supported by the Rust implementation.
    Zstd,
}

/// Result code returned by `Xlog::oneshot_flush`.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum FileIoAction {
    /// No file action was taken.
    None,
    /// The requested file action completed successfully.
    Success,
    /// The requested file action was not needed.
    Unnecessary,
    /// Opening the source or destination file failed.
    OpenFailed,
    /// Reading a source file failed.
    ReadFailed,
    /// Writing a destination file failed.
    WriteFailed,
    /// Closing a file handle failed.
    CloseFailed,
    /// Removing a source file failed.
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
    /// Process id override. Use `-1` to let the backend fill the runtime pid.
    pub pid: i64,
    /// Thread id override. Use `-1` to let the backend fill the runtime tid.
    pub tid: i64,
    /// Main thread id override. Use `-1` to let the backend fill the runtime value.
    pub maintid: i64,
    /// Whether Android `traceLog` console bypass behavior should be enabled.
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
    /// Required config fields such as `log_dir` or `name_prefix` were empty.
    InvalidConfig,
    #[error("logger `{name_prefix}` is already initialized with a different config")]
    /// The requested `name_prefix` already exists but with a different config.
    ConfigConflict {
        /// Name prefix of the already-initialized logger instance.
        name_prefix: String,
    },
    #[error("xlog initialization failed")]
    /// Backend initialization failed.
    InitFailed,
    #[error("cache_days must be non-negative")]
    /// `cache_days` was negative.
    InvalidCacheDays,
    #[error("compress level {level} is outside the range accepted by {mode:?}")]
    /// `compress_level` is outside the selected compressor's range.
    InvalidCompressLevel {
        /// Rejected level.
        level: i32,
        /// Compressor that rejected it.
        mode: CompressMode,
    },
    #[error("max alive time {alive_seconds}s is below the minimum {minimum}s")]
    /// Retention was below the Mars floor of one day.
    AliveTimeTooSmall {
        /// Requested retention, in seconds.
        alive_seconds: i64,
        /// Minimum retention the engine accepts, in seconds.
        minimum: i64,
    },
}

/// Configuration used to create an Xlog instance or open the global appender.
///
/// A given `(name_prefix, log_dir/cache_dir)` namespace is single-writer only.
/// Initialization enforces this with `<name_prefix>.lock` files in each
/// storage directory involved in the instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XlogConfig {
    /// Directory for log files. Must be non-empty.
    pub log_dir: String,
    /// Prefix for log file names and the instance name. Must be non-empty.
    pub name_prefix: String,
    /// Optional public key (hex string, 128 chars) enabling log encryption.
    pub pub_key: Option<String>,
    /// Optional cache directory for mmap buffers and temporary logs.
    ///
    /// When set, it participates in the same single-writer lock namespace as
    /// `log_dir`.
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
    inner: Arc<backend::RustBackend>,
}

impl Xlog {
    /// Initialize or reuse a named Xlog instance (recommended entrypoint).
    ///
    /// Behavior is idempotent by `name_prefix`:
    /// - If no live instance exists for `name_prefix`, a new instance is created.
    /// - If a live instance exists with the same config, it is reused.
    /// - If a live instance exists with a different config, returns
    ///   [`XlogError::ConfigConflict`].
    ///
    /// The line layout is Mars. A reused instance keeps the layout from the
    /// first successful init. The level is updated on every successful init.
    pub fn init(config: XlogConfig, level: LogLevel) -> Result<Self, XlogError> {
        Self::init_with_layout(config, level, Layout::mars())
    }

    /// Initialize or reuse a named instance with an explicit line layout.
    ///
    /// Config equality does not include the layout. When the name is already
    /// live with the same config, `layout` is dropped and the original layout
    /// stays in place.
    pub fn init_with_layout(
        config: XlogConfig,
        level: LogLevel,
        layout: Layout,
    ) -> Result<Self, XlogError> {
        let inner = backend::provider().new_instance_with_layout(&config, level, layout)?;
        Ok(Self { inner })
    }

    #[doc(hidden)]
    pub fn new(config: XlogConfig, level: LogLevel) -> Result<Self, XlogError> {
        Self::init(config, level)
    }

    /// Look up an existing instance by name prefix.
    pub fn get(name_prefix: &str) -> Option<Self> {
        let inner = backend::provider().get_instance(name_prefix)?;
        Some(Self { inner })
    }

    #[doc(hidden)]
    /// Open the global/default appender.
    ///
    /// If already open with a different config, returns
    /// [`XlogError::ConfigConflict`].
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

    /// Returns the process-local instance id.
    #[doc(hidden)]
    pub fn instance(&self) -> usize {
        self.inner.instance()
    }

    /// Returns `true` if logs at `level` are enabled for this instance.
    pub fn is_enabled(&self, level: LogLevel) -> bool {
        self.inner.is_enabled(level)
    }

    /// Get the current log level for this instance.
    pub fn level(&self) -> LogLevel {
        self.inner.level()
    }

    /// Set the minimum log level for this instance.
    pub fn set_level(&self, level: LogLevel) {
        self.inner.set_level(level);
    }

    /// Switch between async and sync appender modes.
    pub fn set_appender_mode(&self, mode: AppenderMode) {
        self.inner.set_appender_mode(mode);
    }

    /// Flush buffered logs for this instance.
    pub fn flush(&self, sync: bool) {
        self.inner.flush(sync);
    }

    /// Enable or disable console logging for this instance (platform dependent).
    pub fn set_console_log_open(&self, open: bool) {
        self.inner.set_console_log_open(open);
    }

    /// Set the max log file size in bytes for this instance (0 disables splitting).
    pub fn set_max_file_size(&self, max_bytes: i64) {
        self.inner.set_max_file_size(max_bytes);
    }

    /// Set the max log file age in seconds for this instance before deletion/rotation.
    pub fn set_max_alive_time(&self, alive_seconds: i64) -> Result<(), XlogError> {
        self.inner.set_max_alive_time(alive_seconds)
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

    /// Log an already-built [`Event`].
    ///
    /// The event's timestamp is kept. `trace_log` is not set. Caller strings
    /// only need to live until this call returns.
    pub fn write_event(&self, event: &Event<'_>) {
        if !self.is_enabled(event.level) {
            return;
        }
        self.inner.emit(event, false);
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
    #[allow(clippy::too_many_arguments)]
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
        self.inner
            .emit_category(level, tag, file, func, line, msg, raw_meta);
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
    /// Forward console output through `printf`.
    Printf = 0,
    /// Forward console output through `NSLog`.
    NSLog = 1,
    /// Forward console output through `os_log`.
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};

    use mars_xlog_core::compress::decompress_raw_zlib;
    use mars_xlog_core::protocol::{
        AppendMode, LogHeader, HEADER_LEN, MAGIC_END, MAGIC_SYNC_NO_CRYPT_ZLIB_START, TAILER_LEN,
    };
    use tempfile::TempDir;

    use super::{
        AppenderMode, CompressMode, Event, Layout, LogLevel, Piece, RawLogMeta, WriteLine, Xlog,
        XlogConfig, XlogError,
    };

    static NEXT_PREFIX_ID: AtomicUsize = AtomicUsize::new(1);
    static APPENDER_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn unique_prefix(label: &str) -> String {
        let id = NEXT_PREFIX_ID.fetch_add(1, Ordering::Relaxed);
        format!("{label}-{}-{id}", std::process::id())
    }

    fn appender_test_lock() -> &'static Mutex<()> {
        APPENDER_TEST_LOCK.get_or_init(|| Mutex::new(()))
    }

    struct AppenderCloseGuard;

    impl Drop for AppenderCloseGuard {
        fn drop(&mut self) {
            Xlog::appender_close();
        }
    }

    #[test]
    fn init_reuses_same_name_prefix_and_applies_latest_level() {
        let dir = TempDir::new().expect("tempdir");
        let prefix = unique_prefix("reuse");
        let cfg = XlogConfig::new(dir.path().display().to_string(), &prefix);

        let first = Xlog::init(cfg.clone(), LogLevel::Info).expect("init first");
        let second = Xlog::init(cfg, LogLevel::Debug).expect("init second");

        assert_eq!(first.instance(), second.instance());
        assert_eq!(first.level(), LogLevel::Debug);
    }

    #[test]
    fn init_rejects_conflicting_config_for_same_name_prefix() {
        let dir = TempDir::new().expect("tempdir");
        let prefix = unique_prefix("conflict");
        let cfg = XlogConfig::new(dir.path().display().to_string(), &prefix);
        let _first = Xlog::init(cfg.clone(), LogLevel::Info).expect("init first");

        let conflict_cfg = cfg.compress_mode(CompressMode::Zstd);
        let err = match Xlog::init(conflict_cfg, LogLevel::Info) {
            Ok(_) => panic!("must reject conflict"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            XlogError::ConfigConflict { ref name_prefix } if name_prefix == &prefix
        ));
    }

    #[test]
    fn appender_open_rejects_conflicting_config_when_default_exists() {
        let _lock = appender_test_lock().lock().expect("lock poisoned");
        let _guard = AppenderCloseGuard;
        Xlog::appender_close();

        let dir1 = TempDir::new().expect("tempdir1");
        let dir2 = TempDir::new().expect("tempdir2");
        let cfg1 = XlogConfig::new(dir1.path().display().to_string(), unique_prefix("global-a"));
        let cfg2 = XlogConfig::new(dir2.path().display().to_string(), unique_prefix("global-b"));

        Xlog::appender_open(cfg1, LogLevel::Info).expect("open first");
        let err = Xlog::appender_open(cfg2, LogLevel::Info).expect_err("must reject conflict");
        assert!(matches!(err, XlogError::ConfigConflict { .. }));
    }

    fn decoded_log_text(dir: &Path) -> String {
        let mut files: Vec<_> = fs::read_dir(dir)
            .expect("read log dir")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("xlog"))
            .collect();
        files.sort();
        assert!(
            !files.is_empty(),
            "expected an xlog file in {}",
            dir.display()
        );

        let mut merged = String::new();
        for file in files {
            let bytes = fs::read(file).expect("read xlog");
            let mut offset = 0usize;
            while offset + HEADER_LEN + TAILER_LEN <= bytes.len() {
                let Ok(header) = LogHeader::decode(&bytes[offset..offset + HEADER_LEN]) else {
                    break;
                };
                let payload_len = header.len as usize;
                let start = offset + HEADER_LEN;
                let end = start + payload_len;
                if end + TAILER_LEN > bytes.len() || bytes[end] != MAGIC_END {
                    break;
                }
                let payload = &bytes[start..end];
                let plain = if header.magic.mode() == AppendMode::Async {
                    decompress_raw_zlib(payload).expect("zlib payload")
                } else {
                    payload.to_vec()
                };
                merged.push_str(std::str::from_utf8(&plain).expect("utf-8 plaintext"));
                offset = end + TAILER_LEN;
            }
        }
        merged
    }

    fn first_sync_payload(dir: &Path) -> (u8, String) {
        let mut files: Vec<_> = fs::read_dir(dir)
            .expect("read log dir")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("xlog"))
            .collect();
        files.sort();
        assert!(!files.is_empty(), "expected an xlog file");
        let bytes = fs::read(&files[0]).expect("read xlog");
        let header = LogHeader::decode(&bytes[..HEADER_LEN]).expect("header");
        let payload_len = header.len as usize;
        let start = HEADER_LEN;
        let end = start + payload_len;
        assert!(end < bytes.len(), "payload fits before end byte");
        assert_eq!(bytes[end], MAGIC_END);
        let text = String::from_utf8(bytes[start..end].to_vec()).expect("sync plaintext");
        (u8::from(header.magic), text)
    }

    fn sync_config(dir: &Path, label: &str) -> XlogConfig {
        XlogConfig::new(dir.display().to_string(), unique_prefix(label)).mode(AppenderMode::Sync)
    }

    #[test]
    fn default_init_sync_payload_is_mars_line() {
        let dir = TempDir::new().expect("tempdir");
        let logger =
            Xlog::init(sync_config(dir.path(), "mars-default"), LogLevel::Info).expect("init");
        logger.log(LogLevel::Info, Some("tag"), "hello-mars");
        logger.flush(true);

        let (magic, text) = first_sync_payload(dir.path());
        assert_eq!(magic, MAGIC_SYNC_NO_CRYPT_ZLIB_START);
        assert!(text.starts_with("[I]["), "{text}");
        assert!(text.contains("hello-mars"));
    }

    #[test]
    fn pieces_layout_writes_message_only() {
        let dir = TempDir::new().expect("tempdir");
        let logger = Xlog::init_with_layout(
            sync_config(dir.path(), "pieces"),
            LogLevel::Info,
            Layout::pieces([Piece::Message, Piece::Literal("\n")]),
        )
        .expect("init");
        logger.log(LogLevel::Info, Some("tag"), "hello");
        logger.flush(true);

        let (magic, text) = first_sync_payload(dir.path());
        assert_eq!(magic, MAGIC_SYNC_NO_CRYPT_ZLIB_START);
        assert_eq!(text, "hello\n");
    }

    struct MessageOnly;

    impl WriteLine for MessageOnly {
        fn write_line(&self, out: &mut String, event: &Event<'_>) {
            out.push_str(event.message);
        }
    }

    #[test]
    fn custom_layout_replaces_plaintext_only() {
        let dir = TempDir::new().expect("tempdir");
        let logger = Xlog::init_with_layout(
            sync_config(dir.path(), "custom"),
            LogLevel::Info,
            Layout::custom(Arc::new(MessageOnly)),
        )
        .expect("init");
        logger.write_event(&Event::new(LogLevel::Info, "tag", "custom-body"));
        logger.flush(true);

        let (magic, text) = first_sync_payload(dir.path());
        assert_eq!(magic, MAGIC_SYNC_NO_CRYPT_ZLIB_START);
        assert_eq!(text, "custom-body");
    }

    #[test]
    fn reused_init_keeps_the_first_layout() {
        let dir = TempDir::new().expect("tempdir");
        let cfg = sync_config(dir.path(), "layout-reuse");
        let first = Xlog::init_with_layout(
            cfg.clone(),
            LogLevel::Info,
            Layout::pieces([Piece::Message, Piece::Literal("\n")]),
        )
        .expect("first");
        let _second =
            Xlog::init_with_layout(cfg, LogLevel::Debug, Layout::custom(Arc::new(MessageOnly)))
                .expect("reuse");
        assert_eq!(first.level(), LogLevel::Debug);
        first.log(LogLevel::Info, Some("tag"), "kept");
        first.flush(true);

        let (_magic, text) = first_sync_payload(dir.path());
        assert_eq!(text, "kept\n");
    }

    #[test]
    fn write_event_drops_caller_string_before_flush() {
        let sync_dir = TempDir::new().expect("sync dir");
        let sync_logger = Xlog::init(sync_config(sync_dir.path(), "borrow-sync"), LogLevel::Info)
            .expect("sync init");
        {
            let message = String::from("borrow-sync-unique");
            let event = Event::new(LogLevel::Info, "tag", &message);
            sync_logger.write_event(&event);
        }
        sync_logger.flush(true);
        let (_magic, text) = first_sync_payload(sync_dir.path());
        assert!(text.contains("borrow-sync-unique"), "{text}");

        let async_dir = TempDir::new().expect("async dir");
        let async_cfg = XlogConfig::new(
            async_dir.path().display().to_string(),
            unique_prefix("borrow-async"),
        );
        let async_logger = Xlog::init(async_cfg, LogLevel::Info).expect("async init");
        {
            let message = String::from("borrow-async-unique");
            let event = Event::new(LogLevel::Info, "tag", &message);
            async_logger.write_event(&event);
        }
        async_logger.flush(true);
        let text = decoded_log_text(async_dir.path());
        assert!(text.contains("borrow-async-unique"), "{text}");
    }

    #[test]
    fn appender_write_uses_global_meta_rule() {
        let _lock = appender_test_lock().lock().expect("lock poisoned");
        let _guard = AppenderCloseGuard;
        Xlog::appender_close();

        let dir = TempDir::new().expect("tempdir");
        let cfg = XlogConfig::new(
            dir.path().display().to_string(),
            unique_prefix("global-meta"),
        )
        .mode(AppenderMode::Sync);
        Xlog::appender_open(cfg, LogLevel::Info).expect("open");
        Xlog::appender_write_with_meta_raw(
            LogLevel::Info,
            Some("tag"),
            "f.rs",
            "func",
            1,
            "global-body",
            RawLogMeta::new(321, -1, -1),
        );
        Xlog::flush_all(true);

        let (magic, text) = first_sync_payload(dir.path());
        assert_eq!(magic, MAGIC_SYNC_NO_CRYPT_ZLIB_START);
        let tid = mars_xlog_core::platform_tid::current_tid();
        let main = mars_xlog_core::platform_tid::main_tid();
        let suffix = if tid == main { "*" } else { "" };
        assert!(text.contains(&format!("[321, {tid}{suffix}]")), "{text}");
    }
}
