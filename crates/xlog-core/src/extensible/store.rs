//! File store for the extensible path.
//!
//! The store owns the namespace directory, the catalog, the journal, and the
//! one open file. It is the only thing that creates, appends to, or deletes
//! files. A format encoder and an encryptor never see a path.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::extensible::catalog::{CatalogEntry, FileCatalog, LifecycleState};
use crate::extensible::crypto::Encryptor;
use crate::extensible::error::ExtensibleError;
use crate::extensible::format::{FileContext, FileFormat, FormatEncoder};
use crate::extensible::journal::{Journal, JournalRecord};
use crate::extensible::naming::{FileIdentity, FileNaming, RelativeFileName};
use crate::extensible::rotation::{RotationDecision, RotationPolicy};

/// Configuration fixed when a store is opened.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Directory owned by this namespace. Nothing else should write here.
    pub root: PathBuf,
    /// Namespace recorded in the catalog and used for naming.
    pub namespace: String,
    /// When to seal the active file and start another.
    pub rotation: RotationPolicy,
    /// How long sealed files are kept, in seconds. Zero disables deletion.
    pub retention_secs: u64,
}

/// What a maintenance pass did and deliberately did not do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MaintenanceReport {
    /// Files deleted because they were sealed, owned, and past retention.
    pub deleted: Vec<String>,
    /// Files present in the directory but absent from the catalog.
    ///
    /// These are kept. Retention does not apply to them, and they are never
    /// deleted on the strength of a suffix or a modification time.
    pub unowned: Vec<String>,
}

/// Result of replaying a journal at open.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecoveryReport {
    /// Segments copied onto their target file.
    pub replayed: usize,
    /// Segments already present on their target file and skipped.
    pub already_durable: usize,
}

/// One open namespace.
pub struct ExtensibleStore {
    config: StoreConfig,
    catalog: FileCatalog,
    journal: Journal,
    naming: Box<dyn FileNaming>,
    format: Box<dyn FileFormat>,
    encoder: Option<Box<dyn FormatEncoder>>,
    active_path: Option<PathBuf>,
    active_file_id: Option<u64>,
    active_len: u64,
    opened_unix_secs: u64,
    suffix: String,
    closed: bool,
}

impl ExtensibleStore {
    /// Open a namespace, replaying any journal left by a previous process.
    ///
    /// Fails without creating a file when the catalog is missing from a
    /// non-empty directory, fails verification, or names another namespace.
    pub fn open(
        config: StoreConfig,
        format: Box<dyn FileFormat>,
        encryptor: &dyn Encryptor,
        naming: Box<dyn FileNaming>,
    ) -> Result<(Self, RecoveryReport), ExtensibleError> {
        validate_namespace(&config.namespace)?;
        format.validate(encryptor)?;
        fs::create_dir_all(&config.root).map_err(|e| ExtensibleError::storage(&config.root, e))?;
        let catalog = FileCatalog::load(&config.root, &config.namespace)?;
        let journal = Journal::open(&config.root)?;
        let mut store = Self {
            suffix: format.descriptor().suffix.to_owned(),
            config,
            catalog,
            journal,
            naming,
            format,
            encoder: None,
            active_path: None,
            active_file_id: None,
            active_len: 0,
            opened_unix_secs: 0,
            closed: false,
        };
        let report = store.replay()?;
        Ok((store, report))
    }

    /// Append one record and make it recoverable.
    ///
    /// Returns once the sealed segment is in the journal and on the target
    /// file, both synced. A crash before that loses the record; a crash after
    /// the journal sync but before the file sync is repaired on the next open.
    pub fn write(
        &mut self,
        record: &[u8],
        encryptor: &mut dyn Encryptor,
    ) -> Result<(), ExtensibleError> {
        self.ensure_open()?;
        let now = unix_now();
        if self.encoder.is_none() || self.needs_rotation(record.len(), now) {
            self.rotate(encryptor, now)?;
        }
        let encoder = self.encoder.as_mut().expect("rotate opens an encoder");
        encoder.push_record(record)?;
        self.seal_and_commit(encryptor)
    }

