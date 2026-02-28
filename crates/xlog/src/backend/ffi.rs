use std::ffi::CString;
use std::ptr;
use std::sync::Arc;

use libc::{c_int, gettimeofday, timeval};
use mars_xlog_sys as sys;

use super::{XlogBackend, XlogBackendProvider};
use crate::{AppenderMode, FileIoAction, LogLevel, XlogConfig, XlogError};

#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos"
))]
use crate::ConsoleFun;

pub(super) fn provider() -> &'static dyn XlogBackendProvider {
    static PROVIDER: FfiBackendProvider = FfiBackendProvider;
    &PROVIDER
}

struct FfiBackendProvider;

struct FfiBackend {
    instance: usize,
    name_prefix: String,
}

impl Drop for FfiBackend {
    fn drop(&mut self) {
        let name = CString::new(self.name_prefix.clone())
            .unwrap_or_else(|_| CString::new("xlog").unwrap());
        unsafe {
            sys::mars_xlog_release_instance(name.as_ptr());
        }
    }
}

impl XlogBackendProvider for FfiBackendProvider {
    fn new_instance(
        &self,
        config: &XlogConfig,
        level: LogLevel,
    ) -> Result<Arc<dyn XlogBackend>, XlogError> {
        if config.log_dir.is_empty() || config.name_prefix.is_empty() {
            return Err(XlogError::InvalidConfig);
        }

        let (cfg, _cstr) = config.to_sys();
        let instance = unsafe { sys::mars_xlog_new_instance(&cfg, level.as_sys() as c_int) };
        if instance == 0 {
            return Err(XlogError::InitFailed);
        }

        Ok(Arc::new(FfiBackend {
            instance,
            name_prefix: config.name_prefix.clone(),
        }))
    }

    fn get_instance(&self, name_prefix: &str) -> Option<Arc<dyn XlogBackend>> {
        let name = CString::new(name_prefix).ok()?;
        let instance = unsafe { sys::mars_xlog_get_instance(name.as_ptr()) };
        if instance == 0 {
            return None;
        }

        Some(Arc::new(FfiBackend {
            instance,
            name_prefix: name_prefix.to_string(),
        }))
    }

    fn appender_open(&self, config: &XlogConfig, level: LogLevel) -> Result<(), XlogError> {
        if config.log_dir.is_empty() || config.name_prefix.is_empty() {
            return Err(XlogError::InvalidConfig);
        }

        let (cfg, _cstr) = config.to_sys();
        unsafe {
            sys::mars_xlog_appender_open(&cfg, level.as_sys() as c_int);
        }
        Ok(())
    }

    fn appender_close(&self) {
        unsafe {
            sys::mars_xlog_appender_close();
        }
    }

    fn flush_all(&self, sync: bool) {
        unsafe {
            sys::mars_xlog_flush_all(if sync { 1 } else { 0 });
        }
    }

