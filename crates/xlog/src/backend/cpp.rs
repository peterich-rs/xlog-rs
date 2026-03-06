use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use libc::{c_char, c_int};
use mars_xlog_sys as sys;

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
    static PROVIDER: CppBackendProvider = CppBackendProvider;
    &PROVIDER
}

struct CppBackendProvider;

struct CppBackend {
    id: usize,
    handle: usize,
    name_prefix: String,
}

struct CppRegistry {
    instances: Mutex<HashMap<String, Weak<CppBackend>>>,
}

impl CppRegistry {
    fn new() -> Self {
        Self {
            instances: Mutex::new(HashMap::new()),
        }
    }

    fn get(&self, name_prefix: &str) -> Option<Arc<CppBackend>> {
        let mut guard = self.instances.lock().expect("cpp registry lock poisoned");
        if let Some(backend) = guard.get(name_prefix).and_then(Weak::upgrade) {
            return Some(backend);
        }
        guard.remove(name_prefix);
        None
    }

    fn get_or_try_insert_with(
        &self,
        name_prefix: &str,
        build: impl FnOnce() -> Result<Arc<CppBackend>, XlogError>,
    ) -> Result<Arc<CppBackend>, XlogError> {
        let mut guard = self.instances.lock().expect("cpp registry lock poisoned");
        if let Some(existing) = guard.get(name_prefix).and_then(Weak::upgrade) {
            return Ok(existing);
        }
        let backend = build()?;
        guard.insert(name_prefix.to_string(), Arc::downgrade(&backend));
        Ok(backend)
    }
}

struct OwnedSysConfig {
    log_dir: CString,
    name_prefix: CString,
    pub_key: Option<CString>,
    cache_dir: Option<CString>,
}

impl OwnedSysConfig {
    fn new(config: &XlogConfig) -> (sys::MarsXlogConfig, Self) {
        let owned = Self {
            log_dir: sanitize_cstring(&config.log_dir),
            name_prefix: sanitize_cstring(&config.name_prefix),
            pub_key: config.pub_key.as_deref().map(sanitize_cstring),
            cache_dir: config.cache_dir.as_deref().map(sanitize_cstring),
        };
        let sys_cfg = sys::MarsXlogConfig {
            mode: to_sys_mode(config.mode),
            logdir: owned.log_dir.as_ptr(),
            nameprefix: owned.name_prefix.as_ptr(),
            pub_key: owned.pub_key.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
            compress_mode: to_sys_compress(config.compress_mode),
            compress_level: config.compress_level,
            cache_dir: owned.cache_dir.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
            cache_days: config.cache_days,
        };
        (sys_cfg, owned)
    }
}

static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

fn registry() -> &'static CppRegistry {
    static REGISTRY: OnceLock<CppRegistry> = OnceLock::new();
    REGISTRY.get_or_init(CppRegistry::new)
}

fn sanitize_cstring(input: &str) -> CString {
    let mut bytes = input.as_bytes().to_vec();
    for byte in &mut bytes {
        if *byte == 0 {
            *byte = b' ';
        }
    }
    CString::new(bytes).expect("sanitized string must not contain nul")
}

fn maybe_cstring(input: &str) -> Option<CString> {
    if input.is_empty() {
        None
    } else {
        Some(sanitize_cstring(input))
    }
}

fn to_sys_level(level: LogLevel) -> sys::TLogLevel {
    match level {
        LogLevel::Verbose => sys::TLogLevel::kLevelVerbose,
        LogLevel::Debug => sys::TLogLevel::kLevelDebug,
        LogLevel::Info => sys::TLogLevel::kLevelInfo,
        LogLevel::Warn => sys::TLogLevel::kLevelWarn,
        LogLevel::Error => sys::TLogLevel::kLevelError,
        LogLevel::Fatal => sys::TLogLevel::kLevelFatal,
        LogLevel::None => sys::TLogLevel::kLevelNone,
    }
}

fn from_sys_level(level: c_int) -> LogLevel {
    match level {
        x if x == sys::TLogLevel::kLevelVerbose as c_int => LogLevel::Verbose,
        x if x == sys::TLogLevel::kLevelDebug as c_int => LogLevel::Debug,
        x if x == sys::TLogLevel::kLevelInfo as c_int => LogLevel::Info,
        x if x == sys::TLogLevel::kLevelWarn as c_int => LogLevel::Warn,
        x if x == sys::TLogLevel::kLevelError as c_int => LogLevel::Error,
        x if x == sys::TLogLevel::kLevelFatal as c_int => LogLevel::Fatal,
        _ => LogLevel::None,
    }
}

