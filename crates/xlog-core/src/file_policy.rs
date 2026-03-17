use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AppendRoutePlan {
    AppendToPath {
        path: PathBuf,
        promote_after_append: bool,
    },
    PreferLogThenCache {
        log_path: PathBuf,
        cache_path: PathBuf,
    },
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CacheRoutePlanner {
    should_cache_logs: bool,
    move_file: bool,
}

impl CacheRoutePlanner {
    pub(crate) fn new(should_cache_logs: bool, move_file: bool) -> Self {
        Self {
            should_cache_logs,
            move_file,
        }
    }

    pub(crate) fn active_log(self, path: PathBuf) -> AppendRoutePlan {
        AppendRoutePlan::AppendToPath {
            path,
            promote_after_append: false,
        }
    }

    pub(crate) fn active_cache(self, path: PathBuf) -> AppendRoutePlan {
        AppendRoutePlan::AppendToPath {
            path,
            promote_after_append: self.promote_after_append(),
        }
    }

    pub(crate) fn resolved_cache(
        self,
        path: PathBuf,
        local_exists: bool,
    ) -> Option<AppendRoutePlan> {
        if self.should_cache_logs || local_exists {
            return Some(AppendRoutePlan::AppendToPath {
                path,
                promote_after_append: self.promote_after_append(),
            });
        }
        None
    }

    pub(crate) fn fallback(self, log_path: PathBuf, cache_path: PathBuf) -> AppendRoutePlan {
        let _ = self;
        AppendRoutePlan::PreferLogThenCache {
            log_path,
            cache_path,
        }
    }

    fn promote_after_append(self) -> bool {
        self.move_file && !self.should_cache_logs
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{AppendRoutePlan, CacheRoutePlanner};

    #[test]
    fn active_log_never_promotes() {
        let plan = CacheRoutePlanner::new(true, true).active_log(PathBuf::from("/tmp/log.xlog"));
        assert_eq!(
            plan,
            AppendRoutePlan::AppendToPath {
                path: PathBuf::from("/tmp/log.xlog"),
                promote_after_append: false,
            }
        );
    }

    #[test]
    fn active_cache_promotes_only_when_cache_policy_is_disabled() {
        let path = PathBuf::from("/tmp/cache.xlog");
        assert_eq!(
            CacheRoutePlanner::new(true, true).active_cache(path.clone()),
            AppendRoutePlan::AppendToPath {
                path: path.clone(),
                promote_after_append: false,
            }
        );
        assert_eq!(
            CacheRoutePlanner::new(false, true).active_cache(path.clone()),
            AppendRoutePlan::AppendToPath {
                path,
                promote_after_append: true,
            }
        );
    }

    #[test]
    fn resolved_cache_returns_none_when_no_cache_target_should_be_used() {
        assert_eq!(
            CacheRoutePlanner::new(false, true)
                .resolved_cache(PathBuf::from("/tmp/cache.xlog"), false),
            None
        );
    }

    #[test]
    fn fallback_prefers_log_then_cache() {
        let plan = CacheRoutePlanner::new(false, true).fallback(
            PathBuf::from("/tmp/log.xlog"),
            PathBuf::from("/tmp/cache.xlog"),
        );
        assert_eq!(
            plan,
            AppendRoutePlan::PreferLogThenCache {
                log_path: PathBuf::from("/tmp/log.xlog"),
                cache_path: PathBuf::from("/tmp/cache.xlog"),
            }
        );
    }
}
