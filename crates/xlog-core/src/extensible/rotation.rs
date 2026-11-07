//! Rotation decisions for the extensible path.
//!
//! Rotation is a concrete policy, not a trait. There is no second external
//! implementation to justify one, and the decision is independent of how a
//! file is named.

/// When the active file should be sealed and a new one opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationPolicy {
    /// Never rotate on size or time.
    Never,
    /// Rotate once the active file reaches `max_bytes`.
    Size {
        /// Soft byte ceiling of one file.
        max_bytes: u64,
    },
    /// Rotate when the calendar day of the file and the record differ.
    Daily,
    /// Rotate on whichever of size and day happens first.
    SizeAndDaily {
        /// Soft byte ceiling of one file.
        max_bytes: u64,
    },
}

/// Outcome of comparing one record against the active file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationDecision {
    /// Append to the active file.
    Keep,
    /// Seal the active file first, then append to a new file.
    Rotate,
}

impl RotationPolicy {
    /// Decide whether `record_len` bytes at `now_unix_secs` fit the file
    /// opened at `opened_unix_secs` whose current size is `current_len`.
    pub fn decide(
        &self,
        current_len: u64,
        record_len: u64,
        opened_unix_secs: u64,
        now_unix_secs: u64,
    ) -> RotationDecision {
        let size_exceeded = match self {
            Self::Size { max_bytes } | Self::SizeAndDaily { max_bytes } => {
                current_len > 0 && current_len.saturating_add(record_len) > *max_bytes
            }
            Self::Never | Self::Daily => false,
        };
        let day_changed = match self {
            Self::Daily | Self::SizeAndDaily { .. } => {
                day_number(opened_unix_secs) != day_number(now_unix_secs)
            }
            Self::Never | Self::Size { .. } => false,
        };
        if size_exceeded || day_changed {
            RotationDecision::Rotate
        } else {
            RotationDecision::Keep
        }
    }
}

fn day_number(unix_secs: u64) -> u64 {
    unix_secs / 86_400
}