fn to_sys_mode(mode: AppenderMode) -> c_int {
    match mode {
        AppenderMode::Async => sys::TAppenderMode::kAppenderAsync as c_int,
        AppenderMode::Sync => sys::TAppenderMode::kAppenderSync as c_int,
    }
}

fn to_sys_compress(mode: CompressMode) -> c_int {
    match mode {
        CompressMode::Zlib => sys::TCompressMode::kZlib as c_int,
        CompressMode::Zstd => sys::TCompressMode::kZstd as c_int,
    }
}

fn now_timeval() -> libc::timeval {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    libc::timeval {
        tv_sec: now.as_secs() as _,
        tv_usec: now.subsec_micros() as _,
    }
}

fn write_with_meta_impl(
    instance: usize,
    level: LogLevel,
    tag: &str,
    file: &str,
    func: &str,
    line: u32,
    msg: &str,
    raw_meta: RawLogMeta,
) {
    let tag = maybe_cstring(tag);
    let file = maybe_cstring(file);
    let func = maybe_cstring(func);
    let msg = sanitize_cstring(msg);
    let mut info = sys::XLoggerInfo {
        level: to_sys_level(level),
        tag: tag.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
        filename: file.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
        func_name: func.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
        line: line as c_int,
        timeval: now_timeval(),
        pid: raw_meta.pid,
        tid: raw_meta.tid,
        maintid: raw_meta.maintid,
        traceLog: i32::from(raw_meta.trace_log),
    };

    unsafe {
        sys::mars_xlog_write(instance, &mut info, msg.as_ptr());
    }
}

fn read_path_buf(fetch: unsafe extern "C" fn(*mut c_char, u32) -> c_int) -> Option<String> {
    let mut buf = vec![0 as c_char; 16 * 1024];
    let ok = unsafe { fetch(buf.as_mut_ptr(), buf.len() as u32) };
    if ok == 0 {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned(),
    )
}

