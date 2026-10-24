use std::cell::RefCell;
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};

use crate::platform_tid::{current_tid, main_tid};
use crate::record::{LogLevel, LogRecord};

/// Borrowed fields for one Mars-formatted line.
///
/// This is the formatter input, not a log event callers construct.
pub struct MarsFields<'a> {
    /// Severity rendered as a single letter.
    pub level: LogLevel,
    /// Tag written as-is.
    pub tag: &'a str,
    /// Source path. Only the last path component is written.
    pub file: &'a str,
    /// Function name. An empty string stays empty.
    pub func_name: &'a str,
    /// Source line, printed as a signed integer.
    pub line: i32,
    /// Timestamp rendered by [`write_time`].
    pub timestamp: SystemTime,
    /// Process id. `None` is filled from the current process.
    pub pid: Option<i64>,
    /// Thread id. `None` is filled from the current thread.
    pub tid: Option<i64>,
    /// Main thread id. `None` is filled from the process main thread.
    pub maintid: Option<i64>,
    /// Message body. Truncation applies only inside [`write_mars_line`].
    pub body: &'a str,
}

/// Keep parity with Mars formatter's body cap behavior.
const MAX_LOG_BODY_BYTES: usize = 0xFFFF;
const LEGACY_STACK_BUFFER_BYTES: usize = 16 * 1024;
const LEGACY_BODY_RESERVED_BYTES: usize = 130;

#[derive(Default)]
struct TimePrefixCache {
    epoch_second: i64,
    prefix: String,
    valid: bool,
}

thread_local! {
    static TIME_PREFIX_CACHE: RefCell<TimePrefixCache> = const { RefCell::new(TimePrefixCache {
        epoch_second: 0,
        prefix: String::new(),
        valid: false,
    }) };
}

/// Returns the last path component from a full source path.
pub fn extract_file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn truncate_utf8_to_max_bytes(input: &str, max_bytes: usize) -> &str {
    if input.len() <= max_bytes {
        return input;
    }

    let mut end = 0usize;
    for (idx, ch) in input.char_indices() {
        let next = idx + ch.len_utf8();
        if next > max_bytes {
            break;
        }
        end = next;
    }

    &input[..end]
}

/// Append the Mars timestamp for `timestamp` into `out`.
///
/// The same-second prefix is cached on the current thread. Times before the
/// Unix epoch use the uncached fallback.
pub fn write_time(out: &mut String, timestamp: SystemTime) {
    let ts = timestamp;
    if let Ok(since_epoch) = ts.duration_since(UNIX_EPOCH) {
        let epoch_secs = since_epoch.as_secs();
        if epoch_secs <= i64::MAX as u64 {
            let epoch_second = epoch_secs as i64;
            let millis = since_epoch.subsec_millis();
            if append_cached_time_prefix(out, epoch_second) {
                let _ = write!(out, "{millis:03}");
                return;
            }
        }
    }

    let dt: DateTime<Local> = ts.into();
    let offset_hours = (dt.offset().local_minus_utc() as f64) / 3600.0;
    let _ = write!(
        out,
        "{:04}-{:02}-{:02} {:+.1} {:02}:{:02}:{:02}.{:03}",
        dt.year(),
        dt.month(),
        dt.day(),
        offset_hours,
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.timestamp_subsec_millis()
    );
}

fn append_cached_time_prefix(out: &mut String, epoch_second: i64) -> bool {
    TIME_PREFIX_CACHE.with(|cache_cell| {
        let mut cache = cache_cell.borrow_mut();
        if !cache.valid || cache.epoch_second != epoch_second {
            let Some(dt) = Local.timestamp_opt(epoch_second, 0).single() else {
                return false;
            };
            cache.prefix.clear();
            let offset_hours = (dt.offset().local_minus_utc() as f64) / 3600.0;
            let _ = write!(
                cache.prefix,
                "{:04}-{:02}-{:02} {:+.1} {:02}:{:02}:{:02}.",
                dt.year(),
                dt.month(),
                dt.day(),
                offset_hours,
                dt.hour(),
                dt.minute(),
                dt.second()
            );
            cache.epoch_second = epoch_second;
            cache.valid = true;
        }
        out.push_str(&cache.prefix);
        true
    })
}

