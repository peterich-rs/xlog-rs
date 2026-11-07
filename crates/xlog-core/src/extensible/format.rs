//! File content formats for the extensible path.
//!
//! A format describes content, not a file name. [`FileFormat`] opens one
//! encoder per file, and the encoder emits self-contained segments. Segments
//! are only ever appended; a format that needs to patch earlier bytes is
//! outside this contract.

use crate::extensible::crypto::{EncryptionMetadata, Encryptor, NoEncryption};
use crate::extensible::error::ExtensibleError;

/// Identity and version of a content format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatDescriptor {
    /// Stable format id, for example `jsonl`.
    pub id: &'static str,
    /// Format version.
    pub version: u16,
    /// Suggested file suffix, including the leading dot.
    pub suffix: &'static str,
    /// Encryption algorithms this format accepts.
    pub encryption: &'static [&'static str],
}

/// Per-file facts handed to a format when a file is opened.
#[derive(Debug, Clone)]
pub struct FileContext {
    /// Stable id of the file being opened.
    pub file_id: u64,
    /// Namespace the file belongs to.
    pub namespace: String,
}

/// What one sealed segment committed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentInfo {
    /// Segment counter inside the file.
    pub sequence: u64,
    /// Bytes the segment occupies once sealed.
    pub len: u64,
    /// Metadata the encryptor returned.
    pub encryption: EncryptionMetadata,
}

/// Opens an encoder for one new file.
pub trait FileFormat: Send {
    /// Describe the format and the combinations it accepts.
    fn descriptor(&self) -> FormatDescriptor;

    /// Reject combinations this format cannot honor.
    ///
    /// A rejection must happen before the store opens the target file or
    /// writes a recovery record.
    fn validate(&self, encryptor: &dyn Encryptor) -> Result<(), ExtensibleError>;

    /// Start a new file. The returned encoder owns the file's mutable state.
    fn start_file(&self, context: &FileContext) -> Result<Box<dyn FormatEncoder>, ExtensibleError>;
}

/// Mutable encoding state for a single file.
pub trait FormatEncoder: Send {
    /// Accept one record.
    ///
    /// Success takes responsibility for the record. Failure leaves the encoder
    /// unchanged and the record unaccepted.
    fn push_record(&mut self, record: &[u8]) -> Result<(), ExtensibleError>;

    /// Seal the buffered records into `output`.
    ///
    /// `output` arrives cleared. `Ok(None)` means there was nothing to seal,
    /// and `output` stays empty. After `Ok(Some(_))` the buffer is empty and
    /// `output` holds one complete appendable segment.
    fn seal_segment(
        &mut self,
        encryptor: &mut dyn Encryptor,
        output: &mut Vec<u8>,
    ) -> Result<Option<SegmentInfo>, ExtensibleError>;

    /// Seal whatever remains and refuse further records.
    fn finish_file(
        &mut self,
        encryptor: &mut dyn Encryptor,
        output: &mut Vec<u8>,
    ) -> Result<Option<SegmentInfo>, ExtensibleError>;
}

/// One JSON object per line, written as plaintext.
///
/// The format accepts only [`NoEncryption`]. Compression and any other
/// encryptor are rejected at validation, because the result would no longer be
/// readable as JSON Lines.
#[derive(Debug, Default, Clone, Copy)]
pub struct JsonLinesFormat;

impl JsonLinesFormat {
    /// Format descriptor for plaintext JSON Lines.
    pub const DESCRIPTOR: FormatDescriptor = FormatDescriptor {
        id: "jsonl",
        version: 1,
        suffix: ".log",
        encryption: &["none"],
    };
}

impl FileFormat for JsonLinesFormat {
    fn descriptor(&self) -> FormatDescriptor {
        Self::DESCRIPTOR
    }

    fn validate(&self, encryptor: &dyn Encryptor) -> Result<(), ExtensibleError> {
        let descriptor = encryptor.descriptor();
        if descriptor == NoEncryption::DESCRIPTOR {
            Ok(())
        } else {
            Err(ExtensibleError::Config(format!(
                "jsonl only accepts no-encryption, not {} v{}",
                descriptor.algorithm, descriptor.version
            )))
        }
    }

    fn start_file(
        &self,
        _context: &FileContext,
    ) -> Result<Box<dyn FormatEncoder>, ExtensibleError> {
        Ok(Box::new(LineEncoder {
            pending: Vec::new(),
            segment_seq: 0,
            finished: false,
        }))
    }
}

