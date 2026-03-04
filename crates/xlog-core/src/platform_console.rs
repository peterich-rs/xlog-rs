#[cfg(any(
    target_os = "android",
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos"
))]
use std::ffi::CString;
#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos"
))]
use std::sync::atomic::{AtomicU8, Ordering};

#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos"
))]
use chrono::Local;

use crate::formatter::extract_file_name;
#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos"
))]
use crate::platform_tid::{current_tid, main_tid};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ConsoleLevel {
    Verbose,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
    None,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum AppleConsoleFun {
    Printf = 0,
    NsLog = 1,
    OsLog = 2,
}

#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos"
))]
static APPLE_CONSOLE_FUN: AtomicU8 = AtomicU8::new(AppleConsoleFun::OsLog as u8);

#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos"
))]
pub fn set_apple_console_fun(fun: AppleConsoleFun) {
    APPLE_CONSOLE_FUN.store(fun as u8, Ordering::Relaxed);
}

#[cfg(not(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos"
)))]
pub fn set_apple_console_fun(_fun: AppleConsoleFun) {}

pub fn write_console_line(
    level: ConsoleLevel,
    tag: &str,
    file: &str,
    func: &str,
    line: u32,
    msg: &str,
) {
    if msg.is_empty() {
        return;
    }

    #[cfg(target_os = "android")]
    {
        write_android_line(level, tag, file, func, line, msg);
        return;
    }

    #[cfg(not(target_os = "android"))]
    {
        let file_name = extract_file_name(file);
        let func_name = if func.is_empty() { "" } else { func };

        #[cfg(any(
            target_os = "ios",
            target_os = "macos",
            target_os = "tvos",
            target_os = "watchos"
        ))]
        {
            let mode = APPLE_CONSOLE_FUN.load(Ordering::Relaxed);
            if mode == AppleConsoleFun::OsLog as u8 {
                let c_tag = to_console_cstring(tag);
                let c_file = to_console_cstring(file_name);
                let c_func = to_console_cstring(func_name);
                let c_msg = to_console_cstring(msg);
                unsafe {
                    xlog_core_apple_console_oslog(
                        apple_level(level),
                        c_tag.as_ptr(),
                        c_file.as_ptr(),
                        line as i32,
                        c_func.as_ptr(),
                        c_msg.as_ptr(),
                    );
                }
                return;
            }
            if mode == AppleConsoleFun::NsLog as u8 {
                let text = format!(
                    "[{}][{}][{}:{}, {}][{}",
                    level_short(level),
                    tag,
                    file_name,
                    line,
                    func_name,
                    msg
                );
                let c_line = to_console_cstring(&text);
                unsafe {
                    xlog_core_apple_console_nslog(c_line.as_ptr());
                }
                return;
            }
            let now = Local::now();
            let pid = std::process::id() as i64;
            let tid = current_tid();
            let maintid = main_tid();
            let tid_suffix = if tid == maintid { "*" } else { "" };
            let text = format!(
                "[{}][{}][{}, {}{}][{}][{}:{}, {}][{}",
                level_short(level),
                now.format("%Y-%m-%d %z %H:%M:%S%.3f"),
                pid,
                tid,
                tid_suffix,
                tag,
                file_name,
                line,
                func_name,
                msg
            );
            let c_line = to_console_cstring(&text);
            unsafe {
                xlog_core_apple_console_printf(c_line.as_ptr());
            }
            return;
        }

        #[cfg(not(any(
            target_os = "ios",
            target_os = "macos",
            target_os = "tvos",
            target_os = "watchos"
        )))]
        {
            eprintln!(
                "[{}][{}][{}:{}, {}][{}",
                level_short(level),
                tag,
                file_name,
                line,
                func_name,
                msg
            );
        }
    }
}

fn level_short(level: ConsoleLevel) -> &'static str {
    match level {
        ConsoleLevel::Verbose => "V",
        ConsoleLevel::Debug => "D",
        ConsoleLevel::Info => "I",
        ConsoleLevel::Warn => "W",
        ConsoleLevel::Error => "E",
        ConsoleLevel::Fatal => "F",
        ConsoleLevel::None => "N",
    }
}

#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos"
))]
fn to_console_cstring(s: &str) -> CString {
    let clean = if s.as_bytes().contains(&0) {
        s.replace('\0', " ")
    } else {
        s.to_string()
    };
    CString::new(clean).expect("console string must not contain nul")
}

#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos"
))]
fn apple_level(level: ConsoleLevel) -> i32 {
    match level {
        ConsoleLevel::Verbose => 0,
        ConsoleLevel::Debug => 1,
        ConsoleLevel::Info => 2,
        ConsoleLevel::Warn => 3,
        ConsoleLevel::Error => 4,
        ConsoleLevel::Fatal => 5,
        ConsoleLevel::None => 6,
    }
}

#[cfg(target_os = "android")]
fn write_android_line(
    level: ConsoleLevel,
    tag: &str,
    file: &str,
    func: &str,
    line: u32,
    msg: &str,
) {
    let file_name = extract_file_name(file);
    let func_name = if func.is_empty() { "" } else { func };
    let mut out = format!("[{file_name}:{line}, {func_name}]:{msg}");
    out = out.replace('\0', " ");
    let tag = if tag.is_empty() { "mars-xlog" } else { tag };
    let safe_tag = tag.replace('\0', " ");
    let c_tag = CString::new(safe_tag).expect("nul bytes replaced");
    let c_msg = CString::new(out).expect("nul bytes replaced");
    unsafe {
        __android_log_write(android_priority(level), c_tag.as_ptr(), c_msg.as_ptr());
    }
}

#[cfg(target_os = "android")]
fn android_priority(level: ConsoleLevel) -> i32 {
    match level {
        ConsoleLevel::Verbose => 2, // ANDROID_LOG_VERBOSE
        ConsoleLevel::Debug => 3,   // ANDROID_LOG_DEBUG
        ConsoleLevel::Info => 4,    // ANDROID_LOG_INFO
        ConsoleLevel::Warn => 5,    // ANDROID_LOG_WARN
        ConsoleLevel::Error => 6,   // ANDROID_LOG_ERROR
        ConsoleLevel::Fatal => 7,   // ANDROID_LOG_FATAL
        ConsoleLevel::None => 4,    // ANDROID_LOG_INFO
    }
}

#[cfg(target_os = "android")]
unsafe extern "C" {
    fn __android_log_write(prio: i32, tag: *const libc::c_char, text: *const libc::c_char) -> i32;
}

#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos"
))]
unsafe extern "C" {
    fn xlog_core_apple_console_printf(text: *const libc::c_char);
    fn xlog_core_apple_console_nslog(text: *const libc::c_char);
    fn xlog_core_apple_console_oslog(
        level: i32,
        tag: *const libc::c_char,
        file: *const libc::c_char,
        line: i32,
        func: *const libc::c_char,
        msg: *const libc::c_char,
    );
}
