//! ECMWF Open Data downloader (currently the AIFS data-driven model).
//!
//! Unlike ICON-EU/GFS — where each (variable, level, step) is a separate file
//! or a filter-CGI subset — ECMWF packs every field for a step into one global
//! `.grib2`, paired with a `.index` JSON-lines sidecar listing each message's
//! byte `_offset`/`_length`. We read the index, pick the u/v pressure-level
//! messages, and pull *only* those byte ranges via HTTP `Range` requests — so
//! we transfer a few MB per step instead of the whole multi-variable file. Each
//! message is a global field (CCSDS-packed, decoded via grib's libaec backend),
//! trimmed to the configured bbox after decode.

use std::collections::BTreeMap;
use std::io::Cursor;

use chrono::{DateTime, Duration, Timelike, Utc};
use futures::{StreamExt, stream};
use grib::LatLons;
use ordered_float::OrderedFloat;
use reqwest::header::{RANGE, RETRY_AFTER};
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;

use crate::config::BoundingBox;
use crate::errors::AppError;
use crate::forecast::WeatherMap;
use crate::forecast::weather::Weather;

const BASE: &str = "https://data.ecmwf.int/forecasts";
const USER_AGENT: &str = "temporal-falcon/0.1";

/// Requests to `data.ecmwf.int` are issued **one at a time** (unlike the
/// DWD/NOAA endpoints, which tolerate parallel fetches). ECMWF rate-limits per
/// IP and returns 429s under bursts, so serialising — combined with
/// [`ECMWF_REQUEST_DELAY`] — keeps us a polite, well-behaved client.
const ECMWF_CONCURRENCY: usize = 1;

/// Minimum pause before every request to ECMWF. With [`ECMWF_CONCURRENCY`] == 1
/// this caps our request rate at ~1/s so we never hammer (or get throttled by)
/// the endpoint.
const ECMWF_REQUEST_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// How many times to retry a single request on a rate-limit / transient
/// upstream error before giving up on it.
const ECMWF_MAX_RETRIES: u32 = 3;

/// The model-specific parts of an ECMWF Open Data stream, isolated here so a
/// second model (e.g. IFS HRES — `ifs/0p25/oper`, different cadence) can be
/// added later as another value without touching the download pipeline.
struct EcmwfModel {
    /// Path segment after the `{date}/{run}z/` directory.
    path: &'static str,
    /// Filename suffix after the `{timestamp}-{step}h-` prefix.
    file_kind: &'static str,
    /// Spacing (hours) between published forecast steps.
    step_hours: u16,
}

/// AIFS deterministic single run: global 0.25°, 6-hourly steps.
const AIFS: EcmwfModel =
    EcmwfModel { path: "aifs-single/0p25/oper", file_kind: "oper-fc", step_hours: 6 };

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Var {
    U,
    V,
}

/// One line of an ECMWF `.index` file (only the fields we use).
#[derive(Debug, Deserialize)]
struct IndexEntry {
    param: String,
    levtype: String,
    levelist: Option<String>,
    #[serde(rename = "_offset")]
    offset: u64,
    #[serde(rename = "_length")]
    length: u64,
}

/// A single u/v pressure-level field to pull by byte range from a step's
/// `.grib2`.
#[derive(Debug)]
struct RangeJob {
    fly_dt: DateTime<Utc>,
    pressure_pa: u32,
    var: Var,
    grib_url: String,
    offset: u64,
    length: u64,
    bbox: BoundingBox,
}

/// Build the shared `.../{timestamp}-{step}h-{kind}` URL stem for a run/step;
/// callers append `.index` or `.grib2`.
fn file_stem(model: &EcmwfModel, run: DateTime<Utc>, step: u16) -> String {
    let date = run.date_naive().format("%Y%m%d");
    let rr = format!("{:02}", run.hour());
    format!(
        "{BASE}/{date}/{rr}z/{path}/{date}{rr}0000-{step}h-{kind}",
        path = model.path,
        kind = model.file_kind,
    )
}

/// ECMWF global grids span longitudes in `[0, 360)`. Normalise to the signed
/// `[-180, 180)` convention the other providers store, so a single `lon` query
/// param means the same thing across providers and bbox comparison is correct
/// west of the prime meridian.
fn normalize_lon(lon: f64) -> f64 {
    if lon >= 180.0 { lon - 360.0 } else { lon }
}

