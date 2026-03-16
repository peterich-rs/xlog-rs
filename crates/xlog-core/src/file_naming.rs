use std::path::{Path, PathBuf};

use chrono::{Datelike, Duration as ChronoDuration, Local};

pub(crate) const LOG_EXT: &str = "xlog";
pub(crate) const LOG_EXT_WITH_DOT: &str = ".xlog";

pub(crate) fn make_date_prefix(prefix: &str, timespan: i32) -> String {
    let now = Local::now() - ChronoDuration::days(timespan as i64);
    format!(
        "{}_{:04}{:02}{:02}",
        prefix,
        now.year(),
        now.month(),
        now.day()
    )
}

pub(crate) fn make_date_prefix_from_day_key(prefix: &str, day_key: i32) -> String {
    let year = day_key / 10_000;
    let month = (day_key / 100) % 100;
    let day = day_key % 100;
    format!("{prefix}_{year:04}{month:02}{day:02}")
}

pub(crate) fn build_path_for_index(
    dir: &Path,
    prefix: &str,
    day_key: i32,
    file_index: i64,
) -> PathBuf {
    let date_prefix = make_date_prefix_from_day_key(prefix, day_key);
    let file_name = if file_index == 0 {
        format!("{date_prefix}.{LOG_EXT}")
    } else {
        format!("{date_prefix}_{file_index}.{LOG_EXT}")
    };
    dir.join(file_name)
}

pub(crate) fn file_index_from_path(path: &Path, prefix: &str) -> Option<i64> {
    let name = path.file_name()?.to_str()?;
    let base = name.strip_suffix(LOG_EXT_WITH_DOT)?;
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

pub(crate) fn day_key(now: chrono::DateTime<Local>) -> i32 {
    now.year() * 10_000 + (now.month() as i32) * 100 + now.day() as i32
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone};

    use super::{
        build_path_for_index, day_key, file_index_from_path, make_date_prefix_from_day_key,
    };

    #[test]
    fn build_and_parse_split_file_names_roundtrip() {
        let dir = std::path::Path::new("/tmp");
        let path = build_path_for_index(dir, "demo", 20260316, 3);
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "demo_20260316_3.xlog"
        );
        assert_eq!(file_index_from_path(&path, "demo"), Some(3));
    }

    #[test]
    fn make_date_prefix_from_day_key_formats_zero_index_name_prefix() {
        assert_eq!(
            make_date_prefix_from_day_key("demo", 20260316),
            "demo_20260316"
        );
    }

    #[test]
    fn day_key_matches_local_calendar_date() {
        let sample = Local.with_ymd_and_hms(2026, 3, 16, 8, 0, 0).unwrap();
        assert_eq!(day_key(sample), 20260316);
    }
}
