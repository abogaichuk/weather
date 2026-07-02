use std::collections::{BTreeMap, BTreeSet};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use weathergrid::{RunInfo, WeatherResponse};

use crate::cache::CachedRun;
use crate::downloader::Provider;
use crate::errors::AppError;
use crate::forecast::WeatherMap;
use crate::{AppState, forecast};

/// Query for `/api/get_winds`, e.g.
/// `?provider=icon_eu&pa=85000&lat=50.4&lon=30.5&range=2.0`.
#[derive(Debug, Deserialize)]
pub struct WindsQuery {
    provider: Provider,
    /// Minimum pressure in Pascals; higher pressure = closer to the ground.
    pa: u32,
    lat: f64,
    lon: f64,
    /// Half-width of the lat/lon box around (lat, lon), in degrees.
    range: f64,
}

/// Serve the latest run for a provider, sliced to now+future, pressures `>=
/// pa`, and cells within `±range` of the coordinate. Returns bincode bytes.
pub async fn get_winds(
    State(state): State<AppState>,
    Query(q): Query<WindsQuery>,
) -> Result<impl IntoResponse, AppError> {
    if q.range <= 0.0 || !(-90.0..=90.0).contains(&q.lat) || !(-180.0..=180.0).contains(&q.lon) {
        return Err(AppError::BadRequest(
            "lat must be in -90..=90, lon in -180..=180, range > 0".into(),
        ));
    }

    // Served from AppState's per-provider cache (pure HashMap lookup). The
    // cache is the single source of truth for the current run; the scheduler
    // refreshes it after each download. `filter_into` slices the shared
    // `Arc<WeatherMap>` by reference, allocating only the cells that pass — no
    // whole-grid clone.
    let map = state.latest_winds(q.provider)?;
    let slice = map.filter_into(Utc::now(), q.pa, q.lat, q.lon, q.range);

    if slice.is_empty() {
        return Err(AppError::NotFound("no winds match the requested filters".into()));
    }

    let bytes = forecast::deserializer::to_binary(&slice)?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes))
}

/// Query for `/api/winds`, e.g. `?provider=icon_eu` (full grid) or
/// `?provider=icon_eu&time=2026-05-26T12:00:00Z` (one forecast instant).
#[derive(Debug, Deserialize)]
pub struct AllWindsQuery {
    provider: Provider,
    /// A single forecast instant to serve. When omitted the whole run is
    /// returned (backward compatible); when present, only that time's cells are
    /// encoded, so a client can fetch a run one slice at a time in parallel.
    #[serde(default)]
    time: Option<DateTime<Utc>>,
}

/// Serve the latest run for a provider as a bincode envelope of the run
/// timestamp followed by a `WeatherMap`. Unlike `/api/get_winds`, this applies
/// no pressure/area filter.
///
/// Without `time`, the payload is the *entire* run. With `time=T`, it is a
/// single-key map holding just that forecast instant — the same envelope shape,
/// so the client decodes both forms identically and can merge per-time slices.
///
/// 404 until a run is cached for the provider, or when `time` names an instant
/// the current run doesn't contain; an unknown `provider` value is rejected as
/// 400 by the query deserializer.
pub async fn get_all_winds(
    State(state): State<AppState>,
    Query(q): Query<AllWindsQuery>,
) -> Result<impl IntoResponse, AppError> {
    // One cache lookup yields both the run timestamp and the shared
    // `Arc<WeatherMap>`, so the timestamp we encode provably labels these cells.
    let run = state.latest_run(q.provider)?;
    let bytes = encode_run(&run, q.time)?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes))
}

/// Encode a run as the `/api/winds` bincode envelope: the whole grid when
/// `time` is `None`, or a single-key slice for one forecast instant. A
/// single-key slice clones just that instant's levels — cheap next to the full
/// grid, and the BTreeMap lookup never visits the other times.
fn encode_run(run: &CachedRun, time: Option<DateTime<Utc>>) -> Result<Vec<u8>, AppError> {
    let bytes = match time {
        None => forecast::deserializer::to_binary_with_run(run.run_time, &run.map)?,
        Some(time) => {
            let levels = run.map.0.get(&time).ok_or_else(|| {
                AppError::NotFound(format!("forecast time {time} is not in the current run"))
            })?;
            let mut slice = BTreeMap::new();
            slice.insert(time, levels.clone());
            forecast::deserializer::to_binary_with_run(run.run_time, &WeatherMap(slice))?
        }
    };
    Ok(bytes)
}

/// Query for `/api/get_weather`, e.g.
/// `?provider=icon_eu&time=2026-05-26T12:00:00Z&pressure=85000&lat=50.4&lon=30.
/// 5`.
#[derive(Debug, Deserialize)]
pub struct WeatherQuery {
    provider: Provider,
    /// Forecast instant (RFC3339). Interpolated between bracketing forecast
    /// hours.
    time: DateTime<Utc>,
    /// Pressure level in Pascals; higher pressure = closer to the ground.
    pressure: u32,
    lat: f64,
    lon: f64,
}