/// Formats a record into the reusable output buffer.
pub fn format_record_into(out: &mut String, record: &LogRecord, body: &str) {
    format_record_parts_into(
        out,
        record.level,
        &record.tag,
        &record.filename,
        &record.func_name,
        record.line,
        record.timestamp,
        record.pid,
        record.tid,
        record.maintid,
        body,
    );
}

fn resolve_id(id: Option<i64>, runtime: i64) -> i64 {
    id.unwrap_or(runtime)
}

/// Resolve missing pid, tid, and main tid from the current process.
///
/// `None` means "fill from the runtime". `Some` is written as that integer,
/// including `-1`.
pub fn resolve_runtime_ids(
    pid: Option<i64>,
    tid: Option<i64>,
    maintid: Option<i64>,
) -> (i64, i64, i64) {
    (
        resolve_id(pid, std::process::id() as i64),
        resolve_id(tid, current_tid()),
        resolve_id(maintid, main_tid()),
    )
}

/// Write one Mars-compatible log line into `out`, replacing its contents.
pub fn write_mars_line(out: &mut String, fields: &MarsFields<'_>) {
    out.clear();
    let filename = extract_file_name(fields.file);
    let (pid, tid, maintid) = resolve_runtime_ids(fields.pid, fields.tid, fields.maintid);
    let tid_suffix = if tid == maintid { "*" } else { "" };
    let func_name = if fields.func_name.is_empty() {
        ""
    } else {
        fields.func_name
    };

    out.push('[');
    out.push_str(fields.level.short());
    out.push_str("][");
    write_time(out, fields.timestamp);
    let _ = write!(
        out,
        "][{pid}, {tid}{tid_suffix}][{}][{filename}:{line}, {func_name}][",
        fields.tag,
        line = fields.line,
    );

    let body_cap = LEGACY_STACK_BUFFER_BYTES
        .saturating_sub(out.len())
        .saturating_sub(LEGACY_BODY_RESERVED_BYTES)
        .min(MAX_LOG_BODY_BYTES);
    let body = truncate_utf8_to_max_bytes(fields.body, body_cap);
    out.push_str(body);
    if !out.ends_with('\n') && out.len() < LEGACY_STACK_BUFFER_BYTES {
        out.push('\n');
    }
    if out.len() > LEGACY_STACK_BUFFER_BYTES {
        out.truncate(truncate_utf8_to_max_bytes(out, LEGACY_STACK_BUFFER_BYTES).len());
    }
}

/// Formats a record from its individual fields into the reusable output buffer.
///
/// The output matches the legacy xlog single-line text formatter and truncates the
/// body to the legacy size limits without splitting UTF-8 code points.
#[allow(clippy::too_many_arguments)]
pub fn format_record_parts_into(
    out: &mut String,
    level: crate::record::LogLevel,
    tag: &str,
    filename: &str,
    func_name: &str,
    line: i32,
    timestamp: std::time::SystemTime,
    pid: Option<i64>,
    tid: Option<i64>,
    maintid: Option<i64>,
    body: &str,
) {
    write_mars_line(
        out,
        &MarsFields {
            level,
            tag,
            file: filename,
            func_name,
            line,
            timestamp,
            pid,
            tid,
            maintid,
            body,
        },
    );
}

