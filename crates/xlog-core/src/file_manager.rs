use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{IoSlice, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use chrono::{Datelike, Duration as ChronoDuration, Local};
use fs2::{available_space, FileExt};
use thiserror::Error;

use crate::metrics::{
    record_cache_move, record_expired_delete, record_file_append, record_file_rotate,
};

const LOG_EXT: &str = "xlog";
const CACHE_AVAILABLE_THRESHOLD_BYTES: u64 = 1024 * 1024 * 1024;
// Keep-open sync path benefits from a moderate userspace append buffer under
// contention without turning flush bursts into a new tail-latency problem.
const ACTIVE_APPEND_BUFFER_CAPACITY: usize = 64 * 1024;
const FILE_COPY_BUFFER_SIZE: usize = 128 * 1024;

#[derive(Debug, Error)]
/// Errors produced while selecting log targets or mutating log/cache files.
pub enum FileManagerError {
    /// The configured log directory path was empty.
    #[error("log_dir must be non-empty")]
    EmptyLogDir,
    /// The configured log file prefix was empty.
    #[error("name_prefix must be non-empty")]
    EmptyNamePrefix,
    /// Creating a required directory failed.
    #[error("create directory failed for {0}: {1}")]
    CreateDir(PathBuf, #[source] std::io::Error),
    /// Listing a directory failed.
    #[error("read directory failed for {0}: {1}")]
    ReadDir(PathBuf, #[source] std::io::Error),
    /// Reading file metadata failed.
    #[error("file metadata failed for {0}: {1}")]
    Metadata(PathBuf, #[source] std::io::Error),
    /// Opening a file for read or append failed.
    #[error("open file failed for {0}: {1}")]
    OpenFile(PathBuf, #[source] std::io::Error),
    /// Acquiring the per-instance lock file failed.
    #[error("lock file failed for {0}: {1}")]
    LockFile(PathBuf, #[source] std::io::Error),
    /// Writing file data or flushing buffered bytes failed.
    #[error("write file failed for {0}: {1}")]
    WriteFile(PathBuf, #[source] std::io::Error),
    /// Synchronizing file data to stable storage failed.
    #[error("sync file failed for {0}: {1}")]
    SyncFile(PathBuf, #[source] std::io::Error),
    /// Reading file data failed.
    #[error("read file failed for {0}: {1}")]
    ReadFile(PathBuf, #[source] std::io::Error),
    /// Removing a file failed.
    #[error("remove file failed for {0}: {1}")]
    RemoveFile(PathBuf, #[source] std::io::Error),
    /// Removing a directory tree failed.
    #[error("remove directory failed for {0}: {1}")]
    RemoveDir(PathBuf, #[source] std::io::Error),
}

#[derive(Debug, Clone)]
/// Resolves daily log file paths and appends encoded log frames to them.
pub struct FileManager {
    log_dir: PathBuf,
    cache_dir: Option<PathBuf>,
    name_prefix: String,
    cache_days: i32,
    runtime: Arc<Mutex<RuntimeState>>,
    _lock_file: Arc<File>,
}

#[derive(Debug, Default)]
struct RuntimeState {
    last_append_time: Option<i64>,
    last_append_path: Option<PathBuf>,
    active_file: Option<ActiveAppendFile>,
    log_target: Option<AppendTargetCache>,
    cache_target: Option<AppendTargetCache>,
}

#[derive(Debug)]
struct ActiveAppendFile {
    path: PathBuf,
    day_key: i32,
    logical_len: u64,
    disk_len: u64,
    buffered: bool,
    write_buffer: Vec<u8>,
    file: File,
}

impl Drop for ActiveAppendFile {
    fn drop(&mut self) {
        if self.write_buffer.is_empty() {
            return;
        }
        let _ = self.file.write_all(&self.write_buffer);
        self.write_buffer.clear();
    }
}

#[derive(Debug, Clone)]
struct AppendTargetCache {
    path: PathBuf,
    day_key: i32,
    file_index: i64,
    merged_len: u64,
    local_len: u64,
    local_exists: bool,
}

impl FileManager {
    /// Creates a file manager for the given log and optional cache directories.
    pub fn new(
        log_dir: PathBuf,
        cache_dir: Option<PathBuf>,
        name_prefix: String,
        cache_days: i32,
    ) -> Result<Self, FileManagerError> {
        if log_dir.as_os_str().is_empty() {
            return Err(FileManagerError::EmptyLogDir);
        }
        if name_prefix.is_empty() {
            return Err(FileManagerError::EmptyNamePrefix);
        }

        fs::create_dir_all(&log_dir)
            .map_err(|e| FileManagerError::CreateDir(log_dir.clone(), e))?;
        if let Some(dir) = &cache_dir {
            fs::create_dir_all(dir).map_err(|e| FileManagerError::CreateDir(dir.clone(), e))?;
        }

        let lock_path = log_dir.join(format!("{}.lock", name_prefix));
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| FileManagerError::OpenFile(lock_path.clone(), e))?;
        lock_file
            .try_lock_exclusive()
            .map_err(|e| FileManagerError::LockFile(lock_path.clone(), e))?;

        Ok(Self {
            log_dir,
            cache_dir,
            name_prefix,
            cache_days,
            runtime: Arc::new(Mutex::new(RuntimeState::default())),
            _lock_file: Arc::new(lock_file),
        })
    }

    /// Returns the primary directory that stores flushed log files.
    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    /// Returns the cache directory used for temporary log files, if enabled.
    pub fn cache_dir(&self) -> Option<&Path> {
        self.cache_dir.as_deref()
    }

    /// Returns the configured file-name prefix used for new log files.
    pub fn name_prefix(&self) -> &str {
        &self.name_prefix
    }

    /// Returns the number of days a cache file may remain before being moved.
    pub fn cache_days(&self) -> i32 {
        self.cache_days
    }

    /// Returns the mmap sidecar path associated with this file set.
    pub fn mmap_path(&self) -> PathBuf {
        let base = self.cache_dir.as_ref().unwrap_or(&self.log_dir);
        base.join(format!("{}.mmap3", self.name_prefix))
    }

    /// Lists existing log files for the given day offset and prefix.
    pub fn filepaths_from_timespan(&self, timespan: i32, prefix: &str) -> Vec<String> {
        let file_prefix = make_date_prefix(prefix, timespan);

        let mut out = Vec::new();
        out.extend(self.list_existing_files(&self.log_dir, &file_prefix));
        if let Some(cache_dir) = &self.cache_dir {
            out.extend(self.list_existing_files(cache_dir, &file_prefix));
        }
        out
    }

    /// Returns candidate log file paths for the given day offset and size policy.
    ///
    /// When a cache directory is configured, existing cache and log targets are both
    /// returned. If no file exists yet, the primary log-file path is returned as the
    /// location a new append would create.
    pub fn make_logfile_name(
        &self,
        timespan: i32,
        prefix: &str,
        max_file_size: u64,
    ) -> Vec<String> {
        let now = Local::now() - ChronoDuration::days(timespan as i64);
        let log_path = self.make_path_for_time(now, &self.log_dir, prefix, max_file_size);

        if self.cache_dir.is_none() {
            return vec![log_path.to_string_lossy().to_string()];
        }

        let cache_dir = self.cache_dir.as_ref().expect("checked is_some");
        let cache_path = self.make_path_for_time(now, cache_dir, prefix, max_file_size);

        let mut out = Vec::new();
        if log_path.exists() {
            out.push(log_path.to_string_lossy().to_string());
        }
        if cache_path.exists() {
            out.push(cache_path.to_string_lossy().to_string());
        }
        if out.is_empty() {
            out.push(log_path.to_string_lossy().to_string());
        }
        out
    }

    /// Appends one encoded log frame to the active target file.
    pub fn append_log_bytes(
        &self,
        bytes: &[u8],
        max_file_size: u64,
        move_file: bool,
        keep_open: bool,
    ) -> Result<(), FileManagerError> {
        self.append_log_slices_inner(&[bytes], max_file_size, move_file, keep_open, false)
    }

    /// Appends one encoded log frame and synchronizes the destination file data.
    ///
    /// This is intended for low-frequency recovery paths that must not remove
    /// their source before the destination append is durable.
    pub fn append_log_bytes_durable(
        &self,
        bytes: &[u8],
        max_file_size: u64,
        move_file: bool,
    ) -> Result<(), FileManagerError> {
        self.append_log_slices_inner(&[bytes], max_file_size, move_file, false, true)
    }

    /// Appends multiple encoded log frame slices to the active target file.
    ///
    /// `move_file` allows cache files to be merged into the primary log directory when
    /// the cache policy no longer applies. `keep_open` reuses a buffered file handle on
    /// the plain log path to reduce reopen overhead on hot sync append paths.
    pub fn append_log_slices(
        &self,
        slices: &[&[u8]],
        max_file_size: u64,
        move_file: bool,
        keep_open: bool,
    ) -> Result<(), FileManagerError> {
        self.append_log_slices_inner(slices, max_file_size, move_file, keep_open, false)
    }

    fn append_log_slices_inner(
        &self,
        slices: &[&[u8]],
        max_file_size: u64,
        move_file: bool,
        keep_open: bool,
        durable: bool,
    ) -> Result<(), FileManagerError> {
        let total_bytes = slices.iter().map(|slice| slice.len()).sum::<usize>();
        if total_bytes == 0 {
            return Ok(());
        }

        if self.cache_dir.is_none() {
            let now = Local::now();
            let mut runtime = self
                .runtime
                .lock()
                .expect("file_manager runtime lock poisoned");
            if keep_open
                && self.try_append_active_plain_keep_open(
                    &mut runtime,
                    slices,
                    now,
                    max_file_size,
                )?
            {
                return Ok(());
            }
            let path = self.select_append_path_locked(
                &mut runtime,
                now,
                &self.log_dir,
                &self.name_prefix,
                max_file_size,
                keep_open,
            );
            return self.append_slices_with_runtime_locked(
                &mut runtime,
                &path,
                slices,
                now,
                keep_open,
                durable,
            );
        }

        let cache_dir = self.cache_dir.as_ref().expect("cache_dir is_some");
        let now = Local::now();
        if let Some(log_path) = self.active_append_path(now, &self.log_dir, keep_open) {
            return self.append_slices_with_runtime(&log_path, slices, now, keep_open, durable);
        }
        if let Some(cache_path) = self.active_append_path(now, cache_dir, keep_open) {
            self.append_slices_with_runtime(&cache_path, slices, now, keep_open, durable)?;
            if move_file && !self.should_cache_logs(now, max_file_size) {
                let log_path =
                    self.select_append_path(now, &self.log_dir, &self.name_prefix, max_file_size);
                append_file_to_file(&cache_path, &log_path)?;
                fs::remove_file(&cache_path)
                    .map_err(|e| FileManagerError::RemoveFile(cache_path.clone(), e))?;
                self.mark_cached_path_removed(&cache_path);
            }
            return Ok(());
        }

        let cache_path = self.select_append_path(now, cache_dir, &self.name_prefix, max_file_size);
        let cache_logs = self.should_cache_logs(now, max_file_size);
        let cache_exists = self
            .cached_local_exists(now, cache_dir, max_file_size)
            .unwrap_or_else(|| cache_path.exists());

        if cache_logs || cache_exists {
            self.append_slices_with_runtime(&cache_path, slices, now, keep_open, durable)?;
            if cache_logs || !move_file {
                return Ok(());
            }

            let log_path =
                self.select_append_path(now, &self.log_dir, &self.name_prefix, max_file_size);
            append_file_to_file(&cache_path, &log_path)?;
            fs::remove_file(&cache_path)
                .map_err(|e| FileManagerError::RemoveFile(cache_path.clone(), e))?;
            self.mark_cached_path_removed(&cache_path);
            return Ok(());
        }

        let log_path =
            self.select_append_path(now, &self.log_dir, &self.name_prefix, max_file_size);
        match self.append_slices_with_runtime(&log_path, slices, now, keep_open, durable) {
            Ok(()) => Ok(()),
            Err(_) => self.append_slices_with_runtime(&cache_path, slices, now, keep_open, durable),
        }
    }

    /// Moves eligible cache files into the primary log directory.
    ///
    /// Files newer than `cache_days` are left in place. When no cache directory is
    /// configured, this is a no-op.
    pub fn move_old_cache_files(&self, _max_file_size: u64) -> Result<(), FileManagerError> {
        let Some(cache_dir) = &self.cache_dir else {
            return Ok(());
        };
        if cache_dir == &self.log_dir {
            return Ok(());
        }
        if !cache_dir.is_dir() {
            return Ok(());
        }

        self.flush_active_file_if_needed()?;

        let now = SystemTime::now();
        let entries =
            fs::read_dir(cache_dir).map_err(|e| FileManagerError::ReadDir(cache_dir.clone(), e))?;
        for entry in entries {
            let entry = entry.map_err(|e| FileManagerError::ReadDir(cache_dir.clone(), e))?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            if !file_name.starts_with(&self.name_prefix)
                || !file_name.ends_with(&format!(".{LOG_EXT}"))
            {
                continue;
            }

            if self.cache_days > 0 {
                let modified = file_mtime(&path)?;
                let Ok(age) = now.duration_since(modified) else {
                    continue;
                };
                if age < Duration::from_secs(self.cache_days as u64 * 24 * 60 * 60) {
                    continue;
                }
            }

            let dest = self.log_dir.join(file_name);
            append_file_to_file(&path, &dest)?;
            fs::remove_file(&path).map_err(|e| FileManagerError::RemoveFile(path, e))?;
            record_cache_move();
        }

        Ok(())
    }

    /// Deletes log and cache files whose modification time exceeds `max_alive_seconds`.
    pub fn delete_expired_files(&self, max_alive_seconds: i64) -> Result<(), FileManagerError> {
        if max_alive_seconds <= 0 {
            return Ok(());
        }
        self.flush_active_file_if_needed()?;
        let threshold = Duration::from_secs(max_alive_seconds as u64);
        self.delete_expired_under(&self.log_dir, threshold)?;
        if let Some(cache_dir) = &self.cache_dir {
            self.delete_expired_under(cache_dir, threshold)?;
        }
        Ok(())
    }

    /// Flushes any buffered bytes held by the keep-open append path.
    pub fn flush_active_file_buffer(&self) -> Result<(), FileManagerError> {
        self.flush_active_file_if_needed()
    }

    fn delete_expired_under(
        &self,
        dir: &Path,
        threshold: Duration,
    ) -> Result<(), FileManagerError> {
        if !dir.is_dir() {
            return Ok(());
        }

        let now = SystemTime::now();
        let entries =
            fs::read_dir(dir).map_err(|e| FileManagerError::ReadDir(dir.to_path_buf(), e))?;
        for entry in entries {
            let entry = entry.map_err(|e| FileManagerError::ReadDir(dir.to_path_buf(), e))?;
            let path = entry.path();
            let modified = file_mtime(&path)?;
            let Ok(age) = now.duration_since(modified) else {
                continue;
            };
            if age <= threshold {
                continue;
            }

            if path.is_file() {
                if path.extension().and_then(OsStr::to_str) == Some(LOG_EXT) {
                    fs::remove_file(&path).map_err(|e| FileManagerError::RemoveFile(path, e))?;
                    record_expired_delete();
                }
                continue;
            }

            if path.is_dir() {
                let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
                let is_day_folder = name.len() == 8 && name.chars().all(|c| c.is_ascii_digit());
                if is_day_folder {
                    fs::remove_dir_all(&path).map_err(|e| FileManagerError::RemoveDir(path, e))?;
                    record_expired_delete();
                }
            }
        }
        Ok(())
    }

    fn list_existing_files(&self, dir: &Path, file_prefix: &str) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            if name.starts_with(file_prefix) && name.ends_with(&format!(".{LOG_EXT}")) {
                out.push(path.to_string_lossy().to_string());
            }
        }
        out
    }

    fn flush_active_file_if_needed(&self) -> Result<(), FileManagerError> {
        let mut runtime = self
            .runtime
            .lock()
            .expect("file_manager runtime lock poisoned");
        if let Some(active) = runtime.active_file.as_mut() {
            flush_active_append_file(active)?;
        }
        Ok(())
    }

    fn cached_target<'a>(
        &self,
        runtime: &'a RuntimeState,
        dir: &Path,
    ) -> Option<&'a AppendTargetCache> {
        if dir == self.log_dir.as_path() {
            runtime.log_target.as_ref()
        } else if self.cache_dir.as_deref() == Some(dir) {
            runtime.cache_target.as_ref()
        } else {
            None
        }
    }

    fn cached_target_mut<'a>(
        &self,
        runtime: &'a mut RuntimeState,
        dir: &Path,
    ) -> Option<&'a mut Option<AppendTargetCache>> {
        if dir == self.log_dir.as_path() {
            Some(&mut runtime.log_target)
        } else if self.cache_dir.as_deref() == Some(dir) {
            Some(&mut runtime.cache_target)
        } else {
            None
        }
    }

    fn set_cached_target(&self, runtime: &mut RuntimeState, dir: &Path, target: AppendTargetCache) {
        if let Some(slot) = self.cached_target_mut(runtime, dir) {
            *slot = Some(target);
        }
    }

    fn record_last_append(runtime: &mut RuntimeState, now_ts: i64, path: &Path) {
        runtime.last_append_time = Some(now_ts);
        runtime.last_append_path = Some(path.to_path_buf());
    }

    fn cached_target_path(
        &self,
        runtime: &mut RuntimeState,
        now_ts: i64,
        dir: &Path,
        day_key: i32,
        prefix: &str,
        max_file_size: u64,
    ) -> Option<PathBuf> {
        let target = self.cached_target(runtime, dir)?.clone();
        if target.day_key != day_key {
            if let Some(slot) = self.cached_target_mut(runtime, dir) {
                *slot = None;
            }
            return None;
        }
        if max_file_size > 0 && target.merged_len > max_file_size {
            let next = AppendTargetCache {
                path: build_path_for_index(dir, prefix, target.day_key, target.file_index + 1),
                day_key,
                file_index: target.file_index + 1,
                merged_len: 0,
                local_len: 0,
                local_exists: false,
            };
            record_file_rotate();
            self.set_cached_target(runtime, dir, next.clone());
            Self::record_last_append(runtime, now_ts, &next.path);
            return Some(next.path);
        }
        Self::record_last_append(runtime, now_ts, &target.path);
        Some(target.path)
    }

    fn cached_local_exists(
        &self,
        now: chrono::DateTime<Local>,
        dir: &Path,
        max_file_size: u64,
    ) -> Option<bool> {
        let mut runtime = self.runtime.lock().ok()?;
        let target = self.cached_target(&runtime, dir)?.clone();
        if target.day_key != day_key(now) {
            if let Some(slot) = self.cached_target_mut(&mut runtime, dir) {
                *slot = None;
            }
            return None;
        }
        if max_file_size > 0 && target.merged_len > max_file_size {
            if let Some(slot) = self.cached_target_mut(&mut runtime, dir) {
                *slot = None;
            }
            return None;
        }
        Some(target.local_exists)
    }

    fn try_append_active_plain_keep_open(
        &self,
        runtime: &mut RuntimeState,
        slices: &[&[u8]],
        now: chrono::DateTime<Local>,
        max_file_size: u64,
    ) -> Result<bool, FileManagerError> {
        let active = match runtime.active_file.as_mut() {
            Some(active) => active,
            None => return Ok(false),
        };
        if active.day_key != day_key(now) {
            return Ok(false);
        }
        if active.path.parent() != Some(self.log_dir.as_path()) {
            return Ok(false);
        }
        if max_file_size > 0 && active.logical_len > max_file_size {
            return Ok(false);
        }

        append_slices_keep_open(active, slices)?;
        if let Some(target) = runtime.log_target.as_mut() {
            if target.day_key == active.day_key && target.path == active.path {
                target.local_exists = true;
                target.local_len = active.logical_len;
                target.merged_len = active.logical_len;
            }
        }
        Ok(true)
    }

    fn update_cached_target_after_append(
        &self,
        runtime: &mut RuntimeState,
        path: &Path,
        day_key: i32,
        merged_len: u64,
        current_len: u64,
    ) {
        let Some(parent) = path.parent() else {
            return;
        };
        let file_name = path.file_name();
        let now_ts = Local::now().timestamp();

        let mut matched_current_dir = false;
        if let Some(target) = runtime.log_target.as_mut() {
            if target.day_key == day_key && target.path.file_name() == file_name {
                target.merged_len = merged_len;
                if target.path == path {
                    target.local_exists = true;
                    target.local_len = current_len;
                    matched_current_dir = true;
                }
            }
        }
        if let Some(target) = runtime.cache_target.as_mut() {
            if target.day_key == day_key && target.path.file_name() == file_name {
                target.merged_len = merged_len;
                if target.path == path {
                    target.local_exists = true;
                    target.local_len = current_len;
                    matched_current_dir = true;
                }
            }
        }

        if !matched_current_dir {
            self.set_cached_target(
                runtime,
                parent,
                AppendTargetCache {
                    path: path.to_path_buf(),
                    day_key,
                    file_index: file_index_from_path(path, &self.name_prefix).unwrap_or(0),
                    merged_len,
                    local_len: current_len,
                    local_exists: true,
                },
            );
        }
        Self::record_last_append(runtime, now_ts, path);
    }

    fn merged_len_after_append(
        &self,
        path: &Path,
        runtime: &RuntimeState,
        day_key: i32,
        delta: u64,
        current_len: u64,
    ) -> u64 {
        let file_name = path.file_name();
        for target in [runtime.log_target.as_ref(), runtime.cache_target.as_ref()]
            .into_iter()
            .flatten()
        {
            if target.day_key == day_key && target.path.file_name() == file_name {
                return target.merged_len.saturating_add(delta);
            }
        }
        current_len
    }

    fn mark_cached_path_removed(&self, path: &Path) {
        let Ok(mut runtime) = self.runtime.lock() else {
            return;
        };

        if let Some(target) = runtime.log_target.as_mut() {
            if target.path == path {
                target.local_exists = false;
                target.local_len = 0;
            }
        }
        if let Some(target) = runtime.cache_target.as_mut() {
            if target.path == path {
                target.local_exists = false;
                target.local_len = 0;
            }
        }
        if runtime
            .active_file
            .as_ref()
            .map(|active| active.path.as_path())
            == Some(path)
        {
            runtime.active_file = None;
        }
        if runtime.last_append_path.as_deref() == Some(path) {
            runtime.last_append_path = None;
        }
    }

    fn should_cache_logs(&self, now: chrono::DateTime<Local>, max_file_size: u64) -> bool {
        let Some(cache_dir) = &self.cache_dir else {
            return false;
        };
        if self.cache_days <= 0 {
            return false;
        }

        if self
            .cached_local_exists(now, &self.log_dir, max_file_size)
            .unwrap_or(false)
        {
            return false;
        }

        let log_path =
            self.make_path_for_time(now, &self.log_dir, &self.name_prefix, max_file_size);
        if log_path.exists() {
            return false;
        }

        match available_space(cache_dir) {
            Ok(bytes) => bytes >= CACHE_AVAILABLE_THRESHOLD_BYTES,
            Err(_) => false,
        }
    }

    fn select_append_path(
        &self,
        now: chrono::DateTime<Local>,
        dir: &Path,
        prefix: &str,
        max_file_size: u64,
    ) -> PathBuf {
        let mut runtime = self
            .runtime
            .lock()
            .expect("file_manager runtime lock poisoned");
        self.select_append_path_locked(&mut runtime, now, dir, prefix, max_file_size, false)
    }

    fn append_slices_with_runtime(
        &self,
        path: &Path,
        slices: &[&[u8]],
        now: chrono::DateTime<Local>,
        keep_open: bool,
        durable: bool,
    ) -> Result<(), FileManagerError> {
        let mut runtime = self
            .runtime
            .lock()
            .expect("file_manager runtime lock poisoned");
        self.append_slices_with_runtime_locked(&mut runtime, path, slices, now, keep_open, durable)
    }

    fn select_append_path_locked(
        &self,
        runtime: &mut RuntimeState,
        now: chrono::DateTime<Local>,
        dir: &Path,
        prefix: &str,
        max_file_size: u64,
        keep_open: bool,
    ) -> PathBuf {
        let now_ts = now.timestamp();
        let day_key = day_key(now);

        if keep_open {
            if let Some(active) = runtime.active_file.as_ref() {
                if active.day_key == day_key && active.path.parent() == Some(dir) {
                    let path = active.path.clone();
                    Self::record_last_append(runtime, now_ts, &path);
                    return path;
                }
            }
        }

        if let Some(last_ts) = runtime.last_append_time {
            if now_ts < last_ts {
                if let Some(path) = runtime.last_append_path.clone() {
                    return path;
                }
            }
        }
        if let Some(path) =
            self.cached_target_path(runtime, now_ts, dir, day_key, prefix, max_file_size)
        {
            return path;
        }

        let target = self.resolve_append_target(now, dir, prefix, max_file_size);
        self.set_cached_target(runtime, dir, target.clone());
        Self::record_last_append(runtime, now_ts, &target.path);
        target.path
    }

    fn append_slices_with_runtime_locked(
        &self,
        runtime: &mut RuntimeState,
        path: &Path,
        slices: &[&[u8]],
        now: chrono::DateTime<Local>,
        keep_open: bool,
        durable: bool,
    ) -> Result<(), FileManagerError> {
        debug_assert!(!(keep_open && durable));
        let day_key = day_key(now);
        let path_buf = path.to_path_buf();
        let active_matches = runtime
            .active_file
            .as_ref()
            .map(|active| active.path == path_buf && active.day_key == day_key)
            .unwrap_or(false);
        let cached_local_len = if active_matches {
            None
        } else {
            self.cached_local_len_for_path(runtime, &path_buf, day_key)
        };

        let must_reopen = runtime
            .active_file
            .as_ref()
            .map(|active| active.path != path_buf || active.day_key != day_key)
            .unwrap_or(false);
        if must_reopen {
            close_active_append_file(runtime)?;
        }
        if runtime.active_file.is_none() {
            let file = open_append_file(path, &path_buf)?;
            let len = match cached_local_len {
                Some(len) => len,
                None => file
                    .metadata()
                    .map_err(|e| FileManagerError::Metadata(path_buf.clone(), e))?
                    .len(),
            };
            runtime.active_file = Some(ActiveAppendFile {
                path: path_buf.clone(),
                day_key,
                logical_len: len,
                disk_len: len,
                buffered: keep_open,
                write_buffer: Vec::with_capacity(if keep_open {
                    ACTIVE_APPEND_BUFFER_CAPACITY
                } else {
                    0
                }),
                file,
            });
        }

        let before_len = runtime
            .active_file
            .as_ref()
            .expect("active file initialized")
            .logical_len;

        let written = slices.iter().map(|slice| slice.len() as u64).sum::<u64>();
        let append_begin = Instant::now();
        let result = {
            let active = runtime
                .active_file
                .as_mut()
                .expect("active file initialized");
            if keep_open {
                append_slices_keep_open(active, slices)?;
            } else {
                append_slices_direct(active, slices)?;
            }
            Ok::<u64, FileManagerError>(active.logical_len)
        };

        if let Ok(current_len) = result {
            record_file_append(written as usize, append_begin.elapsed(), keep_open);
            let merged_len =
                self.merged_len_after_append(path, runtime, day_key, written, current_len);
            self.update_cached_target_after_append(runtime, path, day_key, merged_len, current_len);
        }

        if result.is_ok() && durable {
            let active = runtime
                .active_file
                .as_mut()
                .expect("active file initialized");
            if let Err(e) = sync_active_append_file_data(active) {
                rollback_file_to_len(&mut active.file, before_len);
                active.disk_len = before_len;
                active.logical_len = before_len;
                active.write_buffer.clear();
                return Err(e);
            }
        }

        if !keep_open {
            close_active_append_file(runtime)?;
        }

        result.map(|_| ())
    }

    fn active_append_path(
        &self,
        now: chrono::DateTime<Local>,
        dir: &Path,
        keep_open: bool,
    ) -> Option<PathBuf> {
        if !keep_open {
            return None;
        }
        let mut runtime = self
            .runtime
            .lock()
            .expect("file_manager runtime lock poisoned");
        let active = runtime.active_file.as_ref()?;
        let active_day_key = active.day_key;
        let active_parent = active.path.parent().map(Path::to_path_buf);
        let active_path = active.path.clone();
        if active_day_key != day_key(now) {
            return None;
        }
        if active_parent.as_deref() != Some(dir) {
            return None;
        }
        runtime.last_append_time = Some(now.timestamp());
        runtime.last_append_path = Some(active_path.clone());
        Some(active_path)
    }

    fn make_path_for_time(
        &self,
        now: chrono::DateTime<Local>,
        dir: &Path,
        prefix: &str,
        max_file_size: u64,
    ) -> PathBuf {
        self.resolve_append_target(now, dir, prefix, max_file_size)
            .path
    }

    fn resolve_append_target(
        &self,
        now: chrono::DateTime<Local>,
        dir: &Path,
        prefix: &str,
        max_file_size: u64,
    ) -> AppendTargetCache {
        let day_key = day_key(now);
        let date_prefix = make_date_prefix_from_day_key(prefix, day_key);
        let (idx, merged_len) = if max_file_size == 0 {
            let path = build_path_for_index(dir, prefix, day_key, 0);
            let (local_exists, local_len) = local_file_state(&path);
            return AppendTargetCache {
                path,
                day_key,
                file_index: 0,
                merged_len: local_len,
                local_len,
                local_exists,
            };
        } else {
            self.next_file_index_state(&date_prefix, max_file_size)
        };

        let path = build_path_for_index(dir, prefix, day_key, idx);
        let (local_exists, local_len) = local_file_state(&path);
        AppendTargetCache {
            path,
            day_key,
            file_index: idx,
            merged_len,
            local_len,
            local_exists,
        }
    }

    fn cached_local_len_for_path(
        &self,
        runtime: &RuntimeState,
        path: &Path,
        day_key: i32,
    ) -> Option<u64> {
        for target in [runtime.log_target.as_ref(), runtime.cache_target.as_ref()]
            .into_iter()
            .flatten()
        {
            if target.day_key == day_key && target.path == path {
                return Some(target.local_len);
            }
        }
        None
    }

    fn next_file_index_state(&self, date_prefix: &str, max_file_size: u64) -> (i64, u64) {
        let mut names = self.get_file_names_by_prefix(&self.log_dir, date_prefix);
        if let Some(cache_dir) = &self.cache_dir {
            names.extend(self.get_file_names_by_prefix(cache_dir, date_prefix));
        }
        if names.is_empty() {
            return (0, 0);
        }

        names.sort_by(|a, b| {
            if a.len() == b.len() {
                b.cmp(a)
            } else {
                b.len().cmp(&a.len())
            }
        });
        let last = &names[0];

        let ext = format!(".{LOG_EXT}");
        let mut idx = 0i64;
        if let Some(base) = last.strip_suffix(&ext) {
            if let Some(rest) = base.strip_prefix(date_prefix) {
                let rest = rest.strip_prefix('_').unwrap_or(rest);
                idx = rest.parse::<i64>().unwrap_or(0);
            }
        }

        let mut merged_size = 0u64;
        let log_path = self.log_dir.join(last);
        if let Ok(meta) = fs::metadata(&log_path) {
            merged_size = merged_size.saturating_add(meta.len());
        }
        if let Some(cache_dir) = &self.cache_dir {
            let cache_path = cache_dir.join(last);
            if let Ok(meta) = fs::metadata(&cache_path) {
                merged_size = merged_size.saturating_add(meta.len());
            }
        }
        if merged_size > max_file_size {
            (idx + 1, 0)
        } else {
            (idx, merged_size)
        }
    }

    fn get_file_names_by_prefix(&self, dir: &Path, file_prefix: &str) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            if name.starts_with(file_prefix) && name.ends_with(&format!(".{LOG_EXT}")) {
                out.push(name.to_string());
            }
        }
        out
    }
}

fn make_date_prefix(prefix: &str, timespan: i32) -> String {
    let now = Local::now() - ChronoDuration::days(timespan as i64);
    format!(
        "{}_{:04}{:02}{:02}",
        prefix,
        now.year(),
        now.month(),
        now.day()
    )
}

fn make_date_prefix_from_day_key(prefix: &str, day_key: i32) -> String {
    let year = day_key / 10_000;
    let month = (day_key / 100) % 100;
    let day = day_key % 100;
    format!("{prefix}_{year:04}{month:02}{day:02}")
}

fn build_path_for_index(dir: &Path, prefix: &str, day_key: i32, file_index: i64) -> PathBuf {
    let date_prefix = make_date_prefix_from_day_key(prefix, day_key);
    let file_name = if file_index == 0 {
        format!("{date_prefix}.{LOG_EXT}")
    } else {
        format!("{date_prefix}_{file_index}.{LOG_EXT}")
    };
    dir.join(file_name)
}

fn local_file_state(path: &Path) -> (bool, u64) {
    match fs::metadata(path) {
        Ok(meta) => (true, meta.len()),
        Err(_) => (false, 0),
    }
}

fn file_index_from_path(path: &Path, prefix: &str) -> Option<i64> {
    let name = path.file_name()?.to_str()?;
    let ext = format!(".{LOG_EXT}");
    let base = name.strip_suffix(&ext)?;
    let prefix_part = format!("{prefix}_");
    if !name.starts_with(&prefix_part) {
        return None;
    }
    let rest = &base[prefix_part.len() + 8..];
    if rest.is_empty() {
        Some(0)
    } else {
        rest.strip_prefix('_')?.parse().ok()
    }
}

fn day_key(now: chrono::DateTime<Local>) -> i32 {
    now.year() * 10_000 + (now.month() as i32) * 100 + now.day() as i32
}

fn append_file_to_file(src: &Path, dst: &Path) -> Result<(), FileManagerError> {
    if src == dst || !src.exists() {
        return Ok(());
    }
    let src_meta =
        fs::metadata(src).map_err(|e| FileManagerError::Metadata(src.to_path_buf(), e))?;
    if src_meta.len() == 0 {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| FileManagerError::CreateDir(parent.to_path_buf(), e))?;
    }

    let mut src_file =
        File::open(src).map_err(|e| FileManagerError::OpenFile(src.to_path_buf(), e))?;
    let mut dst_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dst)
        .map_err(|e| FileManagerError::OpenFile(dst.to_path_buf(), e))?;
    let dst_before_len = dst_file
        .metadata()
        .map_err(|e| FileManagerError::Metadata(dst.to_path_buf(), e))?
        .len();

    let mut buf = vec![0u8; FILE_COPY_BUFFER_SIZE];
    let mut copied = 0u64;
    loop {
        let n = match src_file.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                rollback_file_to_len(&mut dst_file, dst_before_len);
                return Err(FileManagerError::ReadFile(src.to_path_buf(), e));
            }
        };
        if n == 0 {
            break;
        }
        if let Err(e) = dst_file.write_all(&buf[..n]) {
            rollback_file_to_len(&mut dst_file, dst_before_len);
            return Err(FileManagerError::WriteFile(dst.to_path_buf(), e));
        }
        copied = copied.saturating_add(n as u64);
    }
    if copied < src_meta.len() {
        rollback_file_to_len(&mut dst_file, dst_before_len);
        return Err(FileManagerError::WriteFile(
            dst.to_path_buf(),
            std::io::Error::new(std::io::ErrorKind::WriteZero, "partial append"),
        ));
    }
    if let Err(e) = dst_file.sync_data() {
        rollback_file_to_len(&mut dst_file, dst_before_len);
        return Err(FileManagerError::SyncFile(dst.to_path_buf(), e));
    }
    Ok(())
}

