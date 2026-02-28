use std::sync::Arc;

use crate::{AppenderMode, FileIoAction, LogLevel, XlogConfig, XlogError};

#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos"
))]
use crate::ConsoleFun;

#[cfg(feature = "ffi-backend")]
mod ffi;

#[cfg(feature = "rust-backend")]
mod rust;

pub(crate) trait XlogBackend: Send + Sync {
    fn instance(&self) -> usize;
    fn is_enabled(&self, level: LogLevel) -> bool;
    fn level(&self) -> LogLevel;
    fn set_level(&self, level: LogLevel);
    fn set_appender_mode(&self, mode: AppenderMode);
    fn flush(&self, sync: bool);
    fn set_console_log_open(&self, open: bool);
    fn set_max_file_size(&self, max_bytes: i64);
    fn set_max_alive_time(&self, alive_seconds: i64);
    fn write_with_meta(
        &self,
        level: LogLevel,
        tag: &str,
        file: &str,
        func: &str,
        line: u32,
        msg: &str,
    );
}

pub(crate) trait XlogBackendProvider: Send + Sync {
    fn new_instance(
        &self,
        config: &XlogConfig,
        level: LogLevel,
    ) -> Result<Arc<dyn XlogBackend>, XlogError>;

    fn get_instance(&self, name_prefix: &str) -> Option<Arc<dyn XlogBackend>>;

    fn appender_open(&self, config: &XlogConfig, level: LogLevel) -> Result<(), XlogError>;
    fn appender_close(&self);
    fn flush_all(&self, sync: bool);

    #[cfg(any(
        target_os = "ios",
        target_os = "macos",
        target_os = "tvos",
        target_os = "watchos"
    ))]
    fn set_console_fun(&self, fun: ConsoleFun);

    fn current_log_path(&self) -> Option<String>;
    fn current_log_cache_path(&self) -> Option<String>;
    fn filepaths_from_timespan(&self, timespan: i32, prefix: &str) -> Vec<String>;
    fn make_logfile_name(&self, timespan: i32, prefix: &str) -> Vec<String>;
    fn oneshot_flush(&self, config: &XlogConfig) -> Result<FileIoAction, XlogError>;
    fn dump(&self, buffer: &[u8]) -> String;
    fn memory_dump(&self, buffer: &[u8]) -> String;
}

pub(crate) fn provider() -> &'static dyn XlogBackendProvider {
    #[cfg(feature = "ffi-backend")]
    {
        // Phase 1: 所有调用先落到 FFI backend，保证行为不变。
        ffi::provider()
    }

    #[cfg(not(feature = "ffi-backend"))]
    {
        compile_error!("xlog requires at least one backend; enable the `ffi-backend` feature");
    }
}
