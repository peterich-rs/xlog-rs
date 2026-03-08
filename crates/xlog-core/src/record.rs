use std::time::{SystemTime, UNIX_EPOCH};

/// Log severity used by the Rust core.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LogLevel {
    /// Verbose diagnostic output.
    Verbose,
    /// Debug output for development and troubleshooting.
    Debug,
    /// Informational output for normal events.
    Info,
    /// Warning output for recoverable issues.
    Warn,
    /// Error output for failures that do not immediately abort the process.
    Error,
    /// Fatal output for unrecoverable failures.
    Fatal,
    /// Logging disabled.
    None,
}

impl LogLevel {
    /// Return the single-letter level tag used by the formatter.
    pub fn short(self) -> &'static str {
        match self {
            LogLevel::Verbose => "V",
            LogLevel::Debug => "D",
            LogLevel::Info => "I",
            LogLevel::Warn => "W",
            LogLevel::Error => "E",
            LogLevel::Fatal => "F",
            LogLevel::None => "N",
        }
    }
}

/// Rust-native representation of a log entry metadata block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    /// Log severity.
    pub level: LogLevel,
    /// User tag/category.
    pub tag: String,
    /// Source filename if known.
    pub filename: String,
    /// Function or symbol name if known.
    pub func_name: String,
    /// Source line number.
    pub line: i32,
    /// Timestamp associated with the record.
    pub timestamp: SystemTime,
    /// Process id override, or `-1` to fill at runtime.
    pub pid: i64,
    /// Thread id override, or `-1` to fill at runtime.
    pub tid: i64,
    /// Main thread id override, or `-1` to fill at runtime.
    pub maintid: i64,
}

impl Default for LogRecord {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            tag: String::new(),
            filename: String::new(),
            func_name: String::new(),
            line: 0,
            timestamp: UNIX_EPOCH,
            pid: -1,
            tid: -1,
            maintid: -1,
        }
    }
}

impl LogRecord {
    /// Create a record with the current time and default metadata placeholders.
    pub fn now(level: LogLevel, tag: impl Into<String>) -> Self {
        Self {
            level,
            tag: tag.into(),
            timestamp: SystemTime::now(),
            ..Self::default()
        }
    }
}
