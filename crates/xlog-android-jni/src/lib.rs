//! JNI bridge used by the Android example app.
//!
//! The exported symbols are consumed from Java/Kotlin via the `XlogBridge`
//! wrapper in `examples/android-jni`. They map Java-friendly primitives to the
//! safe Rust API in `mars-xlog`.
use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::sys::{jboolean, jbyteArray, jint, jlong, jobjectArray, jstring};
use jni::JNIEnv;
use mars_xlog::{AppenderMode, CompressMode, FileIoAction, LogLevel, RawLogMeta, Xlog, XlogConfig};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::ptr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

/// Registry of live logger handles keyed by opaque ids.
static LOGGERS: Lazy<Mutex<HashMap<i64, Xlog>>> = Lazy::new(|| Mutex::new(HashMap::new()));
/// Monotonic id generator for Java-side handles.
static NEXT_ID: AtomicI64 = AtomicI64::new(1);

/// Allocate a new handle id.
fn next_id() -> i64 {
    NEXT_ID.fetch_add(1, Ordering::SeqCst)
}

/// Insert a logger into the registry and return its id.
fn insert_logger(logger: Xlog) -> i64 {
    let id = next_id();
    let mut store = LOGGERS.lock().expect("logger store poisoned");
    store.insert(id, logger);
    id
}

/// Look up a logger by id.
fn get_logger(id: i64) -> Option<Xlog> {
    let store = LOGGERS.lock().expect("logger store poisoned");
    store.get(&id).cloned()
}

/// Remove a logger by id.
fn remove_logger(id: i64) -> bool {
    let mut store = LOGGERS.lock().expect("logger store poisoned");
    store.remove(&id).is_some()
}

/// Convert an optional Java string into Rust.
fn opt_string(env: &mut JNIEnv, input: JString) -> Option<String> {
    if input.is_null() {
        return None;
    }
    env.get_string(&input).ok().map(|s| s.into())
}

/// Convert a Java string into Rust, defaulting to empty on error.
fn req_string(env: &mut JNIEnv, input: JString) -> String {
    opt_string(env, input).unwrap_or_default()
}

/// Map the Java enum ordinal to `LogLevel`.
fn to_log_level(value: jint) -> LogLevel {
    match value {
        0 => LogLevel::Verbose,
        1 => LogLevel::Debug,
        2 => LogLevel::Info,
        3 => LogLevel::Warn,
        4 => LogLevel::Error,
        5 => LogLevel::Fatal,
        _ => LogLevel::None,
    }
}

/// Map the Java enum ordinal to `AppenderMode`.
fn to_appender_mode(value: jint) -> AppenderMode {
    match value {
        1 => AppenderMode::Sync,
        _ => AppenderMode::Async,
    }
}

/// Map the Java enum ordinal to `CompressMode`.
fn to_compress_mode(value: jint) -> CompressMode {
    match value {
        1 => CompressMode::Zstd,
        _ => CompressMode::Zlib,
    }
}

/// Convert a JNI boolean to Rust bool.
fn to_bool(value: jboolean) -> bool {
    value != 0
}

/// Convert an optional Rust string to a JNI string handle.
fn to_jstring(env: &mut JNIEnv, value: Option<String>) -> jstring {
    match value {
        Some(s) => env
            .new_string(s)
            .map(|s| s.into_raw())
            .unwrap_or(ptr::null_mut()),
        None => ptr::null_mut(),
    }
}

/// Convert a vector of Rust strings into a Java `String[]`.
fn strings_to_array(env: &mut JNIEnv, values: Vec<String>) -> jobjectArray {
    let array = env
        .new_object_array(values.len() as jint, "java/lang/String", JObject::null())
        .unwrap_or_else(|_| {
            env.new_object_array(0, "java/lang/String", JObject::null())
                .unwrap()
        });
    for (idx, value) in values.into_iter().enumerate() {
        if let Ok(jstr) = env.new_string(value) {
            let _ = env.set_object_array_element(&array, idx as jint, jstr);
        }
    }
    array.into_raw()
}

