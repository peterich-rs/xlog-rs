//! Encryption seam for the extensible path.
//!
//! [`Encryptor::seal`] covers one complete segment, not an arbitrary slice of
//! a frame. The first real implementation is [`NoEncryption`], which copies
//! the segment through unchanged. A format that does not support encryption
//! rejects every other encryptor before a file is opened.

use crate::extensible::error::ExtensibleError;

/// Stable description of an encryption algorithm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptionDescriptor {
    /// Algorithm name, for example `none`.
    pub algorithm: &'static str,
    /// Algorithm version.
    pub version: u16,
}

/// Public metadata produced while sealing one segment.
///
/// The plaintext passthrough carries nothing. A real algorithm returns its
/// nonce, key id, and tag here instead of a fixed-width Mars key slot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EncryptionMetadata {
    /// Key identifier, when the algorithm has one.
    pub key_id: Option<String>,
    /// Nonce or IV, when the algorithm has one.
    pub nonce: Option<Vec<u8>>,
}

/// Context fixed before a segment is sealed.
///
/// Associated data is built from these fields by the format, before `seal`
/// runs, so nothing learned from the ciphertext can be fed back into it.
#[derive(Debug, Clone, Copy)]
pub struct SealContext<'a> {
    /// File the segment belongs to.
    pub file_id: u64,
    /// Segment counter inside the file, starting at zero.
    pub segment_seq: u64,
    /// Associated data the ciphertext commits to.
    pub aad: &'a [u8],
}

/// Seals one complete segment.
///
/// The writer owns the encryptor exclusively. Implementations may keep a
/// counter or session state and do not need to be `Sync` or `Clone`.
pub trait Encryptor: Send {
    /// Identify the algorithm.
    fn descriptor(&self) -> EncryptionDescriptor;

    /// Seal `plaintext` into `output`, which the caller has cleared.
    ///
    /// On success `output` holds the exact bytes to append. On failure
    /// `output` is unspecified and the caller discards it.
    fn seal(
        &mut self,
        context: &SealContext<'_>,
        plaintext: &[u8],
        output: &mut Vec<u8>,
    ) -> Result<EncryptionMetadata, ExtensibleError>;
}

/// Copies a segment through without transforming it.
///
/// This is an explicit choice, never the result of an encryption failure.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoEncryption;

impl NoEncryption {
    /// The descriptor every passthrough segment reports.
    pub const DESCRIPTOR: EncryptionDescriptor = EncryptionDescriptor {
        algorithm: "none",
        version: 1,
    };
}

impl Encryptor for NoEncryption {
    fn descriptor(&self) -> EncryptionDescriptor {
        Self::DESCRIPTOR
    }

    fn seal(
        &mut self,
        _context: &SealContext<'_>,
        plaintext: &[u8],
        output: &mut Vec<u8>,
    ) -> Result<EncryptionMetadata, ExtensibleError> {
        output.clear();
        output.extend_from_slice(plaintext);
        Ok(EncryptionMetadata::default())
    }
}
