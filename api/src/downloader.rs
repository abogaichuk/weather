use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Timelike, Utc};
// Re-exported so the rest of the crate keeps importing `crate::downloader::Provider`.
// The enum itself now lives in `weathergrid`, shared with client backends so
// the `provider` wire value can't drift between server and client.
pub use weathergrid::Provider;

use crate::cache::WindsCache;
use crate::config::BoundingBox;
use crate::errors::AppError;
use crate::forecast::{self, WeatherMap};

mod ecmwf;
mod icon_eu;
mod noaa;
mod storage;

const CONCURRENT_REQUESTS: usize = 5;
/// Fixed forecast window (hours) for the physics models. ICON-EU caps at 12 h
/// upstream; GFS uses the same window for a comparable horizon. ECMWF's window
/// is configurable instead (see `Config::ecmwf_forecast_hours`).
const FORECAST_DURATION: u16 = 12; // 12 MAX(icon_eu restriction)

/// One weather data source. Each provider's run-slot rounding and download
/// live in its own `impl`, so adding a source is one new struct plus one arm in
/// [`handler`] — not edits scattered across matches. The on-disk/API slug is
/// *not* here: it's pure variant identity, owned by [`Provider::slug`].
#[async_trait]
trait WeatherProvider: Send + Sync {
    /// Round `now` DOWN to this provider's most recent run slot.
    fn forecast_issued(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>, AppError>;

    /// Spacing between consecutive run slots (6h for GFS, 3h for ICON-EU). Used
    /// to derive the previous slot for the on-disk fallback in
    /// [`download_and_save`].
    fn cadence(&self) -> Duration;

    /// Fetch the run issued at `forecast_issued`, trimmed to `bbox`, for the
    /// first `hours` of the forecast (window from issue time).
    async fn download(
        &self,
        forecast_issued: DateTime<Utc>,
        bbox: BoundingBox,
        hours: u16,
    ) -> Result<WeatherMap, AppError>;
}

struct Noaa;
struct IconEu;
struct Ecmwf;

#[async_trait]
impl WeatherProvider for Noaa {
    // Nearest 6-hour NOAA GFS run slot (00, 06, 12, 18 UTC).
    fn forecast_issued(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>, AppError> {
        round_time(now, (now.hour() / 6) * 6)
            .ok_or_else(|| AppError::Config("couldn't round noaa run time".into()))
    }

    fn cadence(&self) -> Duration {
        Duration::hours(6)
    }

    async fn download(
        &self,
        forecast_issued: DateTime<Utc>,
        bbox: BoundingBox,
        hours: u16,
    ) -> Result<WeatherMap, AppError> {
        noaa::download(forecast_issued, bbox, hours).await
    }
}

#[async_trait]
impl WeatherProvider for IconEu {
    // Nearest 3-hour ICON-EU run slot (00, 03, 06, 09, 12, 15, 18, 21 UTC).
    fn forecast_issued(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>, AppError> {
        round_time(now, (now.hour() / 3) * 3)
            .ok_or_else(|| AppError::Config("couldn't round icon run time".into()))
    }

    fn cadence(&self) -> Duration {
        Duration::hours(3)
    }

    async fn download(
        &self,
        forecast_issued: DateTime<Utc>,
        bbox: BoundingBox,
        hours: u16,
    ) -> Result<WeatherMap, AppError> {
        icon_eu::download(forecast_issued, bbox, hours).await
    }
}

#[async_trait]
impl WeatherProvider for Ecmwf {
    // Nearest 6-hour ECMWF run slot (00, 06, 12, 18 UTC), same cadence as GFS.
    fn forecast_issued(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>, AppError> {
        round_time(now, (now.hour() / 6) * 6)
            .ok_or_else(|| AppError::Config("couldn't round ecmwf run time".into()))
    }

    fn cadence(&self) -> Duration {
        Duration::hours(6)
    }

    async fn download(
        &self,
        forecast_issued: DateTime<Utc>,
        bbox: BoundingBox,
        hours: u16,
    ) -> Result<WeatherMap, AppError> {
        ecmwf::download(forecast_issued, bbox, hours).await
    }
}

/// The single [`Provider`]→behaviour bridge: the one site that must learn about
/// a new variant. A free function rather than an inherent method because
/// `Provider` is now a foreign type (defined in `weathergrid`), so the orphan
/// rule forbids `impl Provider` here.
fn handler(provider: Provider) -> &'static dyn WeatherProvider {
    match provider {
        Provider::Noaa => &Noaa,
        Provider::IconEu => &IconEu,
        Provider::Ecmwf => &Ecmwf,
    }
}

fn round_time(now: DateTime<Utc>, hour: u32) -> Option<DateTime<Utc>> {
    now.with_hour(hour)?.with_minute(0)?.with_second(0)?.with_nanosecond(0)
}

pub async fn download_all(
    now: DateTime<Utc>,
    weather_dir: &Path,
    bbox: BoundingBox,
    ecmwf_hours: u16,
    cache: &WindsCache,
) -> Result<(), AppError> {
    if let Err(err) =
        download_and_save(Provider::IconEu, now, weather_dir, bbox, FORECAST_DURATION, cache).await
    {
        tracing::error!(?err, "icon-eu download failed");
    }
    if let Err(err) =
        download_and_save(Provider::Noaa, now, weather_dir, bbox, FORECAST_DURATION, cache).await
    {
        tracing::error!(?err, "noaa download failed");
    }
    if let Err(err) =
        download_and_save(Provider::Ecmwf, now, weather_dir, bbox, ecmwf_hours, cache).await
    {
        tracing::error!(?err, "ecmwf download failed");
    }
    Ok(())
}

/// What [`download_and_save`] should do, given which recent runs are on disk.
#[derive(Debug, PartialEq, Eq)]
enum RunChoice {
    /// Latest run already saved — nothing to do.
    Fresh,
    /// Download the run issued at this slot.
    Fetch(DateTime<Utc>),
}

/// Pick which run to fetch from the latest slot, the previous slot, and which
/// of them are already on disk.
///
/// The previous-slot fallback only kicks in when *neither* run is saved yet: a
/// late upstream publish (latest not ready, previous already saved) keeps
/// retrying `latest`, while a gap from a publish that lagged past a slot
/// boundary (neither saved) is recovered by fetching the previous slot — which
/// is definitely published by now — so the dataset never silently stalls.
fn choose_run(
    latest: DateTime<Utc>,
    previous: DateTime<Utc>,
    latest_on_disk: bool,
    previous_on_disk: bool,
) -> RunChoice {
    if latest_on_disk {
        RunChoice::Fresh
    } else if previous_on_disk {
        RunChoice::Fetch(latest)
    } else {
        RunChoice::Fetch(previous)
    }
}

async fn download_and_save(
    provider: Provider,
    now: DateTime<Utc>,
    weather_dir: &Path,
    bbox: BoundingBox,
    hours: u16,
    cache: &WindsCache,
) -> Result<(), AppError> {
    let handler = handler(provider);
    let latest = handler.forecast_issued(now)?;
    let previous = latest - handler.cadence();
    let latest_path = storage::run_path(weather_dir, provider.slug(), latest);
    let previous_path = storage::run_path(weather_dir, provider.slug(), previous);

    let issued = match choose_run(latest, previous, latest_path.exists(), previous_path.exists()) {
        RunChoice::Fresh => {
            tracing::info!(path = ?latest_path, "fresh data already on disk, skipping");
            return Ok(());
        }
        RunChoice::Fetch(issued) => issued,
    };

    let save_path = storage::run_path(weather_dir, provider.slug(), issued);
    let weather_map = handler.download(issued, bbox, hours).await?;

    if weather_map.is_empty() {
        return Err(AppError::EmptyResult(provider.slug().to_string()));
    }

    forecast::log_total(&weather_map);

    // Wrap once in an `Arc`: the save task and the cache share the same decoded
    // map, so warming the cache costs no extra decode or clone of the grid.
    let map = Arc::new(weather_map);
    let save_map = Arc::clone(&map);

    // Encode + compress + write off the async reactor: bincode serialisation
    // and zstd compression are CPU-bound and the file write is blocking I/O.
    tokio::task::spawn_blocking(move || -> Result<(), AppError> {
        if let Some(parent) = save_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = forecast::deserializer::to_binary(&save_map)?;
        let bytes = forecast::compression::compress(&bytes)?;
        std::fs::write(&save_path, bytes)?;
        Ok(())
    })
    .await
    .map_err(|e| AppError::Config(format!("run save task panicked: {e}")))??;

    // Only after a durable save: push the fresh run into the shared cache so
    // the next request serves the warm `Arc` directly. The brief window
    // between `fs::write` returning and this `insert` is the only point at
    // which disk is ahead of cache — readers see the previous run during that
    // window, which preserves the cache-as-single-source-of-truth invariant.
    cache.insert(provider, issued, map);
    Ok(())
}

/// Populate `cache` with the latest run already on disk for `provider`.
///
/// Called once at startup so the API can serve the previous-process run
/// immediately, without waiting for the first scheduler tick (up to 15 min
/// away). A missing run on disk is not an error — it just means this provider
/// stays empty in the cache until the scheduler downloads one.
///
/// This is the only place a reader-side disk read survives the cache-as-truth
/// refactor; once warm, all serving paths are pure HashMap lookups.
pub fn warm_cache(
    weather_dir: &Path,
    provider: Provider,
    cache: &WindsCache,
) -> Result<bool, AppError> {
    let path = match storage::latest_run_path(weather_dir, provider.slug()) {
        Ok(p) => p,
        // No run yet on disk for this provider — cold start with empty data
        // dir, not an error.
        Err(AppError::NotFound(_)) => return Ok(false),
        Err(e) => return Err(e),
    };
    let Some(run_time) = parse_run_time(&path) else {
        return Err(AppError::Config(format!("could not parse run time from path: {path:?}")));
    };
    let map = Arc::new(forecast::load(&path)?);
    cache.insert(provider, run_time, map);
    Ok(true)
}

/// Path of the most recent run on disk for `provider`. See [`storage`] for the
/// directory layout.
pub fn latest_run_path(weather_dir: &Path, provider: Provider) -> Result<PathBuf, AppError> {
    storage::latest_run_path(weather_dir, provider.slug())
}

/// Delete every saved run, across every provider, that was issued before
/// `cutoff`. Returns the total count deleted.
///
/// Per-provider errors are logged and swallowed so one provider's failure
/// doesn't block the others — disk reclamation is best-effort housekeeping,
/// not the critical path. The scheduler retries daily.
pub fn prune_old_runs(weather_dir: &Path, cutoff: DateTime<Utc>) -> usize {
    let mut total = 0;
    for &provider in Provider::all() {
        match storage::prune_runs_older_than(weather_dir, provider.slug(), cutoff) {
            Ok(n) => total += n,
            Err(err) => tracing::error!(?err, %provider, "prune failed"),
        }
    }
    total
}

/// Parse the forecast run time encoded in a storage path.
///
/// Paths follow the layout `…/{YYYY-MM-DD}/{HH-MM-SS}` (see [`storage`]). The
/// last two components are joined and parsed into a UTC [`DateTime`]; returns
/// `None` when the path is too short or the components don't match the format.
pub fn parse_run_time(path: &Path) -> Option<DateTime<Utc>> {
    let time = path.file_name()?.to_str()?;
    let date = path.parent()?.file_name()?.to_str()?;
    chrono::NaiveDateTime::parse_from_str(&format!("{date} {time}"), "%Y-%m-%d %H-%M-%S")
        .ok()
        .map(|ndt| ndt.and_utc())
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};