/// Convert a Java byte array to a Rust Vec.
fn bytes_from_array(env: &mut JNIEnv, input: jbyteArray) -> Vec<u8> {
    // SAFETY: `input` is a raw local reference passed by JNI for this call.
    // It may be null, which `from_raw` allows; `convert_byte_array` handles errors.
    let array = unsafe { JByteArray::from_raw(input) };
    env.convert_byte_array(array).unwrap_or_default()
}

#[no_mangle]
/// Create a new logger instance and return its handle id.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeCreateLogger(
    mut env: JNIEnv,
    _class: JClass,
    log_dir: JString,
    name_prefix: JString,
    pub_key: JString,
    cache_dir: JString,
    cache_days: jint,
    mode: jint,
    compress_mode: jint,
    compress_level: jint,
    level: jint,
) -> jlong {
    let log_dir = req_string(&mut env, log_dir);
    let name_prefix = req_string(&mut env, name_prefix);
    let pub_key = opt_string(&mut env, pub_key);
    let cache_dir = opt_string(&mut env, cache_dir);

    let mut cfg = XlogConfig::new(log_dir, name_prefix)
        .cache_days(cache_days)
        .mode(to_appender_mode(mode))
        .compress_mode(to_compress_mode(compress_mode))
        .compress_level(compress_level);
    if let Some(key) = pub_key {
        if !key.is_empty() {
            cfg = cfg.pub_key(key);
        }
    }
    if let Some(dir) = cache_dir {
        if !dir.is_empty() {
            cfg = cfg.cache_dir(dir);
        }
    }

    match Xlog::init(cfg, to_log_level(level)) {
        Ok(logger) => insert_logger(logger) as jlong,
        Err(_) => 0,
    }
}

#[no_mangle]
/// Look up a logger by name prefix and return its handle id.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeGetLogger(
    mut env: JNIEnv,
    _class: JClass,
    name_prefix: JString,
) -> jlong {
    let name_prefix = req_string(&mut env, name_prefix);
    match Xlog::get(&name_prefix) {
        Some(logger) => insert_logger(logger) as jlong,
        None => 0,
    }
}

#[no_mangle]
/// Release a logger handle.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeReleaseLogger(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jboolean {
    if handle == 0 {
        return 0;
    }
    if remove_logger(handle as i64) {
        1
    } else {
        0
    }
}

#[no_mangle]
/// Open the global appender using the provided config.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeOpenAppender(
    mut env: JNIEnv,
    _class: JClass,
    log_dir: JString,
    name_prefix: JString,
    pub_key: JString,
    cache_dir: JString,
    cache_days: jint,
    mode: jint,
    compress_mode: jint,
    compress_level: jint,
    level: jint,
) -> jboolean {
    let log_dir = req_string(&mut env, log_dir);
    let name_prefix = req_string(&mut env, name_prefix);
    let pub_key = opt_string(&mut env, pub_key);
    let cache_dir = opt_string(&mut env, cache_dir);

    let mut cfg = XlogConfig::new(log_dir, name_prefix)
        .cache_days(cache_days)
        .mode(to_appender_mode(mode))
        .compress_mode(to_compress_mode(compress_mode))
        .compress_level(compress_level);
    if let Some(key) = pub_key {
        if !key.is_empty() {
            cfg = cfg.pub_key(key);
        }
    }
    if let Some(dir) = cache_dir {
        if !dir.is_empty() {
            cfg = cfg.cache_dir(dir);
        }
    }

    match Xlog::appender_open(cfg, to_log_level(level)) {
        Ok(()) => 1,
        Err(_) => 0,
    }
}

#[no_mangle]
/// Close the global appender.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeCloseAppender(
    _env: JNIEnv,
    _class: JClass,
) {
    Xlog::appender_close();
}

#[no_mangle]
/// Flush all instances.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeFlushAll(
    _env: JNIEnv,
    _class: JClass,
    sync: jboolean,
) {
    Xlog::flush_all(to_bool(sync));
}

#[no_mangle]
/// Check whether a log level is enabled for the given handle.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeIsEnabled(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    level: jint,
) -> jboolean {
    if let Some(logger) = get_logger(handle as i64) {
        if logger.is_enabled(to_log_level(level)) {
            return 1;
        }
    }
    0
}