    #[cfg(any(
        target_os = "ios",
        target_os = "macos",
        target_os = "tvos",
        target_os = "watchos"
    ))]
    fn set_console_fun(&self, fun: ConsoleFun) {
        unsafe {
            sys::mars_xlog_set_console_fun(fun as c_int);
        }
    }

    fn current_log_path(&self) -> Option<String> {
        crate::read_path(|buf, len| unsafe { sys::mars_xlog_get_current_log_path(buf, len) })
    }

    fn current_log_cache_path(&self) -> Option<String> {
        crate::read_path(|buf, len| unsafe { sys::mars_xlog_get_current_log_cache_path(buf, len) })
    }

    fn filepaths_from_timespan(&self, timespan: i32, prefix: &str) -> Vec<String> {
        crate::read_joined(|buf, len| unsafe {
            sys::mars_xlog_get_filepath_from_timespan(
                timespan,
                crate::cstr_or_null(prefix).as_ptr(),
                buf,
                len,
            )
        })
    }

    fn make_logfile_name(&self, timespan: i32, prefix: &str) -> Vec<String> {
        crate::read_joined(|buf, len| unsafe {
            sys::mars_xlog_make_logfile_name(
                timespan,
                crate::cstr_or_null(prefix).as_ptr(),
                buf,
                len,
            )
        })
    }

    fn oneshot_flush(&self, config: &XlogConfig) -> Result<FileIoAction, XlogError> {
        if config.log_dir.is_empty() || config.name_prefix.is_empty() {
            return Err(XlogError::InvalidConfig);
        }

        let (cfg, _cstr) = config.to_sys();
        let mut action: c_int = 0;
        let ok = unsafe { sys::mars_xlog_oneshot_flush(&cfg, &mut action as *mut c_int) };
        if ok == 0 {
            return Err(XlogError::InitFailed);
        }
        Ok(FileIoAction::from(action))
    }

    fn dump(&self, buffer: &[u8]) -> String {
        if buffer.is_empty() {
            return String::new();
        }

        unsafe {
            let ptr = sys::mars_xlog_dump(buffer.as_ptr().cast(), buffer.len());
            crate::cstr_to_string(ptr)
        }
    }

    fn memory_dump(&self, buffer: &[u8]) -> String {
        if buffer.is_empty() {
            return String::new();
        }

        unsafe {
            let ptr = sys::mars_xlog_memory_dump(buffer.as_ptr().cast(), buffer.len());
            crate::cstr_to_string(ptr)
        }
    }
}

impl XlogBackend for FfiBackend {
    fn instance(&self) -> usize {
        self.instance
    }

    fn is_enabled(&self, level: LogLevel) -> bool {
        unsafe { sys::mars_xlog_is_enabled(self.instance, level.as_sys() as c_int) != 0 }
    }

    fn level(&self) -> LogLevel {
        match unsafe { sys::mars_xlog_get_level(self.instance) } {
            0 => LogLevel::Verbose,
            1 => LogLevel::Debug,
            2 => LogLevel::Info,
            3 => LogLevel::Warn,
            4 => LogLevel::Error,
            5 => LogLevel::Fatal,
            _ => LogLevel::None,
        }
    }

    fn set_level(&self, level: LogLevel) {
        unsafe {
            sys::mars_xlog_set_level(self.instance, level.as_sys() as c_int);
        }
    }

    fn set_appender_mode(&self, mode: AppenderMode) {
        unsafe {
            sys::mars_xlog_set_appender_mode(self.instance, mode.as_sys() as c_int);
        }
    }

    fn flush(&self, sync: bool) {
        unsafe {
            sys::mars_xlog_flush(self.instance, if sync { 1 } else { 0 });
        }
    }

    fn set_console_log_open(&self, open: bool) {
        unsafe {
            sys::mars_xlog_set_console_log_open(self.instance, if open { 1 } else { 0 });
        }
    }

    fn set_max_file_size(&self, max_bytes: i64) {
        unsafe {
            sys::mars_xlog_set_max_file_size(self.instance, max_bytes as _);
        }
    }

    fn set_max_alive_time(&self, alive_seconds: i64) {
        unsafe {
            sys::mars_xlog_set_max_alive_time(self.instance, alive_seconds as _);
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
        let mut cstrings = Vec::new();
        let tag_c = crate::to_cstring(tag, &mut cstrings);
        let file_c = crate::to_cstring(file, &mut cstrings);
        let func_c = crate::to_cstring(func, &mut cstrings);
        let msg_c = crate::to_cstring(msg, &mut cstrings);

        let mut tv: timeval = unsafe { std::mem::zeroed() };
        unsafe {
            gettimeofday(&mut tv, ptr::null_mut());
        }

        let info = sys::XLoggerInfo {
            level: level.as_sys(),
            tag: tag_c,
            filename: file_c,
            func_name: func_c,
            line: line as c_int,
            timeval: tv,
            pid: -1,
            tid: -1,
            maintid: -1,
            traceLog: 0,
        };

        unsafe {
            sys::mars_xlog_write(self.instance, &info, msg_c);
        }
    }
}
