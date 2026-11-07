//! Errors for the extensible storage path.
//!
//! Variants stay distinct so a caller can tell a bad configuration apart from
//! a catalog that needs recovery. Nothing in this module collapses an I/O
//! failure into a single "init failed".

use std::path::PathBuf;

/// Failure produced by the extensible storage path.
#[derive(Debug, thiserror::Error)]
pub enum ExtensibleError {
    /// A value supplied at construction cannot be used.
    #[error("invalid extensible log config: {0}")]
    Config(String),

    /// The namespace already has a live store in this process.
    #[error("namespace `{namespace}` already has a live store")]
    AlreadyExists {
        /// Namespace that was requested twice.
        namespace: String,
    },

    /// No live store is registered under the namespace.
    #[error("namespace `{namespace}` has no live store")]
    NotFound {
        /// Namespace that was looked up.
        namespace: String,
    },

    /// The catalog cannot be trusted, so writes and deletion stay disabled.
    #[error("catalog for `{namespace}` requires recovery: {reason}")]
    RecoveryRequired {
        /// Namespace whose catalog failed verification.
        namespace: String,
        /// Why the catalog was rejected.
        reason: String,
    },

    /// A file name collided and the bounded retries were exhausted.
    #[error("file name `{name}` collided")]
    NameCollision {
        /// Name that could not be created exclusively.
        name: String,
    },

    /// The format rejected the record or the current operation.
    #[error("format error: {0}")]
    Format(String),

    /// Encryption failed. The extensible path never falls back to plaintext.
    #[error("encryption error: {0}")]
    Encrypt(String),

    /// The name produced by a naming strategy was rejected.
    #[error("file name rejected: {0}")]
    Naming(String),

    /// A filesystem operation failed.
    #[error("storage error at {path}: {source}")]
    Storage {
        /// Path the operation targeted.
        path: PathBuf,
        /// Underlying operating system error.
        source: std::io::Error,
    },

    /// The store is shutting down or has already failed closed.
    #[error("store is not accepting writes: {0}")]
    Closed(String),
}

impl ExtensibleError {
    pub(crate) fn storage(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Storage {
            path: path.into(),
            source,
        }
    }
}
