use std::fmt::Write as _;

use chrono::{DateTime, Datelike, Local, Timelike};

use crate::record::LogRecord;

/// Keep parity with Mars formatter's body cap behavior.
const MAX_LOG_BODY_BYTES: usize = 0xFFFF;
const LEGACY_STACK_BUFFER_BYTES: usize = 16 * 1024;
const LEGACY_BODY_RESERVED_BYTES: usize = 130;

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

fn format_time_into(out: &mut String, ts: std::time::SystemTime) {
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

pub fn format_record_into(
    out: &mut String,
    record: &LogRecord,
    body: &str,
) {
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

pub fn format_record_parts_into(
    out: &mut String,
    level: crate::record::LogLevel,
    tag: &str,
    filename: &str,
    func_name: &str,
    line: i32,
    timestamp: std::time::SystemTime,
    pid: i64,
    tid: i64,
    maintid: i64,
    body: &str,
) {
    out.clear();
    let filename = extract_file_name(filename);
    let tid_suffix = if tid == maintid {
        "*"
    } else {
        ""
    };
    let func_name = if func_name.is_empty() {
        ""
    } else {
        func_name
    };

    out.push('[');
    out.push_str(level.short());
    out.push_str("][");
    format_time_into(out, timestamp);
    let _ = write!(
        out,
        "][{}, {}{}][{}][{}:{}, {}][",
        pid,
        tid,
        tid_suffix,
        tag,
        filename,
        line,
        func_name
    );

    let body_cap = LEGACY_STACK_BUFFER_BYTES
        .saturating_sub(out.len())
        .saturating_sub(LEGACY_BODY_RESERVED_BYTES)
        .min(MAX_LOG_BODY_BYTES);
    let body = truncate_utf8_to_max_bytes(body, body_cap);
    out.push_str(body);
    if !out.ends_with('\n') && out.len() < LEGACY_STACK_BUFFER_BYTES {
        out.push('\n');
    }
    if out.len() > LEGACY_STACK_BUFFER_BYTES {
        out.truncate(truncate_utf8_to_max_bytes(out, LEGACY_STACK_BUFFER_BYTES).len());
    }
}

/// Reproduce C++ `formater.cc` output layout as one text line.
pub fn format_record(record: &LogRecord, body: &str) -> String {
    let mut out = String::with_capacity(LEGACY_STACK_BUFFER_BYTES);
    format_record_into(&mut out, record, body);
    out
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::format_record;
    use crate::record::{LogLevel, LogRecord};

    #[test]
    fn format_includes_expected_fields() {
        let record = LogRecord {
            level: LogLevel::Error,
            tag: "core".to_string(),
            filename: "/a/b/c.rs".to_string(),
            func_name: "module::f".to_string(),
            line: 42,
            timestamp: UNIX_EPOCH + Duration::from_secs(1_700_000_000) + Duration::from_millis(123),
            pid: 12,
            tid: 34,
            maintid: 34,
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
}
