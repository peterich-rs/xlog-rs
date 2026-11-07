//! Ownership catalog for the extensible path.
//!
//! The catalog is the only authority for which files a namespace owns. File
//! names are not parsed back, and a directory is never scanned by extension to
//! reconstruct one. A catalog that fails verification yields
//! [`ExtensibleError::RecoveryRequired`] and leaves every file in place.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::extensible::error::ExtensibleError;

const CATALOG_VERSION: u32 = 1;
const CATALOG_FILE: &str = "catalog.v1";

/// Lifecycle of one file as the catalog records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    /// The name was reserved but the file is not durable yet.
    Creating,
    /// The file is open for appends.
    Active,
    /// The file is sealed and closed.
    Sealed,
}

impl LifecycleState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Active => "active",
            Self::Sealed => "sealed",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "creating" => Some(Self::Creating),
            "active" => Some(Self::Active),
            "sealed" => Some(Self::Sealed),
            _ => None,
        }
    }
}

/// One file the catalog knows about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    /// Stable id of the file.
    pub file_id: u64,
    /// File name relative to the namespace root.
    pub name: String,
    /// Format id the file was written with.
    pub format_id: String,
    /// Format version.
    pub format_version: u16,
    /// Rotation sequence.
    pub sequence: u64,
    /// Creation time as seconds since the Unix epoch.
    pub created_unix_secs: u64,
    /// Lifecycle state.
    pub state: LifecycleState,
}

/// The verified set of files owned by one namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCatalog {
    namespace: String,
    next_file_id: u64,
    entries: Vec<CatalogEntry>,
}

impl FileCatalog {
    /// Empty catalog for a namespace that has no catalog on disk yet.
    pub fn empty(namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            next_file_id: 1,
            entries: Vec::new(),
        }
    }

    /// Namespace this catalog belongs to.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Borrow the recorded files.
    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }

    /// Record a file the caller has already assigned an id to.
    ///
    /// The next id handed out stays strictly above every recorded id, so a
    /// catalog reloaded from disk never reuses one.
    pub fn reserve(&mut self, entry: CatalogEntry) -> Result<(), ExtensibleError> {
        if entry.file_id == 0
            || self
                .entries
                .iter()
                .any(|known| known.file_id == entry.file_id)
        {
            return Err(ExtensibleError::Config(format!(
                "file id {} is unset or already recorded",
                entry.file_id
            )));
        }
        self.next_file_id = self.next_file_id.max(entry.file_id.saturating_add(1));
        self.entries.push(entry);
        Ok(())
    }

    /// Next file id that will be recorded.
    pub fn next_file_id(&self) -> u64 {
        self.next_file_id
    }

    /// Move a file to a new lifecycle state.
    pub fn set_state(
        &mut self,
        file_id: u64,
        state: LifecycleState,
    ) -> Result<(), ExtensibleError> {
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| entry.file_id == file_id);
        match entry {
            Some(entry) => {
                entry.state = state;
                Ok(())
            }
            None => Err(ExtensibleError::RecoveryRequired {
                namespace: self.namespace.clone(),
                reason: format!("no catalog entry for file id {file_id}"),
            }),
        }
    }

    /// Read and verify the catalog at `root`.
    ///
    /// A missing catalog in an empty directory is a fresh namespace. A missing
    /// catalog in a directory that already holds files, or a catalog that
    /// fails verification, is [`ExtensibleError::RecoveryRequired`].
    pub fn load(root: &Path, namespace: &str) -> Result<Self, ExtensibleError> {
        let path = catalog_path(root);
        if !path.exists() {
            let foreign = foreign_entries(root)?;
            if foreign.is_empty() {
                return Ok(Self::empty(namespace));
            }
            return Err(ExtensibleError::RecoveryRequired {
                namespace: namespace.to_owned(),
                reason: format!(
                    "no catalog but {} unrecognized file(s) are present",
                    foreign.len()
                ),
            });
        }
        let text = read_to_string(&path)?;
        parse_catalog(&text, namespace).map_err(|reason| ExtensibleError::RecoveryRequired {
            namespace: namespace.to_owned(),
            reason,
        })
    }

    /// Write the catalog atomically and sync it.
    ///
    /// The previous catalog is left untouched until the new bytes and the
    /// directory entry are both durable.
    pub fn store(&self, root: &Path) -> Result<(), ExtensibleError> {
        let path = catalog_path(root);
        let tmp = root.join(format!("{CATALOG_FILE}.tmp"));
        let bytes = self.render();
        {
            let mut file = File::create(&tmp).map_err(|e| ExtensibleError::storage(&tmp, e))?;
            file.write_all(&bytes)
                .map_err(|e| ExtensibleError::storage(&tmp, e))?;
            file.sync_all()
                .map_err(|e| ExtensibleError::storage(&tmp, e))?;
        }
        fs::rename(&tmp, &path).map_err(|e| ExtensibleError::storage(&path, e))?;
        sync_dir(root)?;
        Ok(())
    }

    /// Names recorded in the catalog.
    pub fn owned_names(&self) -> Vec<&str> {
        self.entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect()
    }

    fn render(&self) -> Vec<u8> {
        let mut out = format!("version {CATALOG_VERSION}\nnamespace {}\n", self.namespace);
        for entry in &self.entries {
            let _ = std::fmt::Write::write_fmt(
                &mut out,
                format_args!(
                    "file {} {} {} {} {} {} {}\n",
                    entry.file_id,
                    entry.format_id,
                    entry.format_version,
                    entry.sequence,
                    entry.created_unix_secs,
                    entry.state.as_str(),
                    entry.name
                ),
            );
        }
        out.into_bytes()
    }
}

