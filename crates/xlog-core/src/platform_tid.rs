#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos"
))]
use std::sync::OnceLock;

/// Return the current platform thread id, or `-1` on unsupported targets.
pub fn current_tid() -> i64 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        unsafe { libc::syscall(libc::SYS_gettid) as i64 }
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos"
    ))]
    {
        let mut tid: u64 = 0;
        unsafe {
            libc::pthread_threadid_np(0, &mut tid);
        }
        tid as i64
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos"
    )))]
    {
        -1
    }
}

/// Return the platform "main thread id" used by Mars-compatible metadata.
///
/// On Linux/Android this follows the historical Mars behavior and returns the
/// process id. On Apple targets the first observed thread id is memoized.
pub fn main_tid() -> i64 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // Matches Mars unix/android behavior where maintid tracks process main tid (pid).
        std::process::id() as i64
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos"
    ))]
    {
        static MAIN_TID: OnceLock<i64> = OnceLock::new();
        *MAIN_TID.get_or_init(current_tid)
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos"
    )))]
    {
        -1
    }
}

#[cfg(test)]
mod tests {
    use super::{current_tid, main_tid};

    #[test]
    fn main_tid_is_stable_across_calls() {
        let first = main_tid();
        let second = main_tid();

        assert_eq!(first, second);

        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(first, std::process::id() as i64);
    }

    #[test]
    fn current_tid_is_positive_on_supported_targets() {
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios",
            target_os = "tvos",
            target_os = "watchos"
        ))]
        assert!(current_tid() > 0);
    }
}
