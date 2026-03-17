use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::file_manager::FileManagerError;
use crate::file_naming::{LOG_EXT, LOG_EXT_WITH_DOT};
use crate::file_ops::{append_file_to_file, file_mtime};
use crate::metrics::{record_cache_move, record_expired_delete};

#[derive(Debug, Clone, Copy)]
pub(crate) struct CacheMaintenance<'a> {
    pub(crate) log_dir: &'a Path,
    pub(crate) cache_dir: Option<&'a Path>,
    pub(crate) name_prefix: &'a str,
    pub(crate) cache_days: i32,
}

pub(crate) fn move_old_cache_files(
    config: CacheMaintenance<'_>,
) -> Result<Vec<PathBuf>, FileManagerError> {
    let Some(cache_dir) = config.cache_dir else {
        return Ok(Vec::new());
    };
    if cache_dir == config.log_dir {
        return Ok(Vec::new());
    }
    if !cache_dir.is_dir() {
        return Ok(Vec::new());
    }

    let now = SystemTime::now();
    let mut removed_paths = Vec::new();
    let entries = fs::read_dir(cache_dir)
        .map_err(|e| FileManagerError::ReadDir(cache_dir.to_path_buf(), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| FileManagerError::ReadDir(cache_dir.to_path_buf(), e))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if !file_name.starts_with(config.name_prefix) || !file_name.ends_with(LOG_EXT_WITH_DOT) {
            continue;
        }

        if config.cache_days > 0 {
            let modified = file_mtime(&path)?;
            let Ok(age) = now.duration_since(modified) else {
                continue;
            };
            if age < Duration::from_secs(config.cache_days as u64 * 24 * 60 * 60) {
                continue;
            }
        }

        let dest = config.log_dir.join(file_name);
        append_file_to_file(&path, &dest)?;
        fs::remove_file(&path).map_err(|e| FileManagerError::RemoveFile(path.clone(), e))?;
        record_cache_move();
        removed_paths.push(path);
    }

    Ok(removed_paths)
}

pub(crate) fn delete_expired_files(
    log_dir: &Path,
    cache_dir: Option<&Path>,
    max_alive_seconds: i64,
) -> Result<Vec<PathBuf>, FileManagerError> {
    if max_alive_seconds <= 0 {
        return Ok(Vec::new());
    }
    let threshold = Duration::from_secs(max_alive_seconds as u64);
    let mut removed_paths = Vec::new();
    delete_expired_under(log_dir, threshold, &mut removed_paths)?;
    if let Some(cache_dir) = cache_dir {
        delete_expired_under(cache_dir, threshold, &mut removed_paths)?;
    }
    Ok(removed_paths)
}

fn delete_expired_under(
    dir: &Path,
    threshold: Duration,
    removed_paths: &mut Vec<PathBuf>,
) -> Result<(), FileManagerError> {
    if !dir.is_dir() {
        return Ok(());
    }

    let now = SystemTime::now();
    let entries = fs::read_dir(dir).map_err(|e| FileManagerError::ReadDir(dir.to_path_buf(), e))?;
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
                fs::remove_file(&path)
                    .map_err(|e| FileManagerError::RemoveFile(path.clone(), e))?;
                record_expired_delete();
                removed_paths.push(path);
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