fn close_active_append_file(runtime: &mut RuntimeState) -> Result<(), FileManagerError> {
    if let Some(active) = runtime.active_file.as_mut() {
        flush_active_append_file(active)?;
    }
    runtime.active_file = None;
    Ok(())
}

fn append_slices_keep_open(
    active: &mut ActiveAppendFile,
    slices: &[&[u8]],
) -> Result<(), FileManagerError> {
    let incoming = slices.iter().map(|slice| slice.len()).sum::<usize>();
    if incoming == 0 {
        return Ok(());
    }

    if !active.buffered {
        active.buffered = true;
        active
            .write_buffer
            .reserve(ACTIVE_APPEND_BUFFER_CAPACITY.max(incoming));
    }

    if !active.write_buffer.is_empty()
        && active.write_buffer.len().saturating_add(incoming) >= ACTIVE_APPEND_BUFFER_CAPACITY
    {
        flush_active_append_file(active)?;
    }

    if incoming >= ACTIVE_APPEND_BUFFER_CAPACITY {
        append_slices_direct(active, slices)?;
        return Ok(());
    }

    for slice in slices {
        if slice.is_empty() {
            continue;
        }
        active.write_buffer.extend_from_slice(slice);
        active.logical_len = active.logical_len.saturating_add(slice.len() as u64);
    }
    Ok(())
}

