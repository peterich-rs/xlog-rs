//! Recovery journal for the extensible path.
//!
//! The journal records segments that have already been encoded and sealed. A
//! replay copies those bytes to their target file; it never re-runs a
//! formatter or an encryptor. The journal file is versioned and distinct from
//! the Mars `.mmap3` buffer.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::extensible::error::ExtensibleError;

const JOURNAL_VERSION: u32 = 1;
const JOURNAL_FILE: &str = "journal.v1";

/// One sealed segment waiting to be confirmed on its target file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRecord {
    /// File the segment belongs to.
    pub file_id: u64,
    /// Segment counter inside the file.
    pub segment_seq: u64,
    /// Byte offset the segment must start at.
    pub offset: u64,
    /// Sealed bytes.
    pub bytes: Vec<u8>,
}

/// The journal file for one namespace.
#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
}

impl Journal {
    /// Open or create the journal under `root`.
    pub fn open(root: &Path) -> Result<Self, ExtensibleError> {
        let path = root.join(JOURNAL_FILE);
        if !path.exists() {
            let mut file = File::create(&path).map_err(|e| ExtensibleError::storage(&path, e))?;
            write_header(&mut file, &path)?;
            file.sync_all()
                .map_err(|e| ExtensibleError::storage(&path, e))?;
        }
        Ok(Self { path })
    }

    /// Append one record and sync the journal.
    ///
    /// The record is recoverable once this returns. Before that, the segment
    /// exists only in memory and a crash loses it.
    pub fn append(&self, record: &JournalRecord) -> Result<(), ExtensibleError> {
        let mut file = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|e| ExtensibleError::storage(&self.path, e))?;
        let mut line = format!(
            "seg {} {} {} ",
            record.file_id, record.segment_seq, record.offset
        );
        for byte in &record.bytes {
            let _ = std::fmt::Write::write_fmt(&mut line, format_args!("{byte:02x}"));
        }
        line.push('\n');
        file.write_all(line.as_bytes())
            .map_err(|e| ExtensibleError::storage(&self.path, e))?;
        file.sync_all()
            .map_err(|e| ExtensibleError::storage(&self.path, e))
    }

    /// Read every record currently in the journal.
    pub fn read_all(&self) -> Result<Vec<JournalRecord>, ExtensibleError> {
        let text = read_to_string(&self.path)?;
        parse_journal(&text).map_err(|reason| ExtensibleError::RecoveryRequired {
            namespace: String::new(),
            reason,
        })
    }

    /// Remove records that have been confirmed on their target file.
    pub fn retain(&self, pending: &[JournalRecord]) -> Result<(), ExtensibleError> {
        let tmp = self.path.with_extension("v1.tmp");
        {
            let mut file = File::create(&tmp).map_err(|e| ExtensibleError::storage(&tmp, e))?;
            write_header(&mut file, &tmp)?;
            for record in pending {
                let mut line = format!(
                    "seg {} {} {} ",
                    record.file_id, record.segment_seq, record.offset
                );
                for byte in &record.bytes {
                    let _ = std::fmt::Write::write_fmt(&mut line, format_args!("{byte:02x}"));
                }
                line.push('\n');
                file.write_all(line.as_bytes())
                    .map_err(|e| ExtensibleError::storage(&tmp, e))?;
            }
            file.sync_all()
                .map_err(|e| ExtensibleError::storage(&tmp, e))?;
        }
        fs::rename(&tmp, &self.path).map_err(|e| ExtensibleError::storage(&self.path, e))?;
        if let Some(dir) = self.path.parent() {
            super::catalog::sync_dir(dir)?;
        }
        Ok(())
    }
}

fn write_header(file: &mut File, path: &Path) -> Result<(), ExtensibleError> {
    file.write_all(format!("version {JOURNAL_VERSION}\n").as_bytes())
        .map_err(|e| ExtensibleError::storage(path, e))
}

fn parse_journal(text: &str) -> Result<Vec<JournalRecord>, String> {
    let mut lines = text.lines();
    let version = lines
        .next()
        .and_then(|line| line.strip_prefix("version "))
        .ok_or_else(|| "journal has no version header".to_owned())?;
    if version != JOURNAL_VERSION.to_string() {
        return Err(format!("unsupported journal version {version}"));
    }
    let mut records = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(5, ' ');
        if parts.next() != Some("seg") {
            return Err("journal contains an unknown record".to_owned());
        }
        let file_id = parse_u64(parts.next(), "file id")?;
        let segment_seq = parse_u64(parts.next(), "segment")?;
        let offset = parse_u64(parts.next(), "offset")?;
        let hex_bytes = parts.next().unwrap_or("");
        let bytes = decode_hex(hex_bytes)?;
        records.push(JournalRecord {
            file_id,
            segment_seq,
            offset,
            bytes,
        });
    }
    Ok(records)
}

fn parse_u64(value: Option<&str>, what: &str) -> Result<u64, String> {
    value
        .ok_or_else(|| format!("journal record has no {what}"))?
        .parse()
        .map_err(|_| format!("journal record has an invalid {what}"))
}

fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    if text.len() % 2 != 0 {
        return Err("journal payload has an odd number of hex digits".to_owned());
    }
    let mut bytes = Vec::with_capacity(text.len() / 2);
    let chars: Vec<u8> = text.bytes().collect();
    for pair in chars.chunks(2) {
        let hi = hex_value(pair[0])?;
        let lo = hex_value(pair[1])?;
        bytes.push((hi << 4) | lo);
    }
    Ok(bytes)
}

fn hex_value(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err("journal payload is not lowercase hex".to_owned()),
    }
}

fn read_to_string(path: &Path) -> Result<String, ExtensibleError> {
    let mut file = File::open(path).map_err(|e| ExtensibleError::storage(path, e))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)
        .map_err(|e| ExtensibleError::storage(path, e))?;
    Ok(buf)
}