fn catalog_path(root: &Path) -> PathBuf {
    root.join(CATALOG_FILE)
}

fn parse_catalog(text: &str, expected_namespace: &str) -> Result<FileCatalog, String> {
    let mut lines = text.lines();
    let version = lines
        .next()
        .and_then(|line| line.strip_prefix("version "))
        .ok_or_else(|| "catalog has no version header".to_owned())?;
    if version != CATALOG_VERSION.to_string() {
        return Err(format!("unsupported catalog version {version}"));
    }
    let namespace = lines
        .next()
        .and_then(|line| line.strip_prefix("namespace "))
        .ok_or_else(|| "catalog has no namespace header".to_owned())?;
    if namespace != expected_namespace {
        return Err(format!(
            "catalog namespace `{namespace}` does not match `{expected_namespace}`"
        ));
    }
    let mut catalog = FileCatalog::empty(namespace);
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(8, ' ');
        let tag = parts.next().unwrap_or_default();
        if tag != "file" {
            return Err(format!("unknown catalog record `{tag}`"));
        }
        let file_id = parse_field(parts.next(), "file id")?;
        let format_id = parts
            .next()
            .ok_or_else(|| "catalog entry has no format id".to_owned())?
            .to_owned();
        let format_version = parse_field(parts.next(), "format version")?;
        let sequence = parse_field(parts.next(), "sequence")?;
        let created_unix_secs = parse_field(parts.next(), "created")?;
        let state = parts
            .next()
            .and_then(LifecycleState::parse)
            .ok_or_else(|| "catalog entry has an unknown lifecycle".to_owned())?;
        let name = parts
            .next()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| "catalog entry has no file name".to_owned())?
            .to_owned();
        catalog
            .reserve(CatalogEntry {
                file_id,
                name,
                format_id,
                format_version,
                sequence,
                created_unix_secs,
                state,
            })
            .map_err(|error| error.to_string())?;
    }
    Ok(catalog)
}

fn parse_field<T: std::str::FromStr>(value: Option<&str>, what: &str) -> Result<T, String> {
    value
        .ok_or_else(|| format!("catalog entry has no {what}"))?
        .parse()
        .map_err(|_| format!("catalog entry has an invalid {what}"))
}

fn foreign_entries(root: &Path) -> Result<Vec<PathBuf>, ExtensibleError> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut foreign = Vec::new();
    for entry in fs::read_dir(root).map_err(|e| ExtensibleError::storage(root, e))? {
        let entry = entry.map_err(|e| ExtensibleError::storage(root, e))?;
        foreign.push(entry.path());
    }
    Ok(foreign)
}

fn read_to_string(path: &Path) -> Result<String, ExtensibleError> {
    let mut file = File::open(path).map_err(|e| ExtensibleError::storage(path, e))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)
        .map_err(|e| ExtensibleError::storage(path, e))?;
    Ok(buf)
}

fn sync_dir(dir: &Path) -> Result<(), ExtensibleError> {
    let file = File::open(dir).map_err(|e| ExtensibleError::storage(dir, e))?;
    file.sync_all()
        .map_err(|e| ExtensibleError::storage(dir, e))
}
