//! Single-source-of-truth in-memory cache of the latest decoded run per
//! provider.
//!
//! Cheaply cloneable (every clone shares one inner `Arc<Mutex<..>>`), so the
//! background scheduler and the request handlers hold the *same* cache: the
//! scheduler pushes freshly-decoded runs in after each download, and readers
//! serve directly from it without touching disk. Disk is durability for
//! cold-start recovery; once the process is running, the cache alone decides
//! what "current" means — which keeps `/api/info` and `/api/get_winds`
//! consistent at any instant.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use chrono::{DateTime, Utc};

use crate::downloader::Provider;
use crate::forecast::WeatherMap;

/// A decoded run plus the timestamp it represents. Returned by [`WindsCache`]
/// so that handlers don't need a second lookup to learn which run they're
/// serving.
#[derive(Clone, Debug)]
pub struct CachedRun {
    pub run_time: DateTime<Utc>,
    pub map: Arc<WeatherMap>,
}

/// Per-provider cache of the latest decoded run.
#[derive(Clone, Default)]
pub struct WindsCache {
    inner: Arc<Mutex<HashMap<Provider, CachedRun>>>,
}

impl WindsCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached run for `provider`, or `None` if nothing has been inserted
    /// yet. Cloning a `CachedRun` is cheap: it's a `DateTime` plus an
    /// `Arc<WeatherMap>` bump.
    pub fn get(&self, provider: Provider) -> Option<CachedRun> {
        self.lock().get(&provider).cloned()
    }

    /// Store (or replace) the latest decoded run for `provider`. Called by the
    /// scheduler after each successful download, and by cold-start warming.
    pub fn insert(&self, provider: Provider, run_time: DateTime<Utc>, map: Arc<WeatherMap>) {
        self.lock().insert(provider, CachedRun { run_time, map });
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Provider, CachedRun>> {
        // Recover rather than propagate poisoning: the guarded section is a
        // small infallible map op, so a poisoned lock only reflects a previous
        // unrelated panic, not corrupt cache state.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn run_time(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 25, hour, 0, 0).unwrap()
    }

    fn map() -> Arc<WeatherMap> {
        Arc::new(WeatherMap::default())
    }

    #[test]
    fn get_returns_inserted_run() {
        let cache = WindsCache::new();
        let stored = map();
        cache.insert(Provider::IconEu, run_time(6), Arc::clone(&stored));

        let got = cache.get(Provider::IconEu).expect("hit");
        assert_eq!(got.run_time, run_time(6));
        assert!(Arc::ptr_eq(&got.map, &stored));
    }

    #[test]
    fn get_misses_for_absent_provider() {
        let cache = WindsCache::new();
        cache.insert(Provider::IconEu, run_time(6), map());
        assert!(cache.get(Provider::Noaa).is_none());
    }

    #[test]
    fn insert_replaces_previous_run_for_provider() {
        // A new tick from the scheduler atomically swaps the cached run; the
        // next reader sees only the newer one.
        let cache = WindsCache::new();
        cache.insert(Provider::IconEu, run_time(0), map());

        let newer = map();
        cache.insert(Provider::IconEu, run_time(6), Arc::clone(&newer));

        let got = cache.get(Provider::IconEu).expect("hit");
        assert_eq!(got.run_time, run_time(6));
        assert!(Arc::ptr_eq(&got.map, &newer), "newest insert wins");
    }
}
