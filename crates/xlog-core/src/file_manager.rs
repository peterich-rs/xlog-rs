use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use chrono::{Datelike, Duration as ChronoDuration, Local};
use fs2::available_space;
use thiserror::Error;

const LOG_EXT: &str = "xlog";
const CACHE_AVAILABLE_THRESHOLD_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum FileManagerError {
    #[error("log_dir must be non-empty")]
    EmptyLogDir,
    #[error("name_prefix must be non-empty")]
    EmptyNamePrefix,
    #[error("create directory failed for {0}: {1}")]
    CreateDir(PathBuf, #[source] std::io::Error),
    #[error("read directory failed for {0}: {1}")]
    ReadDir(PathBuf, #[source] std::io::Error),
    #[error("file metadata failed for {0}: {1}")]
    Metadata(PathBuf, #[source] std::io::Error),
    #[error("open file failed for {0}: {1}")]
    OpenFile(PathBuf, #[source] std::io::Error),
    #[error("write file failed for {0}: {1}")]
    WriteFile(PathBuf, #[source] std::io::Error),
    #[error("read file failed for {0}: {1}")]
    ReadFile(PathBuf, #[source] std::io::Error),
    #[error("remove file failed for {0}: {1}")]
    RemoveFile(PathBuf, #[source] std::io::Error),
    #[error("remove directory failed for {0}: {1}")]
    RemoveDir(PathBuf, #[source] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct FileManager {
    log_dir: PathBuf,
    cache_dir: Option<PathBuf>,
    name_prefix: String,
    cache_days: i32,
    runtime: Arc<Mutex<RuntimeState>>,
}

#[derive(Debug, Default)]
struct RuntimeState {
    last_append_time: Option<i64>,
    last_append_path: Option<PathBuf>,
}

impl FileManager {
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

        Ok(Self {
            log_dir,
            cache_dir,
            name_prefix,
            cache_days,
            runtime: Arc::new(Mutex::new(RuntimeState::default())),
        })
    }

    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    pub fn cache_dir(&self) -> Option<&Path> {
        self.cache_dir.as_deref()
    }

    pub fn name_prefix(&self) -> &str {
        &self.name_prefix
    }

    pub fn cache_days(&self) -> i32 {
        self.cache_days
    }

    pub fn mmap_path(&self) -> PathBuf {
        let base = self.cache_dir.as_ref().unwrap_or(&self.log_dir);
        base.join(format!("{}.mmap3", self.name_prefix))
    }

    pub fn filepaths_from_timespan(&self, timespan: i32, prefix: &str) -> Vec<String> {
        let file_prefix = make_date_prefix(prefix, timespan);

        let mut out = Vec::new();
        out.extend(self.list_existing_files(&self.log_dir, &file_prefix));
        if let Some(cache_dir) = &self.cache_dir {
            out.extend(self.list_existing_files(cache_dir, &file_prefix));
        }
        out
    }

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

    pub fn append_log_bytes(
        &self,
        bytes: &[u8],
        max_file_size: u64,
        move_file: bool,
    ) -> Result<(), FileManagerError> {
        if bytes.is_empty() {
            return Ok(());
        }

        if self.cache_dir.is_none() {
            let now = Local::now();
            let path =
                self.select_append_path(now, &self.log_dir, &self.name_prefix, max_file_size);
            return append_bytes(&path, bytes);
        }

        let cache_dir = self.cache_dir.as_ref().expect("cache_dir is_some");
        let now = Local::now();
        let cache_path = self.select_append_path(now, cache_dir, &self.name_prefix, max_file_size);
        let cache_logs = self.should_cache_logs(now, max_file_size);

        if cache_logs || cache_path.exists() {
            append_bytes(&cache_path, bytes)?;
            if cache_logs || !move_file {
                return Ok(());
            }

            let log_path =
                self.select_append_path(now, &self.log_dir, &self.name_prefix, max_file_size);
            append_file_to_file(&cache_path, &log_path)?;
            fs::remove_file(&cache_path)
                .map_err(|e| FileManagerError::RemoveFile(cache_path, e))?;
            return Ok(());
        }

        let log_path =
            self.select_append_path(now, &self.log_dir, &self.name_prefix, max_file_size);
        match append_bytes(&log_path, bytes) {
            Ok(()) => Ok(()),
            Err(_) => append_bytes(&cache_path, bytes),
        }
    }

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
        }

        Ok(())
    }

    pub fn delete_expired_files(&self, max_alive_seconds: i64) -> Result<(), FileManagerError> {
        if max_alive_seconds <= 0 {
            return Ok(());
        }
        let threshold = Duration::from_secs(max_alive_seconds as u64);
        self.delete_expired_under(&self.log_dir, threshold)?;
        if let Some(cache_dir) = &self.cache_dir {
            self.delete_expired_under(cache_dir, threshold)?;
        }
        Ok(())
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
                }
                continue;
            }

            if path.is_dir() {
                let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
                let is_day_folder = name.len() == 8 && name.chars().all(|c| c.is_ascii_digit());
                if is_day_folder {
                    fs::remove_dir_all(&path).map_err(|e| FileManagerError::RemoveDir(path, e))?;
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

    fn should_cache_logs(&self, now: chrono::DateTime<Local>, max_file_size: u64) -> bool {
        let Some(cache_dir) = &self.cache_dir else {
            return false;
        };
        if self.cache_days <= 0 {
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
        let now_ts = now.timestamp();
        if let Ok(mut runtime) = self.runtime.lock() {
            if let Some(last_ts) = runtime.last_append_time {
                if now_ts < last_ts {
                    if let Some(path) = runtime.last_append_path.clone() {
                        return path;
                    }
                }
            }
            let path = self.make_path_for_time(now, dir, prefix, max_file_size);
            runtime.last_append_time = Some(now_ts);
            runtime.last_append_path = Some(path.clone());
            return path;
        }
        self.make_path_for_time(now, dir, prefix, max_file_size)
    }

    fn make_path_for_time(
        &self,
        now: chrono::DateTime<Local>,
        dir: &Path,
        prefix: &str,
        max_file_size: u64,
    ) -> PathBuf {
        let date_prefix = format!(
            "{}_{:04}{:02}{:02}",
            prefix,
            now.year(),
            now.month(),
            now.day()
        );
        let idx = if max_file_size == 0 {
            0
        } else {
            self.next_file_index(&date_prefix, max_file_size)
        };

        let file_name = if idx == 0 {
            format!("{date_prefix}.{LOG_EXT}")
        } else {
            format!("{date_prefix}_{idx}.{LOG_EXT}")
        };
        dir.join(file_name)
    }

    fn next_file_index(&self, date_prefix: &str, max_file_size: u64) -> i64 {
        let mut names = self.get_file_names_by_prefix(&self.log_dir, date_prefix);
        if let Some(cache_dir) = &self.cache_dir {
            names.extend(self.get_file_names_by_prefix(cache_dir, date_prefix));
        }
        if names.is_empty() {
            return 0;
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
            idx + 1
        } else {
            idx
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

fn append_bytes(path: &Path, bytes: &[u8]) -> Result<(), FileManagerError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| FileManagerError::CreateDir(parent.to_path_buf(), e))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| FileManagerError::OpenFile(path.to_path_buf(), e))?;
    let before_len = file
        .metadata()
        .map_err(|e| FileManagerError::Metadata(path.to_path_buf(), e))?
        .len();
    if let Err(e) = file.write_all(bytes) {
        rollback_file_to_len(&mut file, before_len);
        return Err(FileManagerError::WriteFile(path.to_path_buf(), e));
    }
    Ok(())
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

    let mut buf = [0u8; 4096];
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
    if let Err(e) = dst_file.flush() {
        rollback_file_to_len(&mut dst_file, dst_before_len);
        return Err(FileManagerError::WriteFile(dst.to_path_buf(), e));
    }

    Ok(())
}

fn rollback_file_to_len(file: &mut File, target_len: u64) {
    let _ = file.set_len(target_len);
    let _ = file.seek(SeekFrom::Start(target_len));
}

fn file_mtime(path: &Path) -> Result<SystemTime, FileManagerError> {
    let meta = fs::metadata(path).map_err(|e| FileManagerError::Metadata(path.to_path_buf(), e))?;
    meta.modified()
        .map_err(|e| FileManagerError::Metadata(path.to_path_buf(), e))
}

#[cfg(test)]
mod tests {
    use super::FileManager;
    use chrono::{Datelike, Local};

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
}
