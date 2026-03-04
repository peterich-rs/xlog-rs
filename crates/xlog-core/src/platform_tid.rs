use std::sync::OnceLock;

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

pub fn main_tid() -> i64 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // Matches Mars unix/android behavior where maintid tracks process main tid (pid).
        return std::process::id() as i64;
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos"
    ))]
    {
        static MAIN_TID: OnceLock<i64> = OnceLock::new();
        return *MAIN_TID.get_or_init(current_tid);
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
