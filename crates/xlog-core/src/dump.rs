use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use chrono::{Datelike, Local, Timelike};

const MAX_DUMP_LENGTH: usize = 4096;

pub fn memory_dump(buffer: &[u8]) -> String {
    if buffer.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push('\n');
    out.push_str(&format!("{} bytes:\n", buffer.len()));

    let mut offset = 0usize;
    while offset < buffer.len() && out.len() < MAX_DUMP_LENGTH {
        let mut bytes = std::cmp::min(32, buffer.len() - offset);
        let dst_left = MAX_DUMP_LENGTH.saturating_sub(out.len());
        while bytes > 0 && calc_dump_required_length(bytes) >= dst_left {
            bytes -= 1;
        }
        if bytes == 0 {
            break;
        }

        append_hex_ascii(&mut out, &buffer[offset..offset + bytes]);
        out.push('\n');
        offset += bytes;
    }

    out
}

pub fn dump_to_file(log_dir: &str, buffer: &[u8]) -> String {
    if log_dir.is_empty() || buffer.is_empty() {
        return String::new();
    }

    let now = Local::now();
    let day_dir_name = format!("{:04}{:02}{:02}", now.year(), now.month(), now.day());
    let day_dir = Path::new(log_dir).join(day_dir_name);
    if fs::create_dir_all(&day_dir).is_err() {
        return String::new();
    }

    let file_name = format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}_{}.dump",
        now.year(),
        now.month(),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        buffer.len()
    );
    let path = day_dir.join(file_name);

    let Ok(mut file) = File::create(&path) else {
        return String::new();
    };
    if file.write_all(buffer).is_err() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str("\n dump file to ");
    out.push_str(&path.to_string_lossy());
    out.push_str(" :\n");

    let mut offset = 0usize;
    for _ in 0..32 {
        if offset >= buffer.len() {
            break;
        }
        let end = std::cmp::min(offset + 16, buffer.len());
        append_hex_ascii(&mut out, &buffer[offset..end]);
        out.push('\n');
        offset = end;
    }

    out
}

fn calc_dump_required_length(src_bytes: usize) -> usize {
    src_bytes * 6 + 1
}

fn append_hex_ascii(out: &mut String, bytes: &[u8]) {
    for b in bytes {
        let _ = write!(out, "{:02x} ", b);
    }
    out.push('\n');
    for b in bytes {
        let c = if (*b as char).is_ascii_graphic() {
            *b as char
        } else {
            ' '
        };
        out.push(c);
        out.push_str("  ");
    }
}