/// Turn a `.index` body into the u/v pressure-level range jobs for one step.
/// Pure (no I/O) so it can be unit-tested against a fixture.
fn parse_index_jobs(
    body: &str,
    grib_url: &str,
    fly_dt: DateTime<Utc>,
    bbox: BoundingBox,
) -> Result<Vec<RangeJob>, AppError> {
    let mut jobs = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: IndexEntry = serde_json::from_str(line)
            .map_err(|e| AppError::Config(format!("ecmwf index parse: {e}")))?;

        // Keep only u/v winds on pressure levels; skip every other field.
        let var = match entry.param.as_str() {
            "u" => Var::U,
            "v" => Var::V,
            _ => continue,
        };
        if entry.levtype != "pl" {
            continue;
        }
        let Some(level_hpa) = entry.levelist.as_deref().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };

        jobs.push(RangeJob {
            fly_dt,
            pressure_pa: level_hpa * 100,
            var,
            grib_url: grib_url.to_string(),
            offset: entry.offset,
            length: entry.length,
            bbox,
        });
    }
    Ok(jobs)
}

type GridKey = (OrderedFloat<f64>, OrderedFloat<f64>);
type Grid = BTreeMap<GridKey, f64>;

/// Decode one CCSDS-packed GRIB2 message (a single global field) and trim to
/// `bbox`, normalising longitudes to the signed convention. The full global
/// grid is iterated but only in-box cells are retained.
fn decode_trimmed(bytes: &[u8], bbox: BoundingBox) -> Result<Grid, AppError> {
    let cursor = Cursor::new(bytes);
    let grib2 = grib::from_reader(cursor).map_err(|e| AppError::Grib(e.to_string()))?;
    let lat_range = bbox.bottom_lat..=bbox.top_lat;
    let lon_range = bbox.left_lon..=bbox.right_lon;
    let mut out = Grid::new();
    for (_, submessage) in grib2.iter() {
        let latlons = submessage.latlons().map_err(|e| AppError::Grib(e.to_string()))?;
        let decoder = grib::Grib2SubmessageDecoder::from(submessage)
            .map_err(|e| AppError::Grib(e.to_string()))?;
        let values = decoder.dispatch().map_err(|e| AppError::Grib(e.to_string()))?;
        for ((lat, lon), value) in latlons.zip(values) {
            let lat_f: f64 = lat.into();
            let lon_f = normalize_lon(lon.into());
            if lat_range.contains(&lat_f) && lon_range.contains(&lon_f) {
                out.insert((OrderedFloat(lat_f), OrderedFloat(lon_f)), value.into());
            }
        }
    }
    Ok(out)
}

struct FetchResult {
    fly_dt: DateTime<Utc>,
    pressure_pa: u32,
    var: Var,
    grid: Grid,
}

/// Whether a status code is worth retrying: rate-limit or transient upstream
/// errors. A 404 (run not published yet) is *not* retryable — it fails fast.
fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || matches!(
            status,
            StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
        )
}

/// Server-suggested wait from a `Retry-After: <seconds>` header, if present.
fn retry_after(resp: &Response) -> Option<std::time::Duration> {
    resp.headers()
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(std::time::Duration::from_secs)
}

/// Exponential backoff for retry `attempt` (0-based): 1s, 2s, 4s, …
fn backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_secs(1u64 << attempt)
}

/// GET `url` (optionally a byte `range`) as a polite ECMWF client: sleep
/// [`ECMWF_REQUEST_DELAY`] before *every* attempt, and on a 429/transient error
/// back off (honouring `Retry-After`) and retry up to [`ECMWF_MAX_RETRIES`].
/// Non-retryable failures (e.g. 404) and exhausted retries surface as
/// [`AppError::HttpStatus`].
async fn get_with_retry(
    client: &Client,
    url: &str,
    range: Option<String>,
) -> Result<Response, AppError> {
    let mut attempt = 0;
    loop {
        // Throttle before *sending* — caps our rate and spaces out retries too.
        tokio::time::sleep(ECMWF_REQUEST_DELAY).await;

        let mut req = client.get(url);
        if let Some(range) = &range {
            req = req.header(RANGE, range);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AppError::HttpRequest { url: url.to_string(), source: e })?;

        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }

        if is_retryable(status) && attempt < ECMWF_MAX_RETRIES {
            let wait = retry_after(&resp).unwrap_or_else(|| backoff(attempt));
            tracing::warn!(%url, %status, ?wait, attempt, "ecmwf throttled/transient — retrying");
            tokio::time::sleep(wait).await;
            attempt += 1;
            continue;
        }

        tracing::error!(%url, %status, "ecmwf HTTP error");
        return Err(AppError::HttpStatus { url: url.to_string(), status: status.as_u16() });
    }
}