    use crate::downloader::{
        Ecmwf, IconEu, Noaa, Provider, RunChoice, WeatherProvider, choose_run, latest_run_path,
        parse_run_time, prune_old_runs, storage,
    };

    fn slot(hour: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 25, hour, 0, 0).unwrap()
    }

    #[test]
    fn provider_cadence_matches_run_spacing() {
        assert_eq!(IconEu.cadence(), Duration::hours(3));
        assert_eq!(Noaa.cadence(), Duration::hours(6));
        assert_eq!(Ecmwf.cadence(), Duration::hours(6));
    }

    #[test]
    fn choose_run_skips_when_latest_already_saved() {
        // Latest present → Fresh, regardless of the previous slot.
        assert_eq!(choose_run(slot(6), slot(3), true, true), RunChoice::Fresh);
        assert_eq!(choose_run(slot(6), slot(3), true, false), RunChoice::Fresh);
    }

    #[test]
    fn choose_run_fetches_latest_while_previous_already_on_disk() {
        // Normal lag window: latest not published yet, previous already saved →
        // keep trying latest (serving previous in the meantime).
        assert_eq!(choose_run(slot(6), slot(3), false, true), RunChoice::Fetch(slot(6)));
    }

    #[test]
    fn choose_run_falls_back_to_previous_when_neither_on_disk() {
        // Recovery: a publish that lagged past the slot boundary left a gap, so
        // fetch the previous slot — definitely published by now.
        assert_eq!(choose_run(slot(6), slot(3), false, false), RunChoice::Fetch(slot(3)));
    }