#[no_mangle]
/// Get the current log level for a handle.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeGetLevel(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jint {
    if let Some(logger) = get_logger(handle as i64) {
        return match logger.level() {
            LogLevel::Verbose => 0,
            LogLevel::Debug => 1,
            LogLevel::Info => 2,
            LogLevel::Warn => 3,
            LogLevel::Error => 4,
            LogLevel::Fatal => 5,
            LogLevel::None => 6,
        };
    }
    -1
}

#[no_mangle]
/// Set the log level for a handle.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeSetLevel(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    level: jint,
) {
    if let Some(logger) = get_logger(handle as i64) {
        logger.set_level(to_log_level(level));
    }
}

#[no_mangle]
/// Set the appender mode for a handle.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeSetAppenderMode(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    mode: jint,
) {
    if let Some(logger) = get_logger(handle as i64) {
        logger.set_appender_mode(to_appender_mode(mode));
    }
}

#[no_mangle]
/// Flush logs for a single handle.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeFlush(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    sync: jboolean,
) {
    if let Some(logger) = get_logger(handle as i64) {
        logger.flush(to_bool(sync));
    }
}

#[no_mangle]
/// Enable or disable console logging for a handle.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeSetConsoleLogOpen(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    open: jboolean,
) {
    if let Some(logger) = get_logger(handle as i64) {
        logger.set_console_log_open(to_bool(open));
    }
}

#[no_mangle]
/// Set maximum file size for a handle.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeSetMaxFileSize(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    max_bytes: jlong,
) {
    if let Some(logger) = get_logger(handle as i64) {
        logger.set_max_file_size(max_bytes as i64);
    }
}

#[no_mangle]
/// Set maximum log file age for a handle.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeSetMaxAliveTime(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    alive_seconds: jlong,
) {
    if let Some(logger) = get_logger(handle as i64) {
        logger.set_max_alive_time(alive_seconds as i64);
    }
}

#[no_mangle]
/// Write a log message.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeWrite(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    level: jint,
    tag: JString,
    message: JString,
) {
    if let Some(logger) = get_logger(handle as i64) {
        let tag = opt_string(&mut env, tag);
        let message = req_string(&mut env, message);
        logger.write(to_log_level(level), tag.as_deref(), &message);
    }
}

#[no_mangle]
/// Write a log message with explicit metadata.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeWriteWithMeta(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    level: jint,
    tag: JString,
    file: JString,
    func: JString,
    line: jint,
    message: JString,
) {
    if let Some(logger) = get_logger(handle as i64) {
        let tag = opt_string(&mut env, tag);
        let file = req_string(&mut env, file);
        let func = req_string(&mut env, func);
        let message = req_string(&mut env, message);
        logger.write_with_meta(
            to_log_level(level),
            tag.as_deref(),
            &file,
            &func,
            line as u32,
            &message,
        );
    }
}

#[no_mangle]
/// Write a log message with explicit metadata and raw pid/tid/trace flags.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeWriteWithRawMeta(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    level: jint,
    tag: JString,
    file: JString,
    func: JString,
    line: jint,
    pid: jlong,
    tid: jlong,
    maintid: jlong,
    trace_log: jboolean,
    message: JString,
) {
    if let Some(logger) = get_logger(handle as i64) {
        let tag = opt_string(&mut env, tag);
        let file = req_string(&mut env, file);
        let func = req_string(&mut env, func);
        let message = req_string(&mut env, message);
        let raw_meta = RawLogMeta::new(pid as i64, tid as i64, maintid as i64)
            .with_trace_log(to_bool(trace_log));
        logger.write_with_meta_raw(
            to_log_level(level),
            tag.as_deref(),
            &file,
            &func,
            line as u32,
            &message,
            raw_meta,
        );
    }
}

