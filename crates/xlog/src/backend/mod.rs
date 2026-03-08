use std::sync::Arc;

use crate::{AppenderMode, FileIoAction, LogLevel, RawLogMeta, XlogConfig, XlogError};

#[cfg(all(feature = "rust-backend", feature = "cpp-backend"))]
compile_error!("enable only one backend feature at a time");

#[cfg(not(any(feature = "rust-backend", feature = "cpp-backend")))]
compile_error!("xlog requires either `rust-backend` or `cpp-backend`");

#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos"
))]
use crate::ConsoleFun;

#[cfg(feature = "cpp-backend")]
mod cpp;
#[cfg(feature = "rust-backend")]
mod rust;
#[cfg(feature = "rust-backend")]
mod stage_profile;

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
        raw_meta: RawLogMeta,
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
    fn global_is_enabled(&self, level: LogLevel) -> bool;
    fn write_global_with_meta(
        &self,
        level: LogLevel,
        tag: &str,
        file: &str,
        func: &str,
        line: u32,
        msg: &str,
        raw_meta: RawLogMeta,
    );

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
    #[cfg(feature = "rust-backend")]
    {
        return rust::provider();
    }

    #[cfg(feature = "cpp-backend")]
    {
        return cpp::provider();
    }
}

#[cfg(all(feature = "rust-backend", feature = "bench-internals"))]
pub(crate) fn set_rust_sync_stage_profile_enabled(enabled: bool) {
    rust::set_sync_stage_profile_enabled(enabled);
}

#[cfg(all(feature = "rust-backend", feature = "bench-internals"))]
pub(crate) fn set_rust_async_stage_profile_enabled(enabled: bool) {
    rust::set_async_stage_profile_enabled(enabled);
}

#[cfg(all(feature = "rust-backend", feature = "bench-internals"))]
pub(crate) fn mark_rust_async_flush_hint_flush_every() {
    rust::mark_async_flush_hint_flush_every();
}

#[cfg(all(feature = "rust-backend", feature = "bench-internals"))]
pub(crate) fn take_rust_sync_stage_stats() -> Option<crate::bench::RustSyncStageStats> {
    rust::take_sync_stage_stats()
}

#[cfg(all(feature = "rust-backend", feature = "bench-internals"))]
pub(crate) fn take_rust_async_stage_stats() -> Option<crate::bench::RustAsyncStageStats> {
    rust::take_async_stage_stats()
}