fn append_slices_direct(
    active: &mut ActiveAppendFile,
    slices: &[&[u8]],
) -> Result<(), FileManagerError> {
    if !active.write_buffer.is_empty() {
        flush_active_append_file(active)?;
    }

    let before_len = active.disk_len;
    match write_all_slices_vectored(&mut active.file, slices) {
        Ok(written) => {
            active.disk_len = before_len.saturating_add(written);
            active.logical_len = active.disk_len;
            Ok(())
        }
        Err(e) => {
            rollback_file_to_len(&mut active.file, before_len);
            Err(FileManagerError::WriteFile(active.path.clone(), e))
        }
    }
}

fn write_all_slices_vectored(file: &mut File, slices: &[&[u8]]) -> std::io::Result<u64> {
    let mut total_written = 0u64;
    let mut slice_idx = 0usize;
    let mut slice_offset = 0usize;

    while slice_idx < slices.len() {
        while slice_idx < slices.len() && slice_offset >= slices[slice_idx].len() {
            slice_idx += 1;
            slice_offset = 0;
        }
        if slice_idx >= slices.len() {
            break;
        }

        let mut iovecs = Vec::with_capacity(slices.len().saturating_sub(slice_idx));
        let first = &slices[slice_idx][slice_offset..];
        if !first.is_empty() {
            iovecs.push(IoSlice::new(first));
        }
        for slice in &slices[slice_idx + 1..] {
            if !slice.is_empty() {
                iovecs.push(IoSlice::new(slice));
            }
        }
        if iovecs.is_empty() {
            break;
        }

        let written = loop {
            match file.write_vectored(&iovecs) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "write_vectored returned 0",
                    ));
                }
                Ok(n) => break n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        };
        total_written = total_written.saturating_add(written as u64);

        let mut remaining = written;
        while slice_idx < slices.len() {
            let current = &slices[slice_idx];
            if slice_offset >= current.len() {
                slice_idx += 1;
                slice_offset = 0;
                continue;
            }
            let available = current.len() - slice_offset;
            if remaining < available {
                slice_offset += remaining;
                break;
            }
            remaining -= available;
            slice_idx += 1;
            slice_offset = 0;
            if remaining == 0 {
                break;
            }
        }
    }

    Ok(total_written)
}