    /// Seal the open segment, sync it, and leave the file closed.
    pub fn flush(&mut self, encryptor: &mut dyn Encryptor) -> Result<(), ExtensibleError> {
        self.ensure_open()?;
        if self.encoder.is_some() {
            self.seal_and_commit(encryptor)?;
        }
        Ok(())
    }

    /// Files the catalog currently records.
    pub fn files(&self) -> &[CatalogEntry] {
        self.catalog.entries()
    }

    /// Delete sealed files past retention and report everything else.
    ///
    /// Only names present in a verified catalog are eligible. Files the
    /// catalog does not mention are reported as unowned and kept.
    pub fn maintain(&mut self) -> Result<MaintenanceReport, ExtensibleError> {
        self.ensure_open()?;
        let mut report = MaintenanceReport::default();
        let owned = self.catalog.owned_names();
        for entry in fs::read_dir(&self.config.root)
            .map_err(|e| ExtensibleError::storage(&self.config.root, e))?
        {
            let entry = entry.map_err(|e| ExtensibleError::storage(&self.config.root, e))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if is_bookkeeping(&name) || owned.iter().any(|owned| *owned == name) {
                continue;
            }
            report.unowned.push(name.into_owned());
        }

        if self.config.retention_secs == 0 {
            return Ok(report);
        }
        let now = unix_now();
        let expired: Vec<CatalogEntry> = self
            .catalog
            .entries()
            .iter()
            .filter(|entry| entry.state == LifecycleState::Sealed)
            .filter(|entry| {
                now.saturating_sub(entry.created_unix_secs) > self.config.retention_secs
            })
            .cloned()
            .collect();
        for entry in &expired {
            let path = self.config.root.join(&entry.name);
            fs::remove_file(&path).map_err(|e| ExtensibleError::storage(&path, e))?;
            report.deleted.push(entry.name.clone());
        }
        if !expired.is_empty() {
            let remaining: Vec<CatalogEntry> = self
                .catalog
                .entries()
                .iter()
                .filter(|entry| !expired.iter().any(|gone| gone.file_id == entry.file_id))
                .cloned()
                .collect();
            self.catalog = rebuild_catalog(&self.catalog, remaining);
            self.catalog.store(&self.config.root)?;
        }
        Ok(report)
    }

    /// Seal the active file and refuse further writes.
    pub fn close(&mut self, encryptor: &mut dyn Encryptor) -> Result<(), ExtensibleError> {
        if self.closed {
            return Ok(());
        }
        if self.encoder.is_some() {
            self.seal_and_commit(encryptor)?;
            self.seal_active_file()?;
        }
        self.closed = true;
        Ok(())
    }

    fn replay(&mut self) -> Result<RecoveryReport, ExtensibleError> {
        let records = self.journal.read_all()?;
        let mut report = RecoveryReport::default();
        let mut pending = Vec::new();
        for record in records {
            match replay_record(&self.config.root, &self.catalog, &record)? {
                Replay::Applied => report.replayed += 1,
                Replay::AlreadyDurable => report.already_durable += 1,
                Replay::StillPending => pending.push(record),
            }
        }
        self.journal.retain(&pending)?;
        Ok(report)
    }

    fn needs_rotation(&self, record_len: usize, now: u64) -> bool {
        let len = u64::try_from(record_len).unwrap_or(u64::MAX);
        self.config
            .rotation
            .decide(self.active_len, len, self.opened_unix_secs, now)
            == RotationDecision::Rotate
    }

    fn rotate(&mut self, encryptor: &mut dyn Encryptor, now: u64) -> Result<(), ExtensibleError> {
        if self.encoder.is_some() {
            self.seal_and_commit(encryptor)?;
            self.seal_active_file()?;
        }
        self.open_new_file(now)
    }

    fn open_new_file(&mut self, now: u64) -> Result<(), ExtensibleError> {
        let file_id = self.catalog.next_file_id();
        let identity = FileIdentity {
            namespace: self.config.namespace.clone(),
            file_id,
            sequence: file_id,
            created_unix_secs: now,
            suffix: self.suffix.clone(),
        };
        let name = self.naming.file_name(&identity)?;
        let path = exclusive_create(&self.config.root, &name)?;
        self.catalog.reserve(CatalogEntry {
            file_id,
            name: name.as_str().to_owned(),
            format_id: self.format.descriptor().id.to_owned(),
            format_version: self.format.descriptor().version,
            sequence: file_id,
            created_unix_secs: now,
            state: LifecycleState::Active,
        })?;
        self.catalog.store(&self.config.root)?;

        let context = FileContext {
            file_id,
            namespace: self.config.namespace.clone(),
        };
        self.encoder = Some(self.format.start_file(&context)?);
        self.active_path = Some(path);
        self.active_file_id = Some(file_id);
        self.active_len = 0;
        self.opened_unix_secs = now;
        Ok(())
    }

