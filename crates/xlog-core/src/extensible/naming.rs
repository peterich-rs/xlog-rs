//! File naming for the extensible path.
//!
//! A name is only a single path component. The store joins it onto its own
//! root, so a naming strategy cannot escape the namespace directory. Names do
//! not have to be reversible: ownership comes from the catalog, not from
//! parsing the name back into a date.

use std::path::{Component, Path};

use crate::extensible::error::ExtensibleError;

/// Identity the runtime assigns to one log file.
///
/// The naming strategy may use any of these fields and may ignore the time.
/// Rotation does not depend on what the strategy puts in the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIdentity {
    /// Namespace the file belongs to.
    pub namespace: String,
    /// Stable id, unique inside the namespace.
    pub file_id: u64,
    /// Rotation counter for this file.
    pub sequence: u64,
    /// Creation time as seconds since the Unix epoch.
    pub created_unix_secs: u64,
    /// Suggested suffix from the format, including the leading dot.
    pub suffix: String,
}

/// A file name that is exactly one normal component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelativeFileName {
    value: String,
}

impl RelativeFileName {
    /// Validate `name` as a single file name.
    ///
    /// Rejects empty names, names containing a separator, `.` and `..`, NUL,
    /// and Windows drive-prefix and verbatim-prefix forms.
    pub fn new(name: impl Into<String>) -> Result<Self, ExtensibleError> {
        let value = name.into();
        if value.is_empty() || value.contains('\0') {
            return Err(ExtensibleError::Naming(
                "file name is empty or contains NUL".to_owned(),
            ));
        }
        if value.contains('/') || value.contains('\\') {
            return Err(ExtensibleError::Naming(
                "file name must be a single component".to_owned(),
            ));
        }
        let path = Path::new(&value);
        let mut components = path.components();
        match components.next() {
            Some(Component::Normal(_)) if components.next().is_none() => {}
            _ => {
                return Err(ExtensibleError::Naming(format!(
                    "`{value}` is not a single normal path component"
                )));
            }
        }
        Ok(Self { value })
    }

    /// Borrow the validated name.
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

/// Produces the file name for one [`FileIdentity`].
///
/// The strategy does not open, lock, or delete files. The store does that
/// after the name has been validated.
pub trait FileNaming: Send {
    /// Build the name for `identity`.
    fn file_name(&mut self, identity: &FileIdentity) -> Result<RelativeFileName, ExtensibleError>;
}

/// Names files `<namespace>_<file id><suffix>`, with no date component.
#[derive(Debug, Default, Clone, Copy)]
pub struct IdFileNaming;

impl FileNaming for IdFileNaming {
    fn file_name(&mut self, identity: &FileIdentity) -> Result<RelativeFileName, ExtensibleError> {
        RelativeFileName::new(format!(
            "{}_{:016x}{}",
            identity.namespace, identity.file_id, identity.suffix
        ))
    }
}

/// Names files `<namespace>_<utc date>_<sequence><suffix>`.
///
/// The date is formatting only. Rotation stays on [`super::RotationPolicy`].
#[derive(Debug, Default, Clone, Copy)]
pub struct TimestampFileNaming;

impl FileNaming for TimestampFileNaming {
    fn file_name(&mut self, identity: &FileIdentity) -> Result<RelativeFileName, ExtensibleError> {
        let date = chrono::DateTime::from_timestamp(identity.created_unix_secs as i64, 0)
            .ok_or_else(|| ExtensibleError::Naming("creation time is out of range".to_owned()))?;
        RelativeFileName::new(format!(
            "{}_{}_{}{}",
            identity.namespace,
            date.format("%Y%m%d"),
            identity.sequence,
            identity.suffix
        ))
    }
}