/// Range-fetch one field's bytes and decode them off the reactor.
async fn fetch_one(client: Client, job: RangeJob) -> Result<FetchResult, AppError> {
    let end = job.offset + job.length - 1;
    let resp =
        get_with_retry(&client, &job.grib_url, Some(format!("bytes={}-{}", job.offset, end)))
            .await?;
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| AppError::HttpRequest { url: job.grib_url.clone(), source: e })?;

    // CCSDS/libaec decode is CPU-bound — keep it off the async reactor.
    let bbox = job.bbox;
    let grid = tokio::task::spawn_blocking(move || decode_trimmed(&bytes, bbox))
        .await
        .map_err(|e| AppError::Config(format!("ecmwf decode panicked: {e}")))??;

    Ok(FetchResult { fly_dt: job.fly_dt, pressure_pa: job.pressure_pa, var: job.var, grid })
}

// Working cell: (u-wind, v-wind), filled in as range fetches arrive in any
// order — mirrors the icon_eu staging.
type WorkingCell = (Option<f64>, Option<f64>);
type WorkingMap = BTreeMap<
    DateTime<Utc>,
    BTreeMap<u32, BTreeMap<OrderedFloat<f64>, BTreeMap<OrderedFloat<f64>, WorkingCell>>>,
>;

fn merge_into(acc: &mut WorkingMap, result: FetchResult) {
    let level_map = acc.entry(result.fly_dt).or_default();
    let lat_map = level_map.entry(result.pressure_pa).or_default();
    for ((lat, lon), value) in result.grid {
        let lon_map = lat_map.entry(lat).or_default();
        let cell = lon_map.entry(lon).or_insert((None, None));
        match result.var {
            Var::U => cell.0 = Some(value),
            Var::V => cell.1 = Some(value),
        }
    }
}

/// Collapse staged cells into a [`WeatherMap`], keeping only cells where both U
/// and V landed.
fn finalize(working: WorkingMap) -> WeatherMap {
    let inner = working
        .into_iter()
        .map(|(dt, level_map)| {
            let level_map = level_map
                .into_iter()
                .map(|(p, lat_map)| {
                    let lat_map = lat_map
                        .into_iter()
                        .map(|(lat, lon_map)| {
                            let lon_map = lon_map
                                .into_iter()
                                .filter_map(|(lon, (u, v))| match (u, v) {
                                    (Some(u), Some(v)) => Some((lon, Weather::new(u, v))),
                                    _ => None,
                                })
                                .collect();
                            (lat, lon_map)
                        })
                        .collect();
                    (p, lat_map)
                })
                .collect();
            (dt, level_map)
        })
        .collect();
    WeatherMap(inner)
}

/// Fetch one step's `.index` and turn it into range jobs.
async fn fetch_step_jobs(
    client: Client,
    model: &EcmwfModel,
    run: DateTime<Utc>,
    step: u16,
    bbox: BoundingBox,
) -> Result<Vec<RangeJob>, AppError> {
    let stem = file_stem(model, run, step);
    let index_url = format!("{stem}.index");
    let grib_url = format!("{stem}.grib2");

    let resp = get_with_retry(&client, &index_url, None).await?;
    let body = resp
        .text()
        .await
        .map_err(|e| AppError::HttpRequest { url: index_url.clone(), source: e })?;

    let fly_dt = run + Duration::hours(i64::from(step));
    parse_index_jobs(&body, &grib_url, fly_dt, bbox)
}