/// Reproduce C++ `formater.cc` output layout as one text line.
pub fn format_record(record: &LogRecord, body: &str) -> String {
    let mut out = String::with_capacity(LEGACY_STACK_BUFFER_BYTES);
    format_record_into(&mut out, record, body);
    out
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use chrono::{DateTime, Datelike, Local, Timelike};

    use super::format_record;
    use super::write_mars_line;
    use super::write_time;
    use super::MarsFields;
    use crate::record::{LogLevel, LogRecord};

    fn format_time_reference(ts: SystemTime) -> String {
        let dt: DateTime<Local> = ts.into();
        let offset_hours = (dt.offset().local_minus_utc() as f64) / 3600.0;
        format!(
            "{:04}-{:02}-{:02} {:+.1} {:02}:{:02}:{:02}.{:03}",
            dt.year(),
            dt.month(),
            dt.day(),
            offset_hours,
            dt.hour(),
            dt.minute(),
            dt.second(),
            dt.timestamp_subsec_millis()
        )
    }

    #[test]
    fn format_includes_expected_fields() {
        let record = LogRecord {
            level: LogLevel::Error,
            tag: "core".to_string(),
            filename: "/a/b/c.rs".to_string(),
            func_name: "module::f".to_string(),
            line: 42,
            timestamp: UNIX_EPOCH + Duration::from_secs(1_700_000_000) + Duration::from_millis(123),
            pid: Some(12),
            tid: Some(34),
            maintid: Some(34),
        };

        let line = format_record(&record, "msg");
        assert!(line.starts_with("[E]["));
        assert!(line.contains("[12, 34*]"));
        assert!(line.contains("[core]"));
        assert!(line.contains("[c.rs:42, module::f]"));
        assert!(line.ends_with("msg\n"));
    }

    #[test]
    fn format_truncates_oversized_body_on_utf8_boundary() {
        let record = LogRecord::default();
        let body = "好".repeat(40_000); // 120_000 bytes, exceeds cap

        let line = format_record(&record, &body);
        assert!(line.len() <= super::LEGACY_STACK_BUFFER_BYTES);
        assert!(std::str::from_utf8(line.as_bytes()).is_ok());
    }

    #[test]
    fn format_time_matches_reference_for_cached_and_fallback_paths() {
        let after_epoch =
            UNIX_EPOCH + Duration::from_secs(1_700_000_000) + Duration::from_millis(123);
        let mut actual = String::new();
        write_time(&mut actual, after_epoch);
        assert_eq!(actual, format_time_reference(after_epoch));

        let mut actual_next = String::new();
        write_time(&mut actual_next, after_epoch + Duration::from_millis(2));
        assert_eq!(
            actual_next,
            format_time_reference(after_epoch + Duration::from_millis(2))
        );

        if let Some(before_epoch) = UNIX_EPOCH.checked_sub(Duration::from_secs(2)) {
            let mut before_actual = String::new();
            write_time(
                &mut before_actual,
                before_epoch + Duration::from_millis(456),
            );
            assert_eq!(
                before_actual,
                format_time_reference(before_epoch + Duration::from_millis(456))
            );
        }
    }

    #[test]
    fn mars_line_matches_legacy_segments() {
        let timestamp =
            UNIX_EPOCH + Duration::from_secs(1_700_000_000) + Duration::from_millis(123);
        let mut time = String::new();
        write_time(&mut time, timestamp);
        let mut out = String::new();
        write_mars_line(
            &mut out,
            &MarsFields {
                level: LogLevel::Error,
                tag: "core",
                file: "/a/b/c.rs",
                func_name: "module::f",
                line: 42,
                timestamp,
                pid: Some(12),
                tid: Some(34),
                maintid: Some(34),
                body: "msg",
            },
        );
        assert_eq!(
            out,
            format!("[E][{time}][12, 34*][core][c.rs:42, module::f][msg\n")
        );

        let huge = "好".repeat(40_000);
        write_mars_line(
            &mut out,
            &MarsFields {
                level: LogLevel::Info,
                tag: "core",
                file: "c.rs",
                func_name: "",
                line: 1,
                timestamp,
                pid: Some(1),
                tid: Some(1),
                maintid: Some(1),
                body: &huge,
            },
        );
        assert!(out.len() <= super::LEGACY_STACK_BUFFER_BYTES);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }
}