#[no_mangle]
/// Write to the global/default appender with raw metadata.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeAppenderWriteWithRawMeta(
    mut env: JNIEnv,
    _class: JClass,
    level: jint,
    tag: JString,
    file: JString,
    func: JString,
    line: jint,
    pid: jlong,
    tid: jlong,
    maintid: jlong,
    trace_log: jboolean,
    message: JString,
) {
    let tag = opt_string(&mut env, tag);
    let file = req_string(&mut env, file);
    let func = req_string(&mut env, func);
    let message = req_string(&mut env, message);
    let raw_meta =
        RawLogMeta::new(pid as i64, tid as i64, maintid as i64).with_trace_log(to_bool(trace_log));
    Xlog::appender_write_with_meta_raw(
        to_log_level(level),
        tag.as_deref(),
        &file,
        &func,
        line as u32,
        &message,
        raw_meta,
    );
}

#[no_mangle]
/// Get the current log path for the global appender.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeCurrentLogPath(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    to_jstring(&mut env, Xlog::current_log_path())
}

#[no_mangle]
/// Get the current log cache path for the global appender.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeCurrentLogCachePath(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    to_jstring(&mut env, Xlog::current_log_cache_path())
}

#[no_mangle]
/// List log file paths for a given timespan (days from today).
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeFilepathsFromTimespan(
    mut env: JNIEnv,
    _class: JClass,
    timespan: jint,
    prefix: JString,
) -> jobjectArray {
    let prefix = req_string(&mut env, prefix);
    strings_to_array(&mut env, Xlog::filepaths_from_timespan(timespan, &prefix))
}

#[no_mangle]
/// Build log file names for a given timespan (days from today).
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeMakeLogfileName(
    mut env: JNIEnv,
    _class: JClass,
    timespan: jint,
    prefix: JString,
) -> jobjectArray {
    let prefix = req_string(&mut env, prefix);
    strings_to_array(&mut env, Xlog::make_logfile_name(timespan, &prefix))
}

#[no_mangle]
/// Flush logs once and return a `FileIoAction` code.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeOneshotFlush(
    mut env: JNIEnv,
    _class: JClass,
    log_dir: JString,
    name_prefix: JString,
    pub_key: JString,
    cache_dir: JString,
    cache_days: jint,
    mode: jint,
    compress_mode: jint,
    compress_level: jint,
) -> jint {
    let log_dir = req_string(&mut env, log_dir);
    let name_prefix = req_string(&mut env, name_prefix);
    let pub_key = opt_string(&mut env, pub_key);
    let cache_dir = opt_string(&mut env, cache_dir);

    let mut cfg = XlogConfig::new(log_dir, name_prefix)
        .cache_days(cache_days)
        .mode(to_appender_mode(mode))
        .compress_mode(to_compress_mode(compress_mode))
        .compress_level(compress_level);
    if let Some(key) = pub_key {
        if !key.is_empty() {
            cfg = cfg.pub_key(key);
        }
    }
    if let Some(dir) = cache_dir {
        if !dir.is_empty() {
            cfg = cfg.cache_dir(dir);
        }
    }

    match Xlog::oneshot_flush(cfg) {
        Ok(action) => match action {
            FileIoAction::None => 0,
            FileIoAction::Success => 1,
            FileIoAction::Unnecessary => 2,
            FileIoAction::OpenFailed => 3,
            FileIoAction::ReadFailed => 4,
            FileIoAction::WriteFailed => 5,
            FileIoAction::CloseFailed => 6,
            FileIoAction::RemoveFailed => 7,
        },
        Err(_) => -1,
    }
}

#[no_mangle]
/// Convert a binary log buffer into text using Mars decoding.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeDump(
    mut env: JNIEnv,
    _class: JClass,
    buffer: jbyteArray,
) -> jstring {
    let bytes = bytes_from_array(&mut env, buffer);
    to_jstring(&mut env, Some(Xlog::dump(&bytes)))
}

#[no_mangle]
/// Convert a binary log buffer into text using in-memory decoding.
pub extern "system" fn Java_com_tencent_mars_xlog_example_XlogBridge_nativeMemoryDump(
    mut env: JNIEnv,
    _class: JClass,
    buffer: jbyteArray,
) -> jstring {
    let bytes = bytes_from_array(&mut env, buffer);
    to_jstring(&mut env, Some(Xlog::memory_dump(&bytes)))
}