pub async fn download(
    forecast_issued: DateTime<Utc>,
    bbox: BoundingBox,
    hours: u16,
) -> Result<WeatherMap, AppError> {
    let client = Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| AppError::HttpRequest { url: String::new(), source: e })?;
    let model = &AIFS;

    // Steps 0, step_hours, 2*step_hours, … up to and including the horizon.
    let steps: Vec<u16> = (0..=hours).step_by(usize::from(model.step_hours)).collect();

    // Phase 1: fetch every step's index in parallel and flatten into range jobs.
    let jobs: Vec<RangeJob> = stream::iter(steps)
        .map(|step| {
            let client = client.clone();
            async move { fetch_step_jobs(client, model, forecast_issued, step, bbox).await }
        })
        .buffer_unordered(ECMWF_CONCURRENCY)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();

    tracing::info!(jobs = jobs.len(), "ecmwf: dispatching range fetches");

    // Phase 2: range-fetch + decode each field in parallel, staging into cells.
    let (working, failures) = stream::iter(jobs)
        .map(|job| {
            let client = client.clone();
            async move { fetch_one(client, job).await }
        })
        .buffer_unordered(ECMWF_CONCURRENCY)
        .fold((WorkingMap::new(), 0usize), |(mut acc, mut fails), result| async move {
            match result {
                Ok(r) => merge_into(&mut acc, r),
                Err(err) => {
                    tracing::error!(?err, "ecmwf field fetch failed");
                    fails += 1;
                }
            }
            (acc, fails)
        })
        .await;

    if failures > 0 {
        return Err(AppError::PartialDownload { count: failures });
    }
    Ok(finalize(working))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn bbox() -> BoundingBox {
        BoundingBox { top_lat: 58.5, bottom_lat: 46.25, left_lon: 22.5, right_lon: 50.75 }
    }

    /// End-to-end proof against the live server: fetch step 0 of yesterday's
    /// 00z AIFS run, decode the CCSDS fields, and confirm we get in-box cells
    /// with finite winds at the expected pressure levels. Ignored by default
    /// (network + a few MB) — run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "hits the live ECMWF Open Data server"]
    async fn live_download_step0_decodes_winds() {
        use chrono::{Duration, Utc};

        // Yesterday 00z is always published by now; ECMWF retains only a few days.
        let run =
            (Utc::now() - Duration::days(1)).date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();

        let map = download(run, bbox(), 0).await.expect("live download should succeed");
        assert!(!map.0.is_empty(), "expected at least the step-0 time slice");

        let (_, levels) = map.0.iter().next().unwrap();
        assert!(levels.contains_key(&85_000), "expected the 850 hPa level (85000 Pa)");

        // Every retained cell must carry finite winds and sit inside the bbox.
        let b = bbox();
        for lats in levels.values() {
            for (lat, lons) in lats {
                for (lon, w) in lons {
                    assert!(w.speed().is_finite(), "wind speed must be finite");
                    assert!((b.bottom_lat..=b.top_lat).contains(&lat.0), "lat in box");
                    assert!((b.left_lon..=b.right_lon).contains(&lon.0), "lon in box");
                }
            }
        }
    }

    #[test]
    fn file_stem_matches_live_naming() {
        // Confirmed against data.ecmwf.int: timestamp is {YYYYMMDD}{HH}0000 and
        // the step is unpadded hours.
        let run = Utc.with_ymd_and_hms(2026, 6, 16, 0, 0, 0).unwrap();
        assert_eq!(
            file_stem(&AIFS, run, 0),
            "https://data.ecmwf.int/forecasts/20260616/00z/aifs-single/0p25/oper/\
             20260616000000-0h-oper-fc"
        );
        let run12 = Utc.with_ymd_and_hms(2026, 6, 16, 12, 0, 0).unwrap();
        assert_eq!(
            file_stem(&AIFS, run12, 18),
            "https://data.ecmwf.int/forecasts/20260616/12z/aifs-single/0p25/oper/\
             20260616120000-18h-oper-fc"
        );
    }

    #[test]
    fn normalize_lon_maps_eastern_hemisphere_unchanged_and_western_wrapped() {
        assert_eq!(normalize_lon(30.0), 30.0); // Europe stays as-is
        assert_eq!(normalize_lon(0.0), 0.0);
        assert_eq!(normalize_lon(179.75), 179.75);
        assert_eq!(normalize_lon(180.0), -180.0);
        assert_eq!(normalize_lon(350.0), -10.0); // 10°W
    }

    #[test]
    fn parse_index_jobs_keeps_only_uv_pressure_levels() {
        // Mirrors real .index lines: w/z/q and a surface u are present and must
        // be skipped; only u/v on pressure levels survive.
        let body = "\
{\"param\": \"w\", \"levtype\": \"pl\", \"levelist\": \"600\", \"_offset\": 0, \"_length\": 100}
{\"param\": \"u\", \"levtype\": \"pl\", \"levelist\": \"250\", \"_offset\": 100, \"_length\": 200}
{\"param\": \"v\", \"levtype\": \"pl\", \"levelist\": \"250\", \"_offset\": 300, \"_length\": 210}
{\"param\": \"u\", \"levtype\": \"sfc\", \"_offset\": 510, \"_length\": 50}
{\"param\": \"t\", \"levtype\": \"pl\", \"levelist\": \"850\", \"_offset\": 560, \"_length\": 70}
";
        let fly = Utc.with_ymd_and_hms(2026, 6, 16, 6, 0, 0).unwrap();
        let jobs = parse_index_jobs(body, "g.grib2", fly, bbox()).unwrap();

        assert_eq!(jobs.len(), 2, "only u/v on pressure levels: {jobs:?}");
        assert_eq!(jobs[0].var, Var::U);
        assert_eq!(jobs[0].pressure_pa, 25_000); // 250 hPa → Pa
        assert_eq!((jobs[0].offset, jobs[0].length), (100, 200));
        assert_eq!(jobs[1].var, Var::V);
        assert_eq!(jobs[1].pressure_pa, 25_000);
        assert_eq!((jobs[1].offset, jobs[1].length), (300, 210));
        assert_eq!(jobs[1].fly_dt, fly);
    }

    #[test]
    fn parse_index_jobs_rejects_malformed_line() {
        let err = parse_index_jobs("not json", "g.grib2", Utc::now(), bbox());
        assert!(matches!(err, Err(AppError::Config(_))));
    }

    #[test]
    fn only_rate_limit_and_transient_statuses_are_retryable() {
        assert!(is_retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable(StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_retryable(StatusCode::BAD_GATEWAY));
        assert!(is_retryable(StatusCode::GATEWAY_TIMEOUT));
        // 404 = run not published yet → must fail fast, never retry.
        assert!(!is_retryable(StatusCode::NOT_FOUND));
        assert!(!is_retryable(StatusCode::FORBIDDEN));
    }

    #[test]
    fn backoff_grows_exponentially() {
        assert_eq!(backoff(0), std::time::Duration::from_secs(1));
        assert_eq!(backoff(1), std::time::Duration::from_secs(2));
        assert_eq!(backoff(2), std::time::Duration::from_secs(4));
    }

    #[test]
    fn finalize_keeps_only_cells_with_both_components() {
        let mut working: WorkingMap = WorkingMap::new();
        let dt = Utc.with_ymd_and_hms(2026, 6, 16, 0, 0, 0).unwrap();
        let complete = FetchResult {
            fly_dt: dt,
            pressure_pa: 85_000,
            var: Var::U,
            grid: Grid::from([((OrderedFloat(50.0), OrderedFloat(30.0)), 1.0)]),
        };
        let complete_v = FetchResult {
            fly_dt: dt,
            pressure_pa: 85_000,
            var: Var::V,
            grid: Grid::from([((OrderedFloat(50.0), OrderedFloat(30.0)), 2.0)]),
        };
        // A lone U with no matching V — must be dropped.
        let lonely = FetchResult {
            fly_dt: dt,
            pressure_pa: 85_000,
            var: Var::U,
            grid: Grid::from([((OrderedFloat(51.0), OrderedFloat(30.0)), 9.0)]),
        };
        merge_into(&mut working, complete);
        merge_into(&mut working, complete_v);
        merge_into(&mut working, lonely);

        let map = finalize(working);
        let lats = &map.0[&dt][&85_000];
        assert_eq!(lats[&OrderedFloat(50.0)][&OrderedFloat(30.0)], Weather::new(1.0, 2.0));
        assert!(
            lats.get(&OrderedFloat(51.0)).is_none_or(|lons| lons.is_empty()),
            "cell missing its V component must not survive"
        );
    }
}