    fn seal_and_commit(&mut self, encryptor: &mut dyn Encryptor) -> Result<(), ExtensibleError> {
        let encoder = match self.encoder.as_mut() {
            Some(encoder) => encoder,
            None => return Ok(()),
        };
        let mut sealed = Vec::new();
        let info = match encoder.seal_segment(encryptor, &mut sealed)? {
            Some(info) => info,
            None => return Ok(()),
        };
        let file_id = self.active_file_id.expect("an encoder implies a file");
        let record = JournalRecord {
            file_id,
            segment_seq: info.sequence,
            offset: self.active_len,
            bytes: sealed,
        };
        self.journal.append(&record)?;
        let path = self.active_path.clone().expect("an encoder implies a path");
        append_synced(&path, &record.bytes)?;
        self.active_len = self.active_len.saturating_add(record.bytes.len() as u64);
        self.journal.retain(&[])?;
        Ok(())
    }

    fn seal_active_file(&mut self) -> Result<(), ExtensibleError> {
        if let Some(file_id) = self.active_file_id.take() {
            self.catalog.set_state(file_id, LifecycleState::Sealed)?;
            self.catalog.store(&self.config.root)?;
        }
        self.encoder = None;
        self.active_path = None;
        Ok(())
    }

    fn ensure_open(&self) -> Result<(), ExtensibleError> {
        if self.closed {
            Err(ExtensibleError::Closed(self.config.namespace.clone()))
        } else {
            Ok(())
        }
    }
}

/// Create `name` inside `root`, failing if it already exists.
fn exclusive_create(root: &Path, name: &RelativeFileName) -> Result<PathBuf, ExtensibleError> {
    let path = root.join(name.as_str());
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| ExtensibleError::storage(&path, e))?;
    Ok(path)
}

fn append_synced(path: &Path, bytes: &[u8]) -> Result<(), ExtensibleError> {
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| ExtensibleError::storage(path, e))?;
    file.write_all(bytes)
        .map_err(|e| ExtensibleError::storage(path, e))?;
    file.sync_all()
        .map_err(|e| ExtensibleError::storage(path, e))
}

enum Replay {
    Applied,
    AlreadyDurable,
    StillPending,
}

fn replay_record(
    root: &Path,
    catalog: &FileCatalog,
    record: &JournalRecord,
) -> Result<Replay, ExtensibleError> {
    let entry = catalog
        .entries()
        .iter()
        .find(|entry| entry.file_id == record.file_id);
    let Some(entry) = entry else {
        return Ok(Replay::StillPending);
    };
    let path = root.join(&entry.name);
    let current = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    if current == record.offset {
        append_synced(&path, &record.bytes)?;
        Ok(Replay::Applied)
    } else if current >= record.offset + record.bytes.len() as u64 {
        Ok(Replay::AlreadyDurable)
    } else {
        Ok(Replay::StillPending)
    }
}

fn rebuild_catalog(source: &FileCatalog, entries: Vec<CatalogEntry>) -> FileCatalog {
    let mut catalog = FileCatalog::empty(source.namespace());
    for entry in entries {
        catalog
            .reserve(entry)
            .expect("retained entries keep their original ids");
    }
    catalog
}

fn is_bookkeeping(name: &str) -> bool {
    name.starts_with("catalog.v1") || name.starts_with("journal.v1")
}

fn validate_namespace(namespace: &str) -> Result<(), ExtensibleError> {
    if namespace.is_empty()
        || namespace.contains(['/', '\\', '\0', ' '])
        || namespace == "."
        || namespace == ".."
    {
        return Err(ExtensibleError::Config(format!(
            "`{namespace}` is not a usable namespace"
        )));
    }
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}
