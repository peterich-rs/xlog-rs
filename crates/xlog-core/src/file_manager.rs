use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::{Duration as ChronoDuration, Local};
use fs2::{available_space, FileExt};
use thiserror::Error;

use crate::active_append::{
    append_slices_direct, append_slices_keep_open, flush_active_append_file, rollback_file_to_len,
    sync_active_append_file_data, ActiveAppendFile, ACTIVE_APPEND_BUFFER_CAPACITY,
};
use crate::file_maintenance::{delete_expired_files, move_old_cache_files, CacheMaintenance};
use crate::file_naming::{day_key, make_date_prefix, LOG_EXT_WITH_DOT};
use crate::file_ops::append_file_to_file;
use crate::file_policy::{AppendRoutePlan, CacheRoutePlanner};
use crate::file_runtime::{RuntimeState, TargetDirs};
use crate::file_target::resolve_append_target;
use crate::metrics::record_file_append;
const CACHE_AVAILABLE_THRESHOLD_BYTES: u64 = 1024 * 1024 * 1024;

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
    _lock_files: Arc<Vec<File>>,
}

impl FileManager {
    fn target_dirs(&self) -> TargetDirs<'_> {
        TargetDirs::new(self.log_dir.as_path(), self.cache_dir.as_deref())
    }

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

        let mut lock_files = Vec::new();
        for lock_path in lock_paths(&log_dir, cache_dir.as_deref(), &name_prefix) {
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
            lock_files.push(lock_file);
        }

        Ok(Self {
            log_dir,
            cache_dir,
            name_prefix,
            cache_days,
            runtime: Arc::new(Mutex::new(RuntimeState::default())),
            _lock_files: Arc::new(lock_files),
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

        let now = Local::now();
        if self.cache_dir.is_none() {
            return self.append_log_slices_plain(slices, now, max_file_size, keep_open, durable);
        }

        self.append_log_slices_with_cache(slices, now, max_file_size, move_file, keep_open, durable)
    }

    /// Moves eligible cache files into the primary log directory.
    ///
    /// Files newer than `cache_days` are left in place. When no cache directory is
    /// configured, this is a no-op.
    pub fn move_old_cache_files(&self, _max_file_size: u64) -> Result<(), FileManagerError> {
        self.flush_active_file_if_needed()?;
        for path in move_old_cache_files(CacheMaintenance {
            log_dir: &self.log_dir,
            cache_dir: self.cache_dir.as_deref(),
            name_prefix: &self.name_prefix,
            cache_days: self.cache_days,
        })? {
            self.mark_runtime_path_removed(&path);
        }

        Ok(())
    }

    /// Deletes log and cache files whose modification time exceeds `max_alive_seconds`.
    pub fn delete_expired_files(&self, max_alive_seconds: i64) -> Result<(), FileManagerError> {
        self.flush_active_file_if_needed()?;
        for path in
            delete_expired_files(&self.log_dir, self.cache_dir.as_deref(), max_alive_seconds)?
        {
            self.mark_runtime_path_removed(&path);
        }
        Ok(())
    }

    /// Flushes any buffered bytes held by the keep-open append path.
    pub fn flush_active_file_buffer(&self) -> Result<(), FileManagerError> {
        self.flush_active_file_if_needed()
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
            if name.starts_with(file_prefix) && name.ends_with(LOG_EXT_WITH_DOT) {
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

    fn cached_local_exists(
        &self,
        now: chrono::DateTime<Local>,
        dir: &Path,
        max_file_size: u64,
    ) -> Option<bool> {
        let mut runtime = self
            .runtime
            .lock()
            .expect("file_manager runtime lock poisoned");
        runtime.cached_local_exists_for_day(self.target_dirs(), dir, day_key(now), max_file_size)
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

    fn mark_runtime_path_removed(&self, path: &Path) {
        let mut runtime = self
            .runtime
            .lock()
            .expect("file_manager runtime lock poisoned");
        runtime.mark_path_removed(path);
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

    fn append_log_slices_plain(
        &self,
        slices: &[&[u8]],
        now: chrono::DateTime<Local>,
        max_file_size: u64,
        keep_open: bool,
        durable: bool,
    ) -> Result<(), FileManagerError> {
        let mut runtime = self
            .runtime
            .lock()
            .expect("file_manager runtime lock poisoned");
        if keep_open
            && self.try_append_active_plain_keep_open(&mut runtime, slices, now, max_file_size)?
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
        self.append_slices_with_runtime_locked(&mut runtime, &path, slices, now, keep_open, durable)
    }

    fn append_log_slices_with_cache(
        &self,
        slices: &[&[u8]],
        now: chrono::DateTime<Local>,
        max_file_size: u64,
        move_file: bool,
        keep_open: bool,
        durable: bool,
    ) -> Result<(), FileManagerError> {
        match self.plan_cache_append(now, max_file_size, move_file, keep_open) {
            AppendRoutePlan::AppendToPath {
                path,
                promote_after_append,
            } => {
                self.append_slices_with_runtime(&path, slices, now, keep_open, durable)?;
                if promote_after_append {
                    self.promote_cache_path_to_log(&path, now, max_file_size)?;
                }
                Ok(())
            }
            AppendRoutePlan::PreferLogThenCache {
                log_path,
                cache_path,
            } => {
                match self.append_slices_with_runtime(&log_path, slices, now, keep_open, durable) {
                    Ok(()) => Ok(()),
                    Err(_) => self.append_slices_with_runtime(
                        &cache_path,
                        slices,
                        now,
                        keep_open,
                        durable,
                    ),
                }
            }
        }
    }

    fn plan_cache_append(
        &self,
        now: chrono::DateTime<Local>,
        max_file_size: u64,
        move_file: bool,
        keep_open: bool,
    ) -> AppendRoutePlan {
        if let Some(log_path) = self.active_append_path(now, &self.log_dir, keep_open) {
            return CacheRoutePlanner::new(false, move_file).active_log(log_path);
        }
        let cache_dir = self.cache_dir.as_deref().expect("cache_dir is_some");
        let planner = CacheRoutePlanner::new(self.should_cache_logs(now, max_file_size), move_file);
        if let Some(cache_path) = self.active_append_path(now, cache_dir, keep_open) {
            return planner.active_cache(cache_path);
        }

        let cache_path = self.select_append_path(now, cache_dir, &self.name_prefix, max_file_size);
        let cache_exists = self
            .cached_local_exists(now, cache_dir, max_file_size)
            .unwrap_or_else(|| cache_path.exists());

        if let Some(plan) = planner.resolved_cache(cache_path.clone(), cache_exists) {
            return plan;
        }

        let log_path =
            self.select_append_path(now, &self.log_dir, &self.name_prefix, max_file_size);
        planner.fallback(log_path, cache_path)
    }

    fn promote_cache_path_to_log(
        &self,
        cache_path: &Path,
        now: chrono::DateTime<Local>,
        max_file_size: u64,
    ) -> Result<(), FileManagerError> {
        let log_path =
            self.select_append_path(now, &self.log_dir, &self.name_prefix, max_file_size);
        append_file_to_file(cache_path, &log_path)?;
        fs::remove_file(cache_path)
            .map_err(|e| FileManagerError::RemoveFile(cache_path.to_path_buf(), e))?;
        self.mark_runtime_path_removed(cache_path);
        Ok(())
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
                    runtime.record_last_append(now_ts, &path);
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
        if let Some(path) = runtime.next_cached_path(
            self.target_dirs(),
            now_ts,
            dir,
            day_key,
            prefix,
            max_file_size,
        ) {
            return path;
        }

        let target = resolve_append_target(
            self.log_dir.as_path(),
            self.cache_dir.as_deref(),
            now,
            dir,
            prefix,
            max_file_size,
        );
        runtime.set_target_for_dir(self.target_dirs(), dir, target.clone());
        runtime.record_last_append(now_ts, &target.path);
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
            runtime.cached_local_len_for_path(&path_buf, day_key)
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
            let merged_len = runtime.merged_len_after_append(path, day_key, written, current_len);
            runtime.update_target_after_append(
                self.target_dirs(),
                &self.name_prefix,
                path,
                day_key,
                merged_len,
                current_len,
            );
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
        resolve_append_target(
            self.log_dir.as_path(),
            self.cache_dir.as_deref(),
            now,
            dir,
            prefix,
            max_file_size,
        )
        .path
    }
}

fn lock_paths(log_dir: &Path, cache_dir: Option<&Path>, prefix: &str) -> Vec<PathBuf> {
    let mut dirs = vec![log_dir.to_path_buf()];
    if let Some(cache_dir) = cache_dir {
        if cache_dir != log_dir {
            dirs.push(cache_dir.to_path_buf());
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs.into_iter()
        .map(|dir| dir.join(format!("{prefix}.lock")))
        .collect()
}

fn close_active_append_file(runtime: &mut RuntimeState) -> Result<(), FileManagerError> {
    if let Some(active) = runtime.active_file.as_mut() {
        flush_active_append_file(active)?;
    }
    runtime.active_file = None;
    Ok(())
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

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::fs::OpenOptions;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{Duration, SystemTime};

    use super::{ActiveAppendFile, FileManager};
    use crate::file_naming::{build_path_for_index, day_key};
    use crate::file_runtime::AppendTargetCache;
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
            .filter(|path| path.extension().and_then(std::ffi::OsStr::to_str) == Some("xlog"))
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
            let cache_dir = env::var("XLOG_LOCK_CACHE_DIR")
                .ok()
                .filter(|path| !path.is_empty())
                .map(PathBuf::from);
            let res = FileManager::new(dir.into(), cache_dir, prefix, 0);
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

    #[test]
    fn file_manager_lock_rejects_shared_cache_dir_across_processes() {
        if env::var("XLOG_LOCK_CHILD").ok().as_deref() == Some("1") {
            let dir = env::var("XLOG_LOCK_DIR").unwrap();
            let prefix = env::var("XLOG_LOCK_PREFIX").unwrap();
            let cache_dir = env::var("XLOG_LOCK_CACHE_DIR")
                .ok()
                .filter(|path| !path.is_empty())
                .map(PathBuf::from);
            let res = FileManager::new(dir.into(), cache_dir, prefix, 0);
            assert!(res.is_err());
            return;
        }

        let root = tempfile::tempdir().unwrap();
        let cache_dir = root.path().join("cache");
        let log_dir_a = root.path().join("log-a");
        let log_dir_b = root.path().join("log-b");
        let prefix = "cachelock".to_string();
        let _first = FileManager::new(
            log_dir_a.clone(),
            Some(cache_dir.clone()),
            prefix.clone(),
            0,
        )
        .unwrap();

        let exe = env::current_exe().unwrap();
        let status = Command::new(exe)
            .arg("--exact")
            .arg("file_manager_lock_rejects_shared_cache_dir_across_processes")
            .arg("--nocapture")
            .env("XLOG_LOCK_CHILD", "1")
            .env("XLOG_LOCK_DIR", log_dir_b.to_string_lossy().to_string())
            .env(
                "XLOG_LOCK_CACHE_DIR",
                cache_dir.to_string_lossy().to_string(),
            )
            .env("XLOG_LOCK_PREFIX", prefix)
            .status()
            .unwrap();

        assert!(status.success());
    }
}
