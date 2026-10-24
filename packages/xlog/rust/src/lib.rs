use std::cell::RefCell;
use std::ffi::{c_char, CStr, CString};
use std::fs;
use std::path::Path;
use std::ptr;
use std::sync::OnceLock;
use std::thread;
use std::time::{Instant, UNIX_EPOCH};

use mars_xlog::{AppenderMode, CompressMode, LogLevel, Xlog, XlogConfig};
use mars_xlog_core::compress::{decompress_raw_zlib, decompress_zstd_frames};
use mars_xlog_core::protocol::{
    AppendMode, CompressionKind, LogHeader, HEADER_LEN, MAGIC_END, TAILER_LEN,
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use serde::{Deserialize, Serialize};

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

static METRICS_HANDLE: OnceLock<Result<PrometheusHandle, String>> = OnceLock::new();

pub struct LoggerState {
    logger: Xlog,
    log_dir: String,
    name_prefix: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoggerConfigDto {
    log_dir: String,
    name_prefix: String,
    pub_key: Option<String>,
    cache_dir: Option<String>,
    cache_days: i32,
    mode: i32,
    compress_mode: i32,
    compress_level: i32,
    enable_console: bool,
    max_file_size_bytes: Option<i64>,
    max_alive_time_seconds: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LogFileEntry {
    path: String,
    file_name: String,
    extension: String,
    size_bytes: u64,
    modified_at_millis: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BenchmarkReport {
    iterations: u32,
    threads: u32,
    message_bytes: u32,
    elapsed_micros: u64,
    lines_per_second: f64,
    bytes_per_second: f64,
    current_log_path: Option<String>,
}

#[derive(Debug)]
struct OwnedCString(*mut c_char);

impl OwnedCString {
    fn from_string(value: impl Into<String>) -> Result<Self, String> {
        let sanitized = value.into().replace('\0', " ");
        let cstr = CString::new(sanitized).map_err(|err| err.to_string())?;
        Ok(Self(cstr.into_raw()))
    }

    fn into_raw(self) -> *mut c_char {
        let ptr = self.0;
        std::mem::forget(self);
        ptr
    }
}

impl Drop for OwnedCString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` always originates from `CString::into_raw`.
            unsafe {
                let _ = CString::from_raw(self.0);
            }
        }
    }
}

fn set_last_error(message: impl Into<String>) {
    let message = message.into();
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = CString::new(message.replace('\0', " ")).ok();
    });
}

fn take_last_error() -> *mut c_char {
    LAST_ERROR.with(|slot| {
        slot.borrow_mut()
            .take()
            .map(CString::into_raw)
            .unwrap_or(ptr::null_mut())
    })
}

fn get_metrics_handle() -> Result<PrometheusHandle, String> {
    METRICS_HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .map_err(|err| err.to_string())
        })
        .clone()
}

fn cstr_to_string(ptr: *const c_char, field: &str) -> Result<String, String> {
    if ptr.is_null() {
        return Err(format!("{field} is null"));
    }
    // SAFETY: `ptr` is expected to point to a valid NUL-terminated UTF-8 string from Dart.
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map(|value| value.to_owned())
        .map_err(|_| format!("{field} is not valid UTF-8"))
}

fn optional_cstr_to_string(ptr: *const c_char) -> Result<Option<String>, String> {
    if ptr.is_null() {
        return Ok(None);
    }
    let value = cstr_to_string(ptr, "string")?;
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn to_level(level: i32) -> Result<LogLevel, String> {
    match level {
        0 => Ok(LogLevel::Verbose),
        1 => Ok(LogLevel::Debug),
        2 => Ok(LogLevel::Info),
        3 => Ok(LogLevel::Warn),
        4 => Ok(LogLevel::Error),
        5 => Ok(LogLevel::Fatal),
        6 => Ok(LogLevel::None),
        _ => Err(format!("unknown log level: {level}")),
    }
}

fn to_appender_mode(mode: i32) -> Result<AppenderMode, String> {
    match mode {
        0 => Ok(AppenderMode::Async),
        1 => Ok(AppenderMode::Sync),
        _ => Err(format!("unknown appender mode: {mode}")),
    }
}

fn to_compress_mode(mode: i32) -> Result<CompressMode, String> {
    match mode {
        0 => Ok(CompressMode::Zlib),
        1 => Ok(CompressMode::Zstd),
        _ => Err(format!("unknown compress mode: {mode}")),
    }
}

fn parse_logger_config(config_json: *const c_char) -> Result<LoggerConfigDto, String> {
    let raw = cstr_to_string(config_json, "configJson")?;
    serde_json::from_str(&raw).map_err(|err| format!("invalid configJson: {err}"))
}

fn build_logger_config(dto: &LoggerConfigDto) -> Result<XlogConfig, String> {
    let mut config = XlogConfig::new(dto.log_dir.clone(), dto.name_prefix.clone())
        .cache_days(dto.cache_days)
        .mode(to_appender_mode(dto.mode)?)
        .compress_mode(to_compress_mode(dto.compress_mode)?)
        .compress_level(dto.compress_level);

    if let Some(pub_key) = dto.pub_key.as_ref().filter(|value| !value.is_empty()) {
        config = config.pub_key(pub_key.clone());
    }

    if let Some(cache_dir) = dto.cache_dir.as_ref().filter(|value| !value.is_empty()) {
        config = config.cache_dir(cache_dir.clone());
    }

    Ok(config)
}

fn logger_state<'a>(handle: *mut LoggerState) -> Result<&'a LoggerState, String> {
    if handle.is_null() {
        return Err("logger handle is null".to_string());
    }

    // SAFETY: the pointer is created by `Box::into_raw` in `mxl_logger_new`
    // and remains valid until `mxl_logger_free`.
    Ok(unsafe { &*handle })
}

fn benchmark_message(bytes: usize) -> String {
    const PATTERN: &str = "mars-xlog-native-assets-benchmark|";
    let mut out = String::with_capacity(bytes.max(PATTERN.len()));
    while out.len() < bytes {
        out.push_str(PATTERN);
    }
    out.truncate(bytes.max(1));
    out
}

fn list_log_files_impl(root: &str, limit: usize) -> Result<Vec<LogFileEntry>, String> {
    let root = Path::new(root);
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    visit_log_files(root, &mut out)?;
    out.sort_by(|left, right| {
        right
            .modified_at_millis
            .cmp(&left.modified_at_millis)
            .then_with(|| right.path.cmp(&left.path))
    });
    if limit > 0 && out.len() > limit {
        out.truncate(limit);
    }
    Ok(out)
}

fn visit_log_files(dir: &Path, out: &mut Vec<LogFileEntry>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|err| format!("read_dir {}: {err}", dir.display()))? {
        let entry = entry.map_err(|err| format!("dir entry {}: {err}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            visit_log_files(&path, out)?;
            continue;
        }

        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_string();
        if extension != "xlog" && extension != "dump" {
            continue;
        }

        let metadata = entry
            .metadata()
            .map_err(|err| format!("metadata {}: {err}", path.display()))?;
        let modified_at_millis = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_millis().min(i64::MAX as u128) as i64)
            .unwrap_or_default();

        out.push(LogFileEntry {
            path: path.to_string_lossy().to_string(),
            file_name: path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_string(),
            extension,
            size_bytes: metadata.len(),
            modified_at_millis,
        });
    }

    Ok(())
}

fn parse_blocks(bytes: &[u8]) -> Vec<(LogHeader, Vec<u8>)> {
    let mut blocks = Vec::new();
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

        blocks.push((header, bytes[payload_start..payload_end].to_vec()));
        offset = payload_end + TAILER_LEN;
    }

    blocks
}

fn decode_block_payload(header: &LogHeader, payload: &[u8]) -> Result<Vec<u8>, String> {
    if header.magic.encrypted() {
        return Err(format!(
            "encrypted block seq={} len={} cannot be decoded without the private key",
            header.seq, header.len
        ));
    }
    match (header.magic.mode(), header.magic.family()) {
        (AppendMode::Async, CompressionKind::Zlib) => {
            decompress_raw_zlib(payload).map_err(|err| err.to_string())
        }
        (AppendMode::Async, CompressionKind::Zstd) => {
            decompress_zstd_frames(payload).map_err(|err| err.to_string())
        }
        (AppendMode::Sync, _) => Ok(payload.to_vec()),
    }
}

fn decode_log_file_impl(path: &str) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|err| format!("read {path}: {err}"))?;
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();

    if extension != "xlog" {
        return Ok(match String::from_utf8(bytes.clone()) {
            Ok(text) => text,
            Err(_) => mars_xlog::Xlog::memory_dump(&bytes),
        });
    }

    let blocks = parse_blocks(&bytes);
    if blocks.is_empty() {
        return Ok(mars_xlog::Xlog::memory_dump(&bytes));
    }

    let mut out = String::new();
    for (index, (header, payload)) in blocks.into_iter().enumerate() {
        if index > 0 && !out.ends_with('\n') {
            out.push('\n');
        }

        match decode_block_payload(&header, &payload) {
            Ok(plain) => out.push_str(&String::from_utf8_lossy(&plain)),
            Err(message) => {
                out.push('[');
                out.push_str(&message);
                out.push_str("]\n");
            }
        }
    }

    Ok(out)
}

fn current_log_path_for(state: &LoggerState) -> Option<String> {
    list_log_files_impl(&state.log_dir, 1)
        .ok()
        .and_then(|files| files.into_iter().next())
        .map(|entry| entry.path)
}

fn run_benchmark_impl(
    state: &LoggerState,
    level: i32,
    tag: Option<String>,
    iterations: u32,
    message_bytes: u32,
    threads: u32,
) -> Result<String, String> {
    let level = to_level(level)?;
    let threads = threads.max(1) as usize;
    let total_iterations = iterations as usize;
    let message = benchmark_message(message_bytes as usize);
    let tag = tag.filter(|value| !value.is_empty());

    let start = Instant::now();
    if total_iterations > 0 {
        let base = total_iterations / threads;
        let remainder = total_iterations % threads;
        let mut joins = Vec::with_capacity(threads);

        for index in 0..threads {
            let logger = state.logger.clone();
            let tag = tag.clone();
            let message = message.clone();
            let count = base + usize::from(index < remainder);
            joins.push(thread::spawn(move || {
                for _ in 0..count {
                    logger.write(level, tag.as_deref(), &message);
                }
            }));
        }

        for join in joins {
            join.join()
                .map_err(|_| "benchmark worker thread panicked".to_string())?;
        }
    }
    state.logger.flush(true);
    let elapsed = start.elapsed();
    let elapsed_micros = elapsed.as_micros().min(u64::MAX as u128) as u64;
    let elapsed_secs = elapsed.as_secs_f64();
    let total_bytes = total_iterations as f64 * message.len() as f64;

    let report = BenchmarkReport {
        iterations,
        threads: threads as u32,
        message_bytes: message.len() as u32,
        elapsed_micros,
        lines_per_second: if elapsed_secs > 0.0 {
            total_iterations as f64 / elapsed_secs
        } else {
            total_iterations as f64
        },
        bytes_per_second: if elapsed_secs > 0.0 {
            total_bytes / elapsed_secs
        } else {
            total_bytes
        },
        current_log_path: current_log_path_for(state),
    };

    serde_json::to_string(&report).map_err(|err| err.to_string())
}

fn wrap_bool<F>(operation: F) -> bool
where
    F: FnOnce() -> Result<(), String>,
{
    match operation() {
        Ok(()) => true,
        Err(err) => {
            set_last_error(err);
            false
        }
    }
}

fn wrap_ptr<F>(operation: F) -> *mut c_char
where
    F: FnOnce() -> Result<String, String>,
{
    match operation().and_then(OwnedCString::from_string) {
        Ok(value) => value.into_raw(),
        Err(err) => {
            set_last_error(err);
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn mxl_last_error_message() -> *mut c_char {
    take_last_error()
}

#[no_mangle]
pub extern "C" fn mxl_string_free(value: *mut c_char) {
    if value.is_null() {
        return;
    }

    // SAFETY: `value` must have been returned by this library via `CString::into_raw`.
    unsafe {
        let _ = CString::from_raw(value);
    }
}

#[no_mangle]
pub extern "C" fn mxl_logger_new(config_json: *const c_char, level: i32) -> *mut LoggerState {
    match (|| -> Result<*mut LoggerState, String> {
        let dto = parse_logger_config(config_json)?;
        let config = build_logger_config(&dto)?;
        let level = to_level(level)?;

        let _ = get_metrics_handle();

        let logger = Xlog::init(config, level).map_err(|err| err.to_string())?;
        logger.set_console_log_open(dto.enable_console);
        if let Some(max_bytes) = dto.max_file_size_bytes {
            logger.set_max_file_size(max_bytes);
        }
        if let Some(alive_seconds) = dto.max_alive_time_seconds {
            let _ = logger.set_max_alive_time(alive_seconds);
        }

        Ok(Box::into_raw(Box::new(LoggerState {
            logger,
            log_dir: dto.log_dir,
            name_prefix: dto.name_prefix,
        })))
    })() {
        Ok(handle) => handle,
        Err(err) => {
            set_last_error(err);
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn mxl_logger_free(handle: *mut LoggerState) {
    if handle.is_null() {
        return;
    }

    // SAFETY: `handle` originates from `Box::into_raw` in `mxl_logger_new`.
    unsafe {
        drop(Box::from_raw(handle));
    }
}

#[no_mangle]
pub extern "C" fn mxl_logger_set_level(handle: *mut LoggerState, level: i32) -> bool {
    wrap_bool(|| {
        let state = logger_state(handle)?;
        state.logger.set_level(to_level(level)?);
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn mxl_logger_set_appender_mode(handle: *mut LoggerState, mode: i32) -> bool {
    wrap_bool(|| {
        let state = logger_state(handle)?;
        state.logger.set_appender_mode(to_appender_mode(mode)?);
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn mxl_logger_set_console_open(handle: *mut LoggerState, open: bool) -> bool {
    wrap_bool(|| {
        let state = logger_state(handle)?;
        state.logger.set_console_log_open(open);
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn mxl_logger_set_max_file_size(handle: *mut LoggerState, max_bytes: i64) -> bool {
    wrap_bool(|| {
        let state = logger_state(handle)?;
        state.logger.set_max_file_size(max_bytes);
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn mxl_logger_set_max_alive_time(
    handle: *mut LoggerState,
    alive_seconds: i64,
) -> bool {
    wrap_bool(|| {
        let state = logger_state(handle)?;
        let _ = state.logger.set_max_alive_time(alive_seconds);
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn mxl_logger_flush(handle: *mut LoggerState, sync: bool) -> bool {
    wrap_bool(|| {
        let state = logger_state(handle)?;
        state.logger.flush(sync);
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn mxl_logger_log(
    handle: *mut LoggerState,
    level: i32,
    tag: *const c_char,
    message: *const c_char,
) -> bool {
    wrap_bool(|| {
        let state = logger_state(handle)?;
        let level = to_level(level)?;
        let tag = optional_cstr_to_string(tag)?;
        let message = cstr_to_string(message, "message")?;
        state.logger.write(level, tag.as_deref(), &message);
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn mxl_logger_log_with_meta(
    handle: *mut LoggerState,
    level: i32,
    tag: *const c_char,
    file: *const c_char,
    function_name: *const c_char,
    line: u32,
    message: *const c_char,
) -> bool {
    wrap_bool(|| {
        let state = logger_state(handle)?;
        let level = to_level(level)?;
        let tag = optional_cstr_to_string(tag)?;
        let file = optional_cstr_to_string(file)?.unwrap_or_default();
        let function_name = optional_cstr_to_string(function_name)?.unwrap_or_default();
        let message = cstr_to_string(message, "message")?;
        state
            .logger
            .write_with_meta(level, tag.as_deref(), &file, &function_name, line, &message);
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn mxl_logger_list_files(handle: *mut LoggerState, limit: u32) -> *mut c_char {
    wrap_ptr(|| {
        let state = logger_state(handle)?;
        let files = list_log_files_impl(&state.log_dir, limit as usize)?;
        serde_json::to_string(&files).map_err(|err| err.to_string())
    })
}

#[no_mangle]
pub extern "C" fn mxl_logger_name_prefix(handle: *mut LoggerState) -> *mut c_char {
    wrap_ptr(|| {
        let state = logger_state(handle)?;
        Ok(state.name_prefix.clone())
    })
}

#[no_mangle]
pub extern "C" fn mxl_logger_benchmark(
    handle: *mut LoggerState,
    level: i32,
    tag: *const c_char,
    iterations: u32,
    message_bytes: u32,
    threads: u32,
) -> *mut c_char {
    wrap_ptr(|| {
        let state = logger_state(handle)?;
        let tag = optional_cstr_to_string(tag)?;
        run_benchmark_impl(state, level, tag, iterations, message_bytes, threads)
    })
}

#[no_mangle]
pub extern "C" fn mxl_decode_log_file(path: *const c_char) -> *mut c_char {
    wrap_ptr(|| decode_log_file_impl(&cstr_to_string(path, "path")?))
}

#[no_mangle]
pub extern "C" fn mxl_metrics_snapshot() -> *mut c_char {
    wrap_ptr(|| Ok(get_metrics_handle()?.render()))
}
