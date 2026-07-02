use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::cache::{CachedRun, WindsCache};
use crate::downloader::Provider;
use crate::errors::AppError;
use crate::forecast::WeatherMap;

pub mod api;
pub mod auth;
pub mod cache;
pub mod config;
pub mod downloader;
pub mod errors;
pub mod forecast;
pub mod scheduler;

/// Shared, cheaply-cloneable state handed to every request handler.
///
/// State holds `weather_dir` so the scheduler can compute storage paths, but
/// readers never consult disk: the in-memory [`WindsCache`] is the single
/// source of truth for what run is current. Cold-start populates the cache
/// from disk once in `main`; thereafter only the scheduler writes to it.
#[derive(Clone)]
pub struct AppState {
    pub weather_dir: PathBuf,
    /// Per-provider cache of the latest decoded run. Shared with the scheduler
    /// (cheap `Arc<Mutex<..>>` clone) so a download warms the same cache
    /// requests read from.
    cache: WindsCache,
}

impl AppState {
    pub fn new(weather_dir: PathBuf) -> Self {
        Self::with_cache(weather_dir, WindsCache::new())
    }

    /// Build state around an existing cache so the scheduler and the request
    /// handlers share one cache instance.
    pub fn with_cache(weather_dir: PathBuf, cache: WindsCache) -> Self {
        Self { weather_dir, cache }
    }

    /// Timestamp of the run currently being served for `provider`, or `None`
    /// when nothing has been cached yet (cold start before any successful
    /// warm or scheduler tick).
    ///
    /// Returns the cache's run, not disk's — so `run_info` and
    /// [`latest_winds`](Self::latest_winds) always agree about what's current.
    pub fn run_info(&self, provider: Provider) -> Option<DateTime<Utc>> {
        self.cache.get(provider).map(|c| c.run_time)
    }

    /// The latest decoded run for `provider`, served from the in-memory cache.
    ///
    /// Pure HashMap lookup — no disk I/O, no decode, no `spawn_blocking`.
    /// Returns [`AppError::NotFound`] when the cache is empty for this
    /// provider, which only happens before the first warm or download.
    pub fn latest_winds(&self, provider: Provider) -> Result<Arc<WeatherMap>, AppError> {
        self.cache
            .get(provider)
            .map(|c| c.map)
            .ok_or_else(|| AppError::NotFound(format!("no run available yet for {provider}")))
    }

    /// The latest run for `provider` — its timestamp and decoded map together,
    /// from a *single* cache lookup so the timestamp provably labels the exact
    /// map returned (no two-lookup window for the scheduler to swap runs in
    /// between, as calling [`run_info`](Self::run_info) and
    /// [`latest_winds`](Self::latest_winds) separately would allow).
    ///
    /// [`AppError::NotFound`] until the first warm/download for this provider.
    pub fn latest_run(&self, provider: Provider) -> Result<CachedRun, AppError> {
        self.cache
            .get(provider)
            .ok_or_else(|| AppError::NotFound(format!("no run available yet for {provider}")))
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::forecast::WeatherMap;

    fn run_time(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 25, hour, 0, 0).unwrap()
    }

    #[test]
    fn latest_winds_serves_cached_arc_without_touching_disk() {
        // No file is ever written to the weather_dir, proving the read path
        // never goes to disk: a populated cache is sufficient and necessary.
        let tmp = tempfile::tempdir().unwrap();
        let cache = WindsCache::new();
        let stored = Arc::new(WeatherMap::default());
        cache.insert(Provider::IconEu, run_time(6), Arc::clone(&stored));

        let state = AppState::with_cache(tmp.path().to_path_buf(), cache);
        let served = state.latest_winds(Provider::IconEu).unwrap();

        assert!(Arc::ptr_eq(&served, &stored), "must serve the cached Arc, not reload");
    }

    #[test]
    fn latest_winds_missing_provider_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let state = AppState::new(tmp.path().to_path_buf());

        let err = state.latest_winds(Provider::Noaa).unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[test]
    fn run_info_returns_cached_run_time() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = WindsCache::new();
        cache.insert(Provider::IconEu, run_time(6), Arc::new(WeatherMap::default()));

        let state = AppState::with_cache(tmp.path().to_path_buf(), cache);
        assert_eq!(state.run_info(Provider::IconEu), Some(run_time(6)));
    }

    #[test]
    fn run_info_and_latest_winds_agree() {
        // The consistency invariant: if run_info reports a run, latest_winds
        // serves it; if run_info is None, latest_winds is NotFound.
        let tmp = tempfile::tempdir().unwrap();
        let state = AppState::new(tmp.path().to_path_buf());

        assert!(state.run_info(Provider::IconEu).is_none());
        assert!(matches!(state.latest_winds(Provider::IconEu), Err(AppError::NotFound(_))));
    }

    #[test]
    fn run_info_missing_provider_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let state = AppState::new(tmp.path().to_path_buf());
        assert!(state.run_info(Provider::Noaa).is_none());
    }

    #[test]
    fn latest_run_returns_run_time_and_map_together() {
        // One lookup hands back both halves, and the map is the cached Arc (not
        // a reload) — so the timestamp provably labels the exact map served.
        let tmp = tempfile::tempdir().unwrap();
        let cache = WindsCache::new();
        let stored = Arc::new(WeatherMap::default());
        cache.insert(Provider::IconEu, run_time(6), Arc::clone(&stored));

        let state = AppState::with_cache(tmp.path().to_path_buf(), cache);
        let run = state.latest_run(Provider::IconEu).unwrap();

        assert_eq!(run.run_time, run_time(6));
        assert!(Arc::ptr_eq(&run.map, &stored), "must hand back the cached Arc");
    }

    #[test]
    fn latest_run_missing_provider_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let state = AppState::new(tmp.path().to_path_buf());

        let err = state.latest_run(Provider::Noaa).unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }
}