/// Bytes copied through unchanged, one record per line.
///
/// Whether a record already ends in a newline is the caller's decision; this
/// format appends a newline only when the record does not have one, so a line
/// layout and the file format stay independent.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlainTextFormat;

impl PlainTextFormat {
    /// Format descriptor for plaintext line files.
    pub const DESCRIPTOR: FormatDescriptor = FormatDescriptor {
        id: "text",
        version: 1,
        suffix: ".log",
        encryption: &["none"],
    };
}

impl FileFormat for PlainTextFormat {
    fn descriptor(&self) -> FormatDescriptor {
        Self::DESCRIPTOR
    }

    fn validate(&self, encryptor: &dyn Encryptor) -> Result<(), ExtensibleError> {
        let descriptor = encryptor.descriptor();
        if descriptor == NoEncryption::DESCRIPTOR {
            Ok(())
        } else {
            Err(ExtensibleError::Config(format!(
                "text only accepts no-encryption, not {} v{}",
                descriptor.algorithm, descriptor.version
            )))
        }
    }

    fn start_file(
        &self,
        _context: &FileContext,
    ) -> Result<Box<dyn FormatEncoder>, ExtensibleError> {
        Ok(Box::new(LineEncoder {
            pending: Vec::new(),
            segment_seq: 0,
            finished: false,
        }))
    }
}

/// Shared encoder for the line-oriented plaintext formats.
///
/// Both formats have the same segment shape: the buffered records, each
/// terminated by a newline, passed through the encryptor. They differ only in
/// who builds the record bytes.
struct LineEncoder {
    pending: Vec<u8>,
    segment_seq: u64,
    finished: bool,
}

impl FormatEncoder for LineEncoder {
    fn push_record(&mut self, record: &[u8]) -> Result<(), ExtensibleError> {
        if self.finished {
            return Err(ExtensibleError::Format(
                "record pushed after the file was finished".to_owned(),
            ));
        }
        self.pending.extend_from_slice(record);
        if !record.ends_with(b"\n") {
            self.pending.push(b'\n');
        }
        Ok(())
    }

    fn seal_segment(
        &mut self,
        encryptor: &mut dyn Encryptor,
        output: &mut Vec<u8>,
    ) -> Result<Option<SegmentInfo>, ExtensibleError> {
        self.seal(encryptor, output, false)
    }

    fn finish_file(
        &mut self,
        encryptor: &mut dyn Encryptor,
        output: &mut Vec<u8>,
    ) -> Result<Option<SegmentInfo>, ExtensibleError> {
        let sealed = self.seal(encryptor, output, true)?;
        self.finished = true;
        Ok(sealed)
    }
}

impl LineEncoder {
    fn seal(
        &mut self,
        encryptor: &mut dyn Encryptor,
        output: &mut Vec<u8>,
        finishing: bool,
    ) -> Result<Option<SegmentInfo>, ExtensibleError> {
        if self.finished {
            return Err(ExtensibleError::Format(
                "file was already finished".to_owned(),
            ));
        }
        if self.pending.is_empty() {
            output.clear();
            return Ok(None);
        }
        let sequence = self.segment_seq;
        let aad = format!("segment:{sequence}").into_bytes();
        let context = super::crypto::SealContext {
            file_id: 0,
            segment_seq: sequence,
            aad: &aad,
        };
        let encryption = match encryptor.seal(&context, &self.pending, output) {
            Ok(metadata) => metadata,
            Err(error) => {
                output.clear();
                return Err(error);
            }
        };
        let len = u64::try_from(output.len()).unwrap_or(u64::MAX);
        self.pending.clear();
        self.segment_seq = self.segment_seq.saturating_add(1);
        let _ = finishing;
        Ok(Some(SegmentInfo {
            sequence,
            len,
            encryption,
        }))
    }
}

/// Serialize one record as a single JSON object line.
///
/// The schema is fixed: level, tag, target, and message, all text. No
/// caller-supplied serializer is accepted, so the only escaping needed is for
/// those four strings and it cannot fail on a legal `&str`.
pub fn write_json_line(out: &mut String, level: &str, tag: &str, target: &str, message: &str) {
    out.push_str("{\"level\":\"");
    push_escaped(out, level);
    out.push_str("\",\"tag\":\"");
    push_escaped(out, tag);
    out.push_str("\",\"target\":\"");
    push_escaped(out, target);
    out.push_str("\",\"message\":\"");
    push_escaped(out, message);
    out.push_str("\"}");
}

fn push_escaped(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = std::fmt::Write::write_fmt(out, format_args!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}