fn flush_active_append_file(active: &mut ActiveAppendFile) -> Result<(), FileManagerError> {
    if active.write_buffer.is_empty() {
        return Ok(());
    }

    let before_len = active.disk_len;
    if let Err(e) = active.file.write_all(&active.write_buffer) {
        rollback_file_to_len(&mut active.file, before_len);
        return Err(FileManagerError::WriteFile(active.path.clone(), e));
    }
    active.disk_len = active.logical_len;
    active.write_buffer.clear();
    Ok(())
}

fn sync_active_append_file_data(active: &mut ActiveAppendFile) -> Result<(), FileManagerError> {
    flush_active_append_file(active)?;
    active
        .file
        .sync_data()
        .map_err(|e| FileManagerError::SyncFile(active.path.clone(), e))
}

fn rollback_file_to_len(file: &mut File, target_len: u64) {
    let _ = file.set_len(target_len);
    let _ = file.seek(SeekFrom::Start(target_len));
}

fn open_append_file(path: &Path, path_buf: &Path) -> Result<File, FileManagerError> {
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => Ok(file),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let Some(parent) = path.parent() else {
                return Err(FileManagerError::OpenFile(path_buf.to_path_buf(), err));
            };
            fs::create_dir_all(parent)
                .map_err(|e| FileManagerError::CreateDir(parent.to_path_buf(), e))?;
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| FileManagerError::OpenFile(path_buf.to_path_buf(), e))
        }
        Err(err) => Err(FileManagerError::OpenFile(path_buf.to_path_buf(), err)),
    }
}