    #[test]
    fn latest_run_path_maps_provider_to_its_slug_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let icon = tmp.path().join("icon_eu");
        let dir = icon.join("2026-05-25");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("06-00-00"), b"x").unwrap();

        // Confirms Provider::IconEu resolves to the "icon_eu" directory via storage.
        let latest = latest_run_path(tmp.path(), Provider::IconEu).unwrap();
        assert_eq!(latest, icon.join("2026-05-25").join("06-00-00"));
    }

    #[test]
    fn provider_forecast_issued() {
        let t = Utc.with_ymd_and_hms(2026, 5, 20, 11, 23, 0).unwrap();

        assert_eq!(
            IconEu.forecast_issued(t).unwrap(),
            Utc.with_ymd_and_hms(2026, 5, 20, 9, 0, 0).unwrap()
        );
        assert_eq!(
            Noaa.forecast_issued(t).unwrap(),
            Utc.with_ymd_and_hms(2026, 5, 20, 6, 0, 0).unwrap()
        );

        // ECMWF shares the 6-hour slot rounding with GFS.
        assert_eq!(
            Ecmwf.forecast_issued(t).unwrap(),
            Utc.with_ymd_and_hms(2026, 5, 20, 6, 0, 0).unwrap()
        );

        let boundary = Utc.with_ymd_and_hms(2026, 5, 20, 12, 0, 0).unwrap();
        assert_eq!(IconEu.forecast_issued(boundary).unwrap(), boundary);
        assert_eq!(Noaa.forecast_issued(boundary).unwrap(), boundary);
        assert_eq!(Ecmwf.forecast_issued(boundary).unwrap(), boundary);
    }

    #[test]
    fn parse_run_time_recovers_datetime_from_path() {
        // Storage layout: {weather_dir}/{slug}/{YYYY-MM-DD}/{HH-MM-SS}
        let path = std::path::Path::new("/data/icon_eu/2026-05-25/06-00-00");
        let expected = Utc.with_ymd_and_hms(2026, 5, 25, 6, 0, 0).unwrap();
        assert_eq!(parse_run_time(path), Some(expected));
    }

    #[test]
    fn parse_run_time_non_standard_path_is_none() {
        assert_eq!(parse_run_time(std::path::Path::new("/data")), None);
        assert_eq!(parse_run_time(std::path::Path::new("/data/icon_eu/not-a-date/xx")), None);
    }

    #[test]
    fn provider_all_covers_every_variant() {
        let all = Provider::all();
        assert!(all.contains(&Provider::IconEu));
        assert!(all.contains(&Provider::Noaa));
        assert!(all.contains(&Provider::Ecmwf));
        assert_eq!(all.len(), 3, "update all() when adding a variant");
    }

    #[test]
    fn provider_slug_matches_serde_rename() {
        assert_eq!(Provider::IconEu.slug(), "icon_eu");
        assert_eq!(Provider::Noaa.slug(), "noaa");
        assert_eq!(Provider::Ecmwf.slug(), "ecmwf");
    }

    #[test]
    fn prune_old_runs_iterates_every_provider() {
        // One old run per provider; a single prune call cleans them all.
        let tmp = tempfile::tempdir().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 5, 25, 0, 0, 0).unwrap();
        let old = now - Duration::days(8);

        for &p in Provider::all() {
            let path = storage::run_path(tmp.path(), p.slug(), old);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"x").unwrap();
        }

        let deleted = prune_old_runs(tmp.path(), now - Duration::days(7));

        assert_eq!(deleted, Provider::all().len());
        for &p in Provider::all() {
            assert!(!storage::run_path(tmp.path(), p.slug(), old).exists());
        }
    }
}