fn read_split_paths(
    fetch: unsafe extern "C" fn(c_int, *const c_char, *mut c_char, usize) -> usize,
    timespan: i32,
    prefix: &str,
) -> Vec<String> {
    let prefix = maybe_cstring(prefix);
    let prefix_ptr = prefix.as_ref().map_or(ptr::null(), |s| s.as_ptr());
    let len = unsafe { fetch(timespan, prefix_ptr, ptr::null_mut(), 0) };
    if len == 0 {
        return Vec::new();
    }
    let mut buf = vec![0 as c_char; len];
    let written = unsafe { fetch(timespan, prefix_ptr, buf.as_mut_ptr(), buf.len()) };
    if written == 0 {
        return Vec::new();
    }
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .split('\n')
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

impl Drop for CppBackend {
    fn drop(&mut self) {
        let name = sanitize_cstring(&self.name_prefix);
        unsafe {
            sys::mars_xlog_release_instance(name.as_ptr());
        }
    }
}

impl XlogBackend for CppBackend {
    fn instance(&self) -> usize {
        self.id
    }

    fn is_enabled(&self, level: LogLevel) -> bool {
        unsafe { sys::mars_xlog_is_enabled(self.handle, to_sys_level(level) as c_int) != 0 }
    }

    fn level(&self) -> LogLevel {
        from_sys_level(unsafe { sys::mars_xlog_get_level(self.handle) })
    }

    fn set_level(&self, level: LogLevel) {
        unsafe {
            sys::mars_xlog_set_level(self.handle, to_sys_level(level) as c_int);
        }
    }

    fn set_appender_mode(&self, mode: AppenderMode) {
        unsafe {
            sys::mars_xlog_set_appender_mode(self.handle, to_sys_mode(mode));
        }
    }

    fn flush(&self, sync: bool) {
        unsafe {
            sys::mars_xlog_flush(self.handle, i32::from(sync));
        }
    }

    fn set_console_log_open(&self, open: bool) {
        unsafe {
            sys::mars_xlog_set_console_log_open(self.handle, i32::from(open));
        }
    }

    fn set_max_file_size(&self, max_bytes: i64) {
        unsafe {
            sys::mars_xlog_set_max_file_size(self.handle, max_bytes as _);
        }
    }

    fn set_max_alive_time(&self, alive_seconds: i64) {
        unsafe {
            sys::mars_xlog_set_max_alive_time(self.handle, alive_seconds as _);
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
        raw_meta: RawLogMeta,
    ) {
        write_with_meta_impl(self.handle, level, tag, file, func, line, msg, raw_meta);
    }
}

impl XlogBackendProvider for CppBackendProvider {
    fn new_instance(
        &self,
        config: &XlogConfig,
        level: LogLevel,
    ) -> Result<Arc<dyn XlogBackend>, XlogError> {
        let backend = registry().get_or_try_insert_with(&config.name_prefix, || {
            let (sys_cfg, _owned) = OwnedSysConfig::new(config);
            let handle = unsafe {
                sys::mars_xlog_new_instance(&sys_cfg, to_sys_level(level) as c_int)
            };
            if handle == 0 {
                return Err(XlogError::InitFailed);
            }
            Ok(Arc::new(CppBackend {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                handle,
                name_prefix: config.name_prefix.clone(),
            }))
        })?;
        Ok(backend)
    }

    fn get_instance(&self, name_prefix: &str) -> Option<Arc<dyn XlogBackend>> {
        if let Some(backend) = registry().get(name_prefix) {
            return Some(backend);
        }

        let name = sanitize_cstring(name_prefix);
        let handle = unsafe { sys::mars_xlog_get_instance(name.as_ptr()) };
        if handle == 0 {
            return None;
        }
        let backend = registry()
            .get_or_try_insert_with(name_prefix, || {
                Ok(Arc::new(CppBackend {
                    id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                    handle,
                    name_prefix: name_prefix.to_string(),
                }))
            })
            .ok()?;
        Some(backend)
    }

    fn appender_open(&self, config: &XlogConfig, level: LogLevel) -> Result<(), XlogError> {
        if config.log_dir.is_empty() || config.name_prefix.is_empty() {
            return Err(XlogError::InvalidConfig);
        }
        let (sys_cfg, _owned) = OwnedSysConfig::new(config);
        unsafe {
            sys::mars_xlog_appender_open(&sys_cfg, to_sys_level(level) as c_int);
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
            sys::mars_xlog_flush_all(i32::from(sync));
        }
    }

    fn global_is_enabled(&self, level: LogLevel) -> bool {
        unsafe { sys::mars_xlog_is_enabled(0, to_sys_level(level) as c_int) != 0 }
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
        write_with_meta_impl(0, level, tag, file, func, line, msg, raw_meta);
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
        read_path_buf(sys::mars_xlog_get_current_log_path)
    }

    fn current_log_cache_path(&self) -> Option<String> {
        read_path_buf(sys::mars_xlog_get_current_log_cache_path)
    }

    fn filepaths_from_timespan(&self, timespan: i32, prefix: &str) -> Vec<String> {
        read_split_paths(sys::mars_xlog_get_filepath_from_timespan, timespan, prefix)
    }

    fn make_logfile_name(&self, timespan: i32, prefix: &str) -> Vec<String> {
        read_split_paths(sys::mars_xlog_make_logfile_name, timespan, prefix)
    }

    fn oneshot_flush(&self, config: &XlogConfig) -> Result<FileIoAction, XlogError> {
        if config.log_dir.is_empty() || config.name_prefix.is_empty() {
            return Err(XlogError::InvalidConfig);
        }
        let (sys_cfg, _owned) = OwnedSysConfig::new(config);
        let mut action = 0;
        let ok = unsafe { sys::mars_xlog_oneshot_flush(&sys_cfg, &mut action) };
        if ok == 0 {
            return Err(XlogError::InitFailed);
        }
        Ok(FileIoAction::from(action))
    }

    fn dump(&self, buffer: &[u8]) -> String {
        let ptr = unsafe { sys::mars_xlog_dump(buffer.as_ptr().cast(), buffer.len()) };
        if ptr.is_null() {
            return String::new();
        }
        unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
    }

    fn memory_dump(&self, buffer: &[u8]) -> String {
        let ptr = unsafe { sys::mars_xlog_memory_dump(buffer.as_ptr().cast(), buffer.len()) };
        if ptr.is_null() {
            return String::new();
        }
        unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
    }
}