fn file_mtime(path: &Path) -> Result<SystemTime, FileManagerError> {
    let meta = fs::metadata(path).map_err(|e| FileManagerError::Metadata(path.to_path_buf(), e))?;
    meta.modified()
        .map_err(|e| FileManagerError::Metadata(path.to_path_buf(), e))
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::fs::OpenOptions;
    use std::process::Command;
    use std::time::{Duration, SystemTime};

    use super::{build_path_for_index, day_key, ActiveAppendFile, AppendTargetCache, FileManager};
    use chrono::{Datelike, Local};
    use filetime::{set_file_mtime, FileTime};

    #[test]
    fn filepaths_from_timespan_keeps_log_then_cache_order() {
        let root = tempfile::tempdir().unwrap();
        let log_dir = root.path().join("log");
        let cache_dir = root.path().join("cache");
        let manager = FileManager::new(
            log_dir.clone(),
            Some(cache_dir.clone()),
            "demo".to_string(),
            1,
        )
        .unwrap();

        let now = Local::now();
        let file_name = format!(
            "demo_{:04}{:02}{:02}.xlog",
            now.year(),
            now.month(),
            now.day()
        );
        std::fs::write(log_dir.join(&file_name), b"log").unwrap();
        std::fs::write(cache_dir.join(&file_name), b"cache").unwrap();

        let paths = manager.filepaths_from_timespan(0, "demo");
        assert_eq!(paths.len(), 2);
        assert!(paths[0].starts_with(log_dir.to_string_lossy().as_ref()));
        assert!(paths[1].starts_with(cache_dir.to_string_lossy().as_ref()));
    }

    #[test]
    fn keep_open_flushes_buffer_when_closed() {
        let root = tempfile::tempdir().unwrap();
        let log_dir = root.path().join("log");
        let manager = FileManager::new(log_dir.clone(), None, "demo".to_string(), 0).unwrap();

        manager.append_log_bytes(b"aaaa", 0, false, true).unwrap();
        manager.append_log_bytes(b"bbbb", 0, false, true).unwrap();
        manager.append_log_bytes(b"cccc", 0, false, false).unwrap();

        let entries = std::fs::read_dir(&log_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().and_then(std::ffi::OsStr::to_str) == Some("xlog"))
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        assert_eq!(std::fs::read(&entries[0]).unwrap(), b"aaaabbbbcccc");
    }

    #[test]
    fn close_after_write_still_rotates_by_size() {
        let root = tempfile::tempdir().unwrap();
        let log_dir = root.path().join("log");
        let manager = FileManager::new(log_dir.clone(), None, "demo".to_string(), 0).unwrap();

        manager.append_log_bytes(b"aaaa", 1, false, false).unwrap();
        manager.append_log_bytes(b"bbbb", 1, false, false).unwrap();

        let mut entries = std::fs::read_dir(&log_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().and_then(std::ffi::OsStr::to_str) == Some("xlog"))
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn append_log_slices_writes_segments_in_order() {
        let root = tempfile::tempdir().unwrap();
        let log_dir = root.path().join("log");
        let manager = FileManager::new(log_dir.clone(), None, "demo".to_string(), 0).unwrap();

        manager
            .append_log_slices(&[b"hello", b"-", b"world"], 0, false, false)
            .unwrap();

        let entry = std::fs::read_dir(&log_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().and_then(std::ffi::OsStr::to_str) == Some("xlog"))
            .unwrap();
        assert_eq!(std::fs::read(entry).unwrap(), b"hello-world");
    }

    #[test]
    fn cached_target_advances_to_next_split_file_without_rescan() {
        let root = tempfile::tempdir().unwrap();
        let log_dir = root.path().join("log");
        let manager = FileManager::new(log_dir.clone(), None, "demo".to_string(), 0).unwrap();

        manager.append_log_bytes(b"aaaa", 1, false, false).unwrap();

        let next = manager.select_append_path(Local::now(), &log_dir, "demo", 1);
        let file_name = next.file_name().and_then(std::ffi::OsStr::to_str).unwrap();
        assert!(file_name.ends_with("_1.xlog"));

        let runtime = manager.runtime.lock().unwrap();
        let target = runtime.log_target.as_ref().unwrap();
        assert_eq!(target.file_index, 1);
        assert_eq!(target.local_len, 0);
        assert_eq!(target.merged_len, 0);
        assert!(!target.local_exists);
    }

    #[test]
    fn keep_open_reuses_active_cache_file_without_rerouting() {
        let root = tempfile::tempdir().unwrap();
        let log_dir = root.path().join("log");
        let cache_dir = root.path().join("cache");
        let manager = FileManager::new(
            log_dir.clone(),
            Some(cache_dir.clone()),
            "demo".to_string(),
            1,
        )
        .unwrap();

        let now = Local::now();
        let day = day_key(now);
        let cache_path = build_path_for_index(&cache_dir, "demo", day, 0);
        std::fs::write(&cache_path, b"aaaa").unwrap();

        {
            let mut runtime = manager.runtime.lock().unwrap();
            runtime.active_file = Some(ActiveAppendFile {
                path: cache_path.clone(),
                day_key: day,
                logical_len: 4,
                disk_len: 4,
                buffered: true,
                write_buffer: Vec::with_capacity(super::ACTIVE_APPEND_BUFFER_CAPACITY),
                file: OpenOptions::new().append(true).open(&cache_path).unwrap(),
            });
            runtime.cache_target = Some(AppendTargetCache {
                path: root.path().join("bogus").join("demo_stale.xlog"),
                day_key: day,
                file_index: 0,
                merged_len: 0,
                local_len: 0,
                local_exists: true,
            });
            runtime.log_target = Some(AppendTargetCache {
                path: root.path().join("bogus").join("demo_log_stale.xlog"),
                day_key: day,
                file_index: 0,
                merged_len: 0,
                local_len: 0,
                local_exists: true,
            });
        }

        manager.append_log_bytes(b"bbbb", 0, false, true).unwrap();
        manager.append_log_bytes(b"cccc", 0, false, false).unwrap();

        let cache_entries = std::fs::read_dir(&cache_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(cache_entries, vec![cache_path.clone()]);
        assert_eq!(std::fs::read(&cache_path).unwrap(), b"aaaabbbbcccc");
        assert!(!root.path().join("bogus").exists());
        let has_logs = std::fs::read_dir(&log_dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| {
                entry.path().extension().and_then(std::ffi::OsStr::to_str) == Some("xlog")
            });
        assert!(!has_logs);
    }

    #[test]
    fn append_moves_existing_cache_file_into_log_when_caching_is_disabled() {
        let root = tempfile::tempdir().unwrap();
        let log_dir = root.path().join("log");
        let cache_dir = root.path().join("cache");
        let manager = FileManager::new(
            log_dir.clone(),
            Some(cache_dir.clone()),
            "demo".to_string(),
            0,
        )
        .unwrap();

        let now = Local::now();
        let day = day_key(now);
        let cache_path = build_path_for_index(&cache_dir, "demo", day, 0);
        std::fs::write(&cache_path, b"cached-").unwrap();

        manager.append_log_bytes(b"tail", 0, true, false).unwrap();

        assert!(!cache_path.exists());
        let log_path = build_path_for_index(&log_dir, "demo", day, 0);
        assert_eq!(std::fs::read(&log_path).unwrap(), b"cached-tail");

        let runtime = manager.runtime.lock().unwrap();
        let cache_target = runtime.cache_target.as_ref().unwrap();
        assert_eq!(cache_target.path, cache_path);
        assert!(!cache_target.local_exists);
        assert_eq!(cache_target.local_len, 0);
    }

    #[test]
    fn append_recreates_missing_parent_dir_on_open() {
        let root = tempfile::tempdir().unwrap();
        let log_dir = root.path().join("log");
        let manager = FileManager::new(log_dir.clone(), None, "demo".to_string(), 0).unwrap();

        std::fs::remove_dir_all(&log_dir).unwrap();
        manager.append_log_bytes(b"hello", 0, false, false).unwrap();

        let entry = std::fs::read_dir(&log_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(std::fs::read(entry).unwrap(), b"hello");
    }

    #[test]
    fn move_old_cache_files_only_moves_eligible_logs() {
        let root = tempfile::tempdir().unwrap();
        let log_dir = root.path().join("log");
        let cache_dir = root.path().join("cache");
        let manager = FileManager::new(
            log_dir.clone(),
            Some(cache_dir.clone()),
            "demo".to_string(),
            1,
        )
        .unwrap();

        let old_cache_log = cache_dir.join("demo_legacy.xlog");
        let recent_cache_log = cache_dir.join("demo_recent.xlog");
        let ignored_file = cache_dir.join("notes.txt");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(&old_cache_log, b"old-cache").unwrap();
        fs::write(&recent_cache_log, b"recent-cache").unwrap();
        fs::write(&ignored_file, b"skip").unwrap();

        let old =
            FileTime::from_system_time(SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60));
        let recent =
            FileTime::from_system_time(SystemTime::now() - Duration::from_secs(6 * 60 * 60));
        set_file_mtime(&old_cache_log, old).unwrap();
        set_file_mtime(&recent_cache_log, recent).unwrap();

        manager.move_old_cache_files(0).unwrap();

        assert!(!old_cache_log.exists());
        assert_eq!(
            fs::read(log_dir.join("demo_legacy.xlog")).unwrap(),
            b"old-cache"
        );
        assert!(recent_cache_log.exists());
        assert!(ignored_file.exists());
    }

    #[test]
    fn delete_expired_files_removes_old_logs_and_day_dirs_only() {
        let root = tempfile::tempdir().unwrap();
        let log_dir = root.path().join("log");
        let cache_dir = root.path().join("cache");
        let manager = FileManager::new(
            log_dir.clone(),
            Some(cache_dir.clone()),
            "demo".to_string(),
            1,
        )
        .unwrap();

        fs::create_dir_all(&log_dir).unwrap();
        fs::create_dir_all(&cache_dir).unwrap();

        let old_log = log_dir.join("demo_old.xlog");
        let old_other = log_dir.join("keep.txt");
        let recent_log = cache_dir.join("demo_recent.xlog");
        let old_day_dir = cache_dir.join("20240101");
        let other_dir = cache_dir.join("misc");

        fs::write(&old_log, b"old-log").unwrap();
        fs::write(&old_other, b"other").unwrap();
        fs::write(&recent_log, b"recent-log").unwrap();
        fs::create_dir_all(&old_day_dir).unwrap();
        fs::write(old_day_dir.join("nested.txt"), b"nested").unwrap();
        fs::create_dir_all(&other_dir).unwrap();

        let old =
            FileTime::from_system_time(SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60));
        let recent = FileTime::from_system_time(SystemTime::now() - Duration::from_secs(30 * 60));
        set_file_mtime(&old_log, old).unwrap();
        set_file_mtime(&old_other, old).unwrap();
        set_file_mtime(&recent_log, recent).unwrap();
        set_file_mtime(&old_day_dir, old).unwrap();
        set_file_mtime(&other_dir, old).unwrap();

        manager.delete_expired_files(24 * 60 * 60).unwrap();

        assert!(!old_log.exists());
        assert!(old_other.exists());
        assert!(recent_log.exists());
        assert!(!old_day_dir.exists());
        assert!(other_dir.exists());
    }

    #[test]
    fn file_manager_lock_rejects_second_process() {
        if env::var("XLOG_LOCK_CHILD").ok().as_deref() == Some("1") {
            let dir = env::var("XLOG_LOCK_DIR").unwrap();
            let prefix = env::var("XLOG_LOCK_PREFIX").unwrap();
            let res = FileManager::new(dir.into(), None, prefix, 0);
            assert!(res.is_err());
            return;
        }

        let root = tempfile::tempdir().unwrap();
        let prefix = "locktest".to_string();
        let _first = FileManager::new(root.path().to_path_buf(), None, prefix.clone(), 0).unwrap();

        let exe = env::current_exe().unwrap();
        let status = Command::new(exe)
            .arg("--exact")
            .arg("file_manager_lock_rejects_second_process")
            .arg("--nocapture")
            .env("XLOG_LOCK_CHILD", "1")
            .env("XLOG_LOCK_DIR", root.path().to_string_lossy().to_string())
            .env("XLOG_LOCK_PREFIX", prefix)
            .status()
            .unwrap();

        assert!(status.success());
    }
}
