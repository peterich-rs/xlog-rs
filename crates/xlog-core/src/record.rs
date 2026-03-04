use std::time::{SystemTime, UNIX_EPOCH};

/// Log severity used by the Rust core.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LogLevel {
    Verbose,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
    None,
}

impl LogLevel {
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
    pub level: LogLevel,
    pub tag: String,
    pub filename: String,
    pub func_name: String,
    pub line: i32,
    pub timestamp: SystemTime,
    pub pid: i64,
    pub tid: i64,
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
    pub fn now(level: LogLevel, tag: impl Into<String>) -> Self {
        Self {
            level,
            tag: tag.into(),
            timestamp: SystemTime::now(),
            ..Self::default()
        }
    }
}
