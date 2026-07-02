//! On-disk run layout, owned in one place.
//!
//! Runs live at `<weather_dir>/<slug>/<YYYY-MM-DD>/<HH-MM-SS>`. Both directory
//! levels are zero-padded, so the lexically-greatest child is always the newest
//! run — which is what [`latest_run_path`] relies on. Keeping the write path
//! ([`run_path`]) and the read path ([`latest_run_path`]) side by side means
//! the convention is defined once instead of re-derived at each call site.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::errors::AppError;

/// Absolute path the run issued at `forecast_issued` for `slug` is written to.
pub fn run_path(weather_dir: &Path, slug: &str, forecast_issued: DateTime<Utc>) -> PathBuf {
    let date = forecast_issued.date_naive().to_string();
    let time = forecast_issued.time().format("%H-%M-%S").to_string();
    weather_dir.join(slug).join(date).join(time)
}

/// Path of the most recent run on disk for `slug`. Errors with
/// [`AppError::NotFound`] when the provider directory is missing or empty.
pub fn latest_run_path(weather_dir: &Path, slug: &str) -> Result<PathBuf, AppError> {
    let date_dir = latest_child(&weather_dir.join(slug))?;
    latest_child(&date_dir)
}

/// The lexically-greatest entry in `dir`.
fn latest_child(dir: &Path) -> Result<PathBuf, AppError> {
    std::fs::read_dir(dir)
        .map_err(|_| AppError::NotFound(format!("no data directory: {}", dir.display())))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .max_by(|a, b| a.file_name().cmp(&b.file_name()))
        .ok_or_else(|| AppError::NotFound(format!("no runs found in {}", dir.display())))
}

