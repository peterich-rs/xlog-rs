use std::path::{Path, PathBuf};

use chrono::Local;

use crate::active_append::ActiveAppendFile;
use crate::file_naming::{build_path_for_index, file_index_from_path};
use crate::metrics::record_file_rotate;

#[derive(Debug, Default)]
pub(crate) struct RuntimeState {
    pub(crate) last_append_time: Option<i64>,
    pub(crate) last_append_path: Option<PathBuf>,
    pub(crate) active_file: Option<ActiveAppendFile>,
    pub(crate) log_target: Option<AppendTargetCache>,
    pub(crate) cache_target: Option<AppendTargetCache>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TargetDirs<'a> {
    pub(crate) log_dir: &'a Path,
    pub(crate) cache_dir: Option<&'a Path>,
}

impl<'a> TargetDirs<'a> {
    pub(crate) fn new(log_dir: &'a Path, cache_dir: Option<&'a Path>) -> Self {
        Self { log_dir, cache_dir }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AppendTargetCache {
    pub(crate) path: PathBuf,
    pub(crate) day_key: i32,
    pub(crate) file_index: i64,
    pub(crate) merged_len: u64,
    pub(crate) local_len: u64,
    pub(crate) local_exists: bool,
}

impl RuntimeState {
    pub(crate) fn target_for_dir<'a>(
        &'a self,
        dirs: TargetDirs<'_>,
        dir: &Path,
    ) -> Option<&'a AppendTargetCache> {
        if dir == dirs.log_dir {
            self.log_target.as_ref()
        } else if dirs.cache_dir == Some(dir) {
            self.cache_target.as_ref()
        } else {
            None
        }
    }

    pub(crate) fn target_slot_for_dir<'a>(
        &'a mut self,
        dirs: TargetDirs<'_>,
        dir: &Path,
    ) -> Option<&'a mut Option<AppendTargetCache>> {
        if dir == dirs.log_dir {
            Some(&mut self.log_target)
        } else if dirs.cache_dir == Some(dir) {
            Some(&mut self.cache_target)
        } else {
            None
        }
    }

    pub(crate) fn set_target_for_dir(
        &mut self,
        dirs: TargetDirs<'_>,
        dir: &Path,
        target: AppendTargetCache,
    ) {
        if let Some(slot) = self.target_slot_for_dir(dirs, dir) {
            *slot = Some(target);
        }
    }

    pub(crate) fn record_last_append(&mut self, now_ts: i64, path: &Path) {
        self.last_append_time = Some(now_ts);
        self.last_append_path = Some(path.to_path_buf());
    }

    pub(crate) fn next_cached_path(
        &mut self,
        dirs: TargetDirs<'_>,
        now_ts: i64,
        dir: &Path,
        day_key: i32,
        prefix: &str,
        max_file_size: u64,
    ) -> Option<PathBuf> {
        let target = self.target_for_dir(dirs, dir)?.clone();
        if target.day_key != day_key {
            if let Some(slot) = self.target_slot_for_dir(dirs, dir) {
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
            self.set_target_for_dir(dirs, dir, next.clone());
            self.record_last_append(now_ts, &next.path);
            return Some(next.path);
        }
        self.record_last_append(now_ts, &target.path);
        Some(target.path)
    }

    pub(crate) fn cached_local_exists_for_day(
        &mut self,
        dirs: TargetDirs<'_>,
        dir: &Path,
        current_day: i32,
        max_file_size: u64,
    ) -> Option<bool> {
        let target = self.target_for_dir(dirs, dir)?.clone();
        if target.day_key != current_day {
            if let Some(slot) = self.target_slot_for_dir(dirs, dir) {
                *slot = None;
            }
            return None;
        }
        if max_file_size > 0 && target.merged_len > max_file_size {
            if let Some(slot) = self.target_slot_for_dir(dirs, dir) {
                *slot = None;
            }
            return None;
        }
        Some(target.local_exists)
    }

    pub(crate) fn cached_local_len_for_path(&self, path: &Path, day_key: i32) -> Option<u64> {
        for target in [self.log_target.as_ref(), self.cache_target.as_ref()]
            .into_iter()
            .flatten()
        {
            if target.day_key == day_key && target.path == path {
                return Some(target.local_len);
            }
        }
        None
    }

    pub(crate) fn merged_len_after_append(
        &self,
        path: &Path,
        day_key: i32,
        delta: u64,
        current_len: u64,
    ) -> u64 {
        let file_name = path.file_name();
        for target in [self.log_target.as_ref(), self.cache_target.as_ref()]
            .into_iter()
            .flatten()
        {
            if target.day_key == day_key && target.path.file_name() == file_name {
                return target.merged_len.saturating_add(delta);
            }
        }
        current_len
    }

    pub(crate) fn update_target_after_append(
        &mut self,
        dirs: TargetDirs<'_>,
        name_prefix: &str,
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
        if let Some(target) = self.log_target.as_mut() {
            if target.day_key == day_key && target.path.file_name() == file_name {
                target.merged_len = merged_len;
                if target.path == path {
                    target.local_exists = true;
                    target.local_len = current_len;
                    matched_current_dir = true;
                }
            }
        }
        if let Some(target) = self.cache_target.as_mut() {
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
            self.set_target_for_dir(
                dirs,
                parent,
                AppendTargetCache {
                    path: path.to_path_buf(),
                    day_key,
                    file_index: file_index_from_path(path, name_prefix).unwrap_or(0),
                    merged_len,
                    local_len: current_len,
                    local_exists: true,
                },
            );
        }
        self.record_last_append(now_ts, path);
    }

    pub(crate) fn mark_path_removed(&mut self, path: &Path) {
        if let Some(target) = self.log_target.as_mut() {
            if target.path == path {
                target.local_exists = false;
                target.local_len = 0;
            }
        }
        if let Some(target) = self.cache_target.as_mut() {
            if target.path == path {
                target.local_exists = false;
                target.local_len = 0;
            }
        }
        if self
            .active_file
            .as_ref()
            .map(|active| active.path.as_path())
            == Some(path)
        {
            self.active_file = None;
        }
        if self.last_append_path.as_deref() == Some(path) {
            self.last_append_path = None;
        }
    }
}
