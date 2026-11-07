//! Opt-in extensible logger.
//!
//! This is a separate entry point from [`crate::Xlog`]. It writes plaintext
//! files under its own directory and records ownership in a catalog, so it
//! never produces Mars bytes and never shares a directory with a Mars logger.
//!
//! The content format and the file naming are chosen at construction and are
//! not yet a stable public contract. The default is JSON Lines with file names
//! that carry no date.

use std::path::PathBuf;
use std::sync::Mutex;

use mars_xlog_core::extensible::{
    write_json_line, ExtensibleError, ExtensibleStore, FileNaming, IdFileNaming, JsonLinesFormat,
    MaintenanceReport, NoEncryption, PlainTextFormat, RotationPolicy, StoreConfig,
    TimestampFileNaming,
};

use crate::layout::{format_line, Layout};
use crate::LogLevel;

/// Which plaintext file content an [`ExtensibleLog`] writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentFormat {
    /// One JSON object per line, under a `.log` suffix.
    JsonLines,
    /// The line layout's bytes copied through, one record per line.
    PlainText,
}

/// How files are named. Neither variant parses the name back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameStyle {
    /// `<namespace>_<file id>.log`, with no date.
    FileId,
    /// `<namespace>_<utc date>_<sequence>.log`. The date is decorative.
    Timestamp,
}

/// When the active file is sealed and a new one opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotate {
    /// Keep a single file.
    Never,
    /// Rotate once a file grows past `max_bytes`.
    AtBytes(u64),
    /// Rotate when the calendar day changes.
    Daily,
}

/// Builder for an [`ExtensibleLog`].
///
/// The logger is opt-in: nothing here changes the behavior of [`crate::Xlog`].
#[derive(Debug, Clone)]
pub struct ExtensibleLogBuilder {
    root: PathBuf,
    namespace: String,
    format: ContentFormat,
    naming: NameStyle,
    rotation: Rotate,
    retention_secs: u64,
}

impl ExtensibleLogBuilder {
    /// Start a builder rooted at `root` and named `namespace`.
    ///
    /// `namespace` becomes part of every file name and of the catalog, so it
    /// must be a single path component.
    pub fn new(root: impl Into<PathBuf>, namespace: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            namespace: namespace.into(),
            format: ContentFormat::JsonLines,
            naming: NameStyle::FileId,
            rotation: Rotate::Never,
            retention_secs: 0,
        }
    }

    /// Select the file content.
    pub fn format(mut self, format: ContentFormat) -> Self {
        self.format = format;
        self
    }

    /// Select the file naming.
    pub fn naming(mut self, naming: NameStyle) -> Self {
        self.naming = naming;
        self
    }

    /// Select the rotation rule.
    pub fn rotation(mut self, rotation: Rotate) -> Self {
        self.rotation = rotation;
        self
    }

    /// Delete sealed files older than `retention_secs`. Zero disables deletion.
    pub fn retention_secs(mut self, retention_secs: u64) -> Self {
        self.retention_secs = retention_secs;
        self
    }

    /// Create the logger and replay any journal left by an earlier process.
    ///
    /// `layout` formats records for [`ContentFormat::PlainText`]. JSON Lines
    /// uses a fixed schema and ignores it.
    pub fn open(self, layout: Layout) -> Result<ExtensibleLog, ExtensibleError> {
        let rotation = match self.rotation {
            Rotate::Never => RotationPolicy::Never,
            Rotate::AtBytes(max_bytes) => RotationPolicy::Size { max_bytes },
            Rotate::Daily => RotationPolicy::Daily,
        };
        let config = StoreConfig {
            root: self.root,
            namespace: self.namespace,
            rotation,
            retention_secs: self.retention_secs,
        };
        let naming: Box<dyn FileNaming> = match self.naming {
            NameStyle::FileId => Box::new(IdFileNaming),
            NameStyle::Timestamp => Box::new(TimestampFileNaming),
        };
        let (store, _report) = match self.format {
            ContentFormat::JsonLines => {
                ExtensibleStore::open(config, Box::new(JsonLinesFormat), &NoEncryption, naming)?
            }
            ContentFormat::PlainText => {
                ExtensibleStore::open(config, Box::new(PlainTextFormat), &NoEncryption, naming)?
            }
        };
        Ok(ExtensibleLog {
            store: Mutex::new(store),
            layout,
            format: self.format,
        })
    }
}

/// A logger that writes plaintext files under a catalog.
///
/// Writes are serialized and synced. [`ExtensibleLog::log`] returns once the
/// record is durable, not merely queued.
pub struct ExtensibleLog {
    store: Mutex<ExtensibleStore>,
    layout: Layout,
    format: ContentFormat,
}

impl ExtensibleLog {
    /// Append one record.
    ///
    /// The record is encoded, sealed, journaled, and synced before this
    /// returns. An encryption or storage failure is returned, never downgraded
    /// to plaintext.
    pub fn log(&self, level: LogLevel, tag: &str, message: &str) -> Result<(), ExtensibleError> {
        let record = match self.format {
            ContentFormat::JsonLines => {
                let mut line = String::new();
                write_json_line(&mut line, level.as_str(), tag, "", message);
                line.into_bytes()
            }
            ContentFormat::PlainText => {
                let mut line = String::new();
                let event = crate::Event::new(level, tag, message);
                format_line(&self.layout, &mut line, &event);
                line.into_bytes()
            }
        };
        let mut store = self.store.lock().map_err(|_| {
            ExtensibleError::Closed("store lock was poisoned; refusing to continue".to_owned())
        })?;
        let mut encryptor = NoEncryption;
        store.write(&record, &mut encryptor)
    }

    /// Seal and sync the current segment.
    pub fn flush(&self) -> Result<(), ExtensibleError> {
        let mut store = self.store.lock().map_err(|_| {
            ExtensibleError::Closed("store lock was poisoned; refusing to continue".to_owned())
        })?;
        let mut encryptor = NoEncryption;
        store.flush(&mut encryptor)
    }

    /// Apply retention and report files the catalog does not own.
    pub fn maintain(&self) -> Result<MaintenanceReport, ExtensibleError> {
        let mut store = self.store.lock().map_err(|_| {
            ExtensibleError::Closed("store lock was poisoned; refusing to continue".to_owned())
        })?;
        store.maintain()
    }

    /// File names the catalog currently records.
    pub fn file_names(&self) -> Result<Vec<String>, ExtensibleError> {
        let store = self.store.lock().map_err(|_| {
            ExtensibleError::Closed("store lock was poisoned; refusing to continue".to_owned())
        })?;
        Ok(store
            .files()
            .iter()
            .map(|entry| entry.name.clone())
            .collect())
    }
}

impl Drop for ExtensibleLog {
    fn drop(&mut self) {
        if let Ok(mut store) = self.store.lock() {
            let mut encryptor = NoEncryption;
            let _ = store.close(&mut encryptor);
        }
    }
}
