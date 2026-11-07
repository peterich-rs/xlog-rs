//! Opt-in extensible log mode.
//!
//! This module is a separate storage path from the Mars-compatible engine. It
//! never writes Mars headers, never reuses `.mmap3`, and never scans a
//! directory by file extension. Files it owns are recorded in a catalog, and a
//! catalog that cannot be verified refuses writes and destructive maintenance
//! instead of falling back to a directory scan.
//!
//! The format, encoder, and encryptor seams here are crate-internal prototypes.
//! They become a public contract only after a second, genuinely encrypted
//! format exercises them.

mod catalog;
mod crypto;
mod error;
mod format;
mod journal;
mod naming;
mod rotation;
mod store;

pub use catalog::{CatalogEntry, FileCatalog, LifecycleState};
pub use crypto::{EncryptionDescriptor, EncryptionMetadata, Encryptor, NoEncryption, SealContext};
pub use error::ExtensibleError;
pub use format::{
    write_json_line, FileContext, FileFormat, FormatDescriptor, FormatEncoder, JsonLinesFormat,
    PlainTextFormat, SegmentInfo,
};
pub use journal::{Journal, JournalRecord};
pub use naming::{FileIdentity, FileNaming, IdFileNaming, RelativeFileName, TimestampFileNaming};
pub use rotation::{RotationDecision, RotationPolicy};
pub use store::{ExtensibleStore, MaintenanceReport, RecoveryReport, StoreConfig};

#[cfg(test)]
mod tests;