/// Delete every saved run for `slug` whose forecast-issued time is strictly
/// older than `cutoff`, then remove any date directories left empty.
///
/// Only entries whose paths match the storage layout (`<date>/<time>`) are
/// touched — unknown files in the tree are left alone. A missing slug
/// directory is not an error (returns `Ok(0)`): a provider that hasn't been
/// downloaded yet has nothing to clean.
pub fn prune_runs_older_than(
    weather_dir: &Path,
    slug: &str,
    cutoff: DateTime<Utc>,
) -> Result<usize, AppError> {
    let slug_dir = weather_dir.join(slug);
    if !slug_dir.exists() {
        return Ok(0);
    }

    let mut deleted = 0;
    for date_entry in std::fs::read_dir(&slug_dir)? {
        let date_path = date_entry?.path();
        if !date_path.is_dir() {
            continue;
        }
        for time_entry in std::fs::read_dir(&date_path)? {
            let run_path = time_entry?.path();
            let Some(issued) = super::parse_run_time(&run_path) else {
                continue;
            };
            if issued < cutoff {
                std::fs::remove_file(&run_path)?;
                deleted += 1;
            }
        }
        // `remove_dir` only succeeds on empty directories — naturally skips
        // dates with surviving runs (or unknown files we deliberately kept).
        let _ = std::fs::remove_dir(&date_path);
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;

    fn ymd_hms(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    fn write_run(weather_dir: &Path, slug: &str, issued: DateTime<Utc>) {
        let path = run_path(weather_dir, slug, issued);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    #[test]
    fn run_path_uses_zero_padded_date_time_layout() {
        let issued = ymd_hms(2026, 5, 25, 6);
        let path = run_path(Path::new("/data"), "icon_eu", issued);
        assert_eq!(path, PathBuf::from("/data/icon_eu/2026-05-25/06-00-00"));
    }

    #[test]
    fn latest_run_path_picks_newest_date_then_time() {
        let tmp = tempfile::tempdir().unwrap();
        let runs =
            [("2026-05-24", "21-00-00"), ("2026-05-25", "00-00-00"), ("2026-05-25", "06-00-00")];
        let icon = tmp.path().join("icon_eu");
        for (date, time) in runs {
            let dir = icon.join(date);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(time), b"x").unwrap();
        }

        let latest = latest_run_path(tmp.path(), "icon_eu").unwrap();
        assert_eq!(latest, icon.join("2026-05-25").join("06-00-00"));
    }

    #[test]
    fn latest_run_path_missing_provider_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let err = latest_run_path(tmp.path(), "noaa").unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[test]
    fn prune_removes_runs_older_than_cutoff() {
        let tmp = tempfile::tempdir().unwrap();
        let now = ymd_hms(2026, 5, 25, 0);
        let old = now - Duration::days(8);
        let recent = now - Duration::days(3);
        write_run(tmp.path(), "icon_eu", old);
        write_run(tmp.path(), "icon_eu", recent);

        let deleted =
            prune_runs_older_than(tmp.path(), "icon_eu", now - Duration::days(7)).unwrap();

        assert_eq!(deleted, 1);
        assert!(!run_path(tmp.path(), "icon_eu", old).exists());
        assert!(run_path(tmp.path(), "icon_eu", recent).exists());
    }

    #[test]
    fn prune_keeps_runs_at_or_after_cutoff() {
        // Strict less-than: a run issued exactly at the cutoff survives,
        // because it is not *older* than the cutoff.
        let tmp = tempfile::tempdir().unwrap();
        let cutoff = ymd_hms(2026, 5, 18, 0);
        write_run(tmp.path(), "icon_eu", cutoff);
        write_run(tmp.path(), "icon_eu", cutoff + Duration::hours(3));

        let deleted = prune_runs_older_than(tmp.path(), "icon_eu", cutoff).unwrap();

        assert_eq!(deleted, 0);
        assert!(run_path(tmp.path(), "icon_eu", cutoff).exists());
    }

    #[test]
    fn prune_removes_empty_date_directories() {
        // A date directory whose every run gets deleted is itself removed,
        // so the tree doesn't accumulate empty shells forever.
        let tmp = tempfile::tempdir().unwrap();
        let old = ymd_hms(2026, 5, 1, 0);
        write_run(tmp.path(), "icon_eu", old);
        write_run(tmp.path(), "icon_eu", old + Duration::hours(3));

        prune_runs_older_than(tmp.path(), "icon_eu", ymd_hms(2026, 5, 25, 0)).unwrap();

        assert!(!tmp.path().join("icon_eu").join("2026-05-01").exists());
    }

    #[test]
    fn prune_keeps_date_directory_with_surviving_runs() {
        // Same date, one run older than cutoff, one newer: the directory must
        // stay so the newer run remains reachable.
        let tmp = tempfile::tempdir().unwrap();
        let day = ymd_hms(2026, 5, 25, 0);
        let cutoff = day + Duration::hours(12);
        write_run(tmp.path(), "icon_eu", day);
        write_run(tmp.path(), "icon_eu", day + Duration::hours(15));

        prune_runs_older_than(tmp.path(), "icon_eu", cutoff).unwrap();

        let date_dir = tmp.path().join("icon_eu").join("2026-05-25");
        assert!(date_dir.exists());
        assert!(run_path(tmp.path(), "icon_eu", day + Duration::hours(15)).exists());
    }

    #[test]
    fn prune_leaves_unknown_files_alone() {
        // Files that don't match `<date>/<time>` are ignored — we only
        // delete entries we recognise as runs we wrote.
        let tmp = tempfile::tempdir().unwrap();
        let date_dir = tmp.path().join("icon_eu").join("2026-05-01");
        std::fs::create_dir_all(&date_dir).unwrap();
        let junk = date_dir.join("garbage.tmp");
        std::fs::write(&junk, b"x").unwrap();

        let deleted =
            prune_runs_older_than(tmp.path(), "icon_eu", ymd_hms(2026, 5, 25, 0)).unwrap();

        assert_eq!(deleted, 0);
        assert!(junk.exists());
    }

    #[test]
    fn prune_missing_slug_dir_returns_zero() {
        // Cold-start case: provider directory doesn't exist yet, prune is a
        // no-op rather than an error.
        let tmp = tempfile::tempdir().unwrap();
        let deleted = prune_runs_older_than(tmp.path(), "noaa", ymd_hms(2026, 5, 25, 0)).unwrap();
        assert_eq!(deleted, 0);
    }
}
