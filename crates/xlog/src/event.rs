use std::time::SystemTime;

use crate::LogLevel;

/// One log call's borrowed fields.
///
/// The borrow ends when formatting returns. Async workers never see an `Event`.
#[derive(Clone, Copy)]
pub struct Event<'a> {
    /// Severity.
    pub level: LogLevel,
    /// Tag. An empty string stays empty; name-prefix fallback happens before this is built.
    pub tag: &'a str,
    /// Source path. Empty when the caller did not supply one.
    pub file: &'a str,
    /// Function name. Empty when the caller did not supply one.
    pub func: &'a str,
    /// Source line. Zero when the caller did not supply one.
    pub line: u32,
    /// Message body.
    pub message: &'a str,
    /// Process id override. `None` is filled at format time.
    pub pid: Option<i64>,
    /// Thread id override. `None` is filled at format time.
    pub tid: Option<i64>,
    /// Main thread id override. `None` is filled at format time.
    pub maintid: Option<i64>,
    /// Timestamp used for this line. Callers that care set it before `write_event`.
    pub timestamp: SystemTime,
}

impl<'a> Event<'a> {
    /// Build an event with empty location, runtime ids, and the current time.
    pub fn new(level: LogLevel, tag: &'a str, message: &'a str) -> Self {
        Self {
            level,
            tag,
            file: "",
            func: "",
            line: 0,
            message,
            pid: None,
            tid: None,
            maintid: None,
            timestamp: SystemTime::now(),
        }
    }
}