/// Interpolate the wind at a single (time, pressure, lat, lon) from the latest
/// run for `provider`, returned as JSON. Invalid coordinates (off the planet)
/// yield 400; a coordinate outside the grid's covered box, or no run decoded
/// yet, yields 404. Time and pressure clamp to the nearest forecast level.
pub async fn get_weather(
    State(state): State<AppState>,
    Query(q): Query<WeatherQuery>,
) -> Result<impl IntoResponse, AppError> {
    if !(-90.0..=90.0).contains(&q.lat) || !(-180.0..=180.0).contains(&q.lon) {
        return Err(AppError::BadRequest("lat must be in -90..=90, lon in -180..=180".into()));
    }

    // Same warm per-provider cache as `get_winds`; interpolates by reference on
    // the shared `Arc<WeatherMap>` — no whole-grid clone.
    let map = state.latest_winds(q.provider)?;
    let weather = map
        .get_weather(q.time, q.pressure, q.lat, q.lon)
        .ok_or_else(|| AppError::NotFound("requested point is outside the grid coverage".into()))?;

    Ok(Json(WeatherResponse::from(&weather)))
}

/// Return the current forecast run for every provider.
///
/// Response is a JSON object keyed by provider slug (`"icon_eu"`, `"noaa"`,
/// `"ecmwf"`).
/// A provider with no run cached yet (cold start before any warm/scheduler
/// tick) appears as `null`. Because the cache is the single source of truth,
/// the run timestamp returned here is exactly what `/api/get_winds` and
/// `/api/get_weather` will serve at this moment.
pub async fn get_info(State(state): State<AppState>) -> impl IntoResponse {
    let mut map: BTreeMap<&str, Option<RunInfo>> = BTreeMap::new();
    for &provider in Provider::all() {
        // `latest_run` (not `run_info`) so the times/pressures are derived from
        // the same cached map the timestamp labels — a single lookup, no decode.
        let info = state.latest_run(provider).ok().map(|run| run_info_from(&run));
        map.insert(provider.slug(), info);
    }
    Json(map)
}

/// Summarise a cached run as a [`RunInfo`]: its timestamp, forecast instants
/// (time keys, ascending), and pressure levels (the union of level keys across
/// times, ascending). Reads only the map's keys — no cell is touched.
fn run_info_from(run: &CachedRun) -> RunInfo {
    let times = run.map.0.keys().copied().collect();
    let mut pressures: BTreeSet<u32> = BTreeSet::new();
    for levels in run.map.0.values() {
        pressures.extend(levels.keys().copied());
    }
    RunInfo { run: run.run_time, times, pressures: pressures.into_iter().collect() }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::TimeZone;
    use ordered_float::OrderedFloat;

    use super::*;
    use crate::forecast::weather::Weather;
    use crate::forecast::{LatMap, LevelMap, LonMap};

    fn dt(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 25, hour, 0, 0).unwrap()
    }

    /// A cached run with one cell per `(time, pressure)`, two pressures (50000,
    /// 85000 Pa), at the given forecast hours. Run timestamp is fixed at 00:00.
    fn cached(hours: &[u32]) -> CachedRun {
        let mut grid = BTreeMap::new();
        for &h in hours {
            let mut levels = LevelMap::new();
            for pa in [50_000u32, 85_000] {
                let mut lons = LonMap::new();
                lons.insert(OrderedFloat(30.0), Weather::new(1.0, 2.0));
                let mut lats = LatMap::new();
                lats.insert(OrderedFloat(50.0), lons);
                levels.insert(pa, lats);
            }
            grid.insert(dt(h), levels);
        }
        CachedRun { run_time: dt(0), map: Arc::new(WeatherMap(grid)) }
    }

    #[test]
    fn weather_response_derives_speed_and_nav_bearing() {
        // u=3.4, v=-1.2 -> speed = hypot(3.4, 1.2) ≈ 3.606,
        // nav bearing (0°=N, 90°=E) ≈ 109.44°.
        let resp = WeatherResponse::from(&Weather::new(3.4, -1.2));
        // Components round-trip through the stored f32, so the f64 response value
        // is the exact f32→f64 upcast (3.4 isn't f32-exact) — compare with a tol.
        assert!((resp.u_wind - 3.4).abs() < 1e-6, "u_wind was {}", resp.u_wind);
        assert!((resp.v_wind - -1.2).abs() < 1e-6, "v_wind was {}", resp.v_wind);
        assert!((resp.speed - 3.605_551).abs() < 1e-6, "speed was {}", resp.speed);
        assert!((resp.bearing_deg - 109.440_035).abs() < 1e-4, "bearing was {}", resp.bearing_deg);
    }

    #[test]
    fn run_info_from_lists_times_and_pressures_ascending() {
        let info = run_info_from(&cached(&[6, 7, 8]));
        assert_eq!(info.run, dt(0));
        assert_eq!(info.times, vec![dt(6), dt(7), dt(8)]);
        assert_eq!(info.pressures, vec![50_000, 85_000]);
    }

    #[test]
    fn encode_run_full_grid_round_trips_every_time() {
        let bytes = encode_run(&cached(&[6, 7]), None).expect("encodes");
        let (run, map) = weathergrid::codec::decode_winds_with_run(&bytes).expect("decodes");
        assert_eq!(run, dt(0));
        assert_eq!(map.keys().copied().collect::<Vec<_>>(), vec![dt(6), dt(7)]);
    }

    #[test]
    fn encode_run_single_time_returns_only_that_slice() {
        let bytes = encode_run(&cached(&[6, 7, 8]), Some(dt(7))).expect("encodes");
        let (run, map) = weathergrid::codec::decode_winds_with_run(&bytes).expect("decodes");
        assert_eq!(run, dt(0), "the slice still carries the run timestamp");
        assert_eq!(map.keys().copied().collect::<Vec<_>>(), vec![dt(7)], "only the asked time");
    }

    #[test]
    fn encode_run_missing_time_is_not_found() {
        let err = encode_run(&cached(&[6, 7]), Some(dt(9))).unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }
}
