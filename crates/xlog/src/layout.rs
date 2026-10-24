use std::fmt::Write as _;
use std::sync::Arc;

use mars_xlog_core::formatter::{
    extract_file_name, resolve_runtime_ids, write_mars_line, write_time, MarsFields,
};
use mars_xlog_core::record::LogLevel as CoreLogLevel;

use crate::event::Event;

/// One closed segment of a custom line.
///
/// Brackets, commas, and spaces are [`Piece::Literal`] values. This enum does
/// not apply Mars truncation or the trailing newline.
#[derive(Debug, Copy, Clone)]
pub enum Piece {
    /// Static text copied as-is.
    Literal(&'static str),
    /// Single-letter level tag.
    Level,
    /// Mars timestamp, including the thread-local second cache.
    Time,
    /// Decimal process id.
    Pid,
    /// Decimal thread id.
    Tid,
    /// `*` when the thread id equals the main thread id, otherwise empty.
    TidSuffix,
    /// Tag text.
    Tag,
    /// Last path component of the source file.
    FileName,
    /// Decimal source line, as the `u32` stored on the event.
    LineNumber,
    /// Function name.
    Func,
    /// Message body, without the Mars size cap.
    Message,
}

/// Caller-supplied line writer.
///
/// Runs on the logging thread. Do not store references from `event`.
pub trait WriteLine: Send + Sync {
    /// Append one line into `out`. The buffer is cleared before this is called.
    fn write_line(&self, out: &mut String, event: &Event<'_>);
}

/// Line layout chosen when a logger is first created.
pub enum Layout {
    /// Byte-compatible Mars line.
    Mars,
    /// Closed piece list assembled by the caller.
    Pieces(Box<[Piece]>),
    /// Open writer, used only when the caller supplies one at init.
    Custom(Arc<dyn WriteLine>),
}

impl Layout {
    /// Mars layout. This is the default for `Xlog::init` and `appender_open`.
    pub fn mars() -> Self {
        Self::Mars
    }

    /// Build a layout from a piece list. The list is stored once.
    pub fn pieces(pieces: impl Into<Box<[Piece]>>) -> Self {
        Self::Pieces(pieces.into())
    }

    /// Build a layout from a caller-supplied writer.
    pub fn custom(writer: Arc<dyn WriteLine>) -> Self {
        Self::Custom(writer)
    }
}

pub(crate) fn format_line(layout: &Layout, out: &mut String, event: &Event<'_>) {
    out.clear();
    match layout {
        Layout::Mars => {
            let fields = mars_fields(event);
            write_mars_line(out, &fields);
        }
        Layout::Pieces(pieces) => write_pieces(out, pieces, event),
        Layout::Custom(writer) => writer.write_line(out, event),
    }
}

fn mars_fields<'a>(event: &'a Event<'a>) -> MarsFields<'a> {
    MarsFields {
        level: CoreLogLevel::from(event.level),
        tag: event.tag,
        file: event.file,
        func_name: event.func,
        line: event.line as i32,
        timestamp: event.timestamp,
        pid: event.pid,
        tid: event.tid,
        maintid: event.maintid,
        body: event.message,
    }
}

fn write_pieces(out: &mut String, pieces: &[Piece], event: &Event<'_>) {
    let (pid, tid, maintid) = resolve_runtime_ids(event.pid, event.tid, event.maintid);
    let tid_suffix = if tid == maintid { "*" } else { "" };
    for piece in pieces {
        match piece {
            Piece::Literal(text) => out.push_str(text),
            Piece::Level => out.push_str(CoreLogLevel::from(event.level).short()),
            Piece::Time => write_time(out, event.timestamp),
            Piece::Pid => {
                let _ = write!(out, "{pid}");
            }
            Piece::Tid => {
                let _ = write!(out, "{tid}");
            }
            Piece::TidSuffix => out.push_str(tid_suffix),
            Piece::Tag => out.push_str(event.tag),
            Piece::FileName => out.push_str(extract_file_name(event.file)),
            Piece::LineNumber => {
                let _ = write!(out, "{}", event.line);
            }
            Piece::Func => out.push_str(event.func),
            Piece::Message => out.push_str(event.message),
        }
    }
}
