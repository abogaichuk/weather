use std::collections::BTreeMap;
use std::io::Read;

use bzip2::read::BzDecoder;
use chrono::{DateTime, Duration, Timelike, Utc};
use futures::{StreamExt, stream};
// `latlons()` is a trait method as of grib 0.15 (was inherent in 0.10); the
// trait must be in scope to call it on a submessage.
use grib::LatLons;
use ordered_float::OrderedFloat;
use reqwest::Client;

use crate::config::BoundingBox;
use crate::downloader::CONCURRENT_REQUESTS;
use crate::errors::AppError;
use crate::forecast::WeatherMap;
use crate::forecast::weather::Weather;

const BASE: &str = "https://opendata.dwd.de/weather/nwp/icon-eu/grib";
const USER_AGENT: &str = "temporal-falcon/0.1";

/// Maximum altitude (km) the downloader should fetch data for.
/// Bump this if you need higher-altitude flights; the level selector will
/// include enough pressure levels to bracket any point up to this altitude.
pub const MAX_ALTITUDE_KM: f64 = 20.0;

/// All ICON-EU pressure levels (hPa) with their approximate altitudes (km)
/// per the ICAO standard atmosphere. Sorted ascending by altitude — bottom
/// of the atmosphere first.
const ICON_EU_LEVELS: &[(u16, f64)] = &[
    (1000, 0.11),
    (950, 0.54),
    (925, 0.76),
    (900, 0.99),
    (875, 1.22),
    (850, 1.46),
    (825, 1.71),
    (800, 1.95),
    (775, 2.21),
    (700, 3.01),
    (600, 4.21),
    (500, 5.57),
    (400, 7.18),
    (300, 9.16),
    (250, 10.36),
    (200, 11.78),
    (150, 13.61),
    (100, 16.18),
    (70, 18.44),
    (50, 20.58),
];

/// Pressure levels (hPa) to fetch, given a maximum altitude in km. Returns
/// every level at or below the limit, plus the first level above — that
/// bracketing level is what makes interpolation valid right up to the
/// requested ceiling.
fn levels_to_fetch(max_altitude_km: f64) -> Vec<u16> {
    let mut levels: Vec<u16> = ICON_EU_LEVELS
        .iter()
        .filter(|(_, alt_km)| *alt_km <= max_altitude_km)
        .map(|(hpa, _)| *hpa)
        .collect();
    if let Some((hpa, _)) = ICON_EU_LEVELS.iter().find(|(_, alt_km)| *alt_km > max_altitude_km) {
        levels.push(*hpa);
    }
    levels
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Var {
    U,
    V,
}

impl Var {
    fn as_lower(self) -> &'static str {
        match self {
            Var::U => "u",
            Var::V => "v",
        }
    }

    fn as_upper(self) -> &'static str {
        match self {
            Var::U => "U",
            Var::V => "V",
        }
    }
}

fn build_url(run: DateTime<Utc>, step: u16, var: Var, level_hpa: u16) -> String {
    let rr = format!("{:02}", run.hour());
    let date = run.date_naive().format("%Y%m%d");
    format!(
        "{base}/{rr}/{lower}/icon-eu_europe_regular-lat-lon_pressure-level_{date}{rr}_{step:03}_{level}_{upper}.grib2.bz2",
        base = BASE,
        lower = var.as_lower(),
        upper = var.as_upper(),
        level = level_hpa,
    )
}

fn decompress_bz2(url: &str, compressed: &[u8]) -> Result<Vec<u8>, AppError> {
    let mut decoder = BzDecoder::new(compressed);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| AppError::Decompress { url: url.to_string(), source: e })?;
    Ok(out)
}

type GridKey = (OrderedFloat<f64>, OrderedFloat<f64>);
type Grid = BTreeMap<GridKey, f64>;

/// Parse a single-variable, single-level ICON-EU GRIB2 file into a (lat, lon) →
/// value map, trimming to the configured bbox in-stream so the full 657×1377
/// grid is never materialised.
fn parse_grib_trimmed(bytes: &[u8], bbox: BoundingBox) -> Result<Grid, AppError> {
    let cursor = std::io::Cursor::new(bytes);
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
            let lon_f: f64 = lon.into();
            if lat_range.contains(&lat_f) && lon_range.contains(&lon_f) {
                out.insert((OrderedFloat(lat_f), OrderedFloat(lon_f)), value.into());
            }
        }
    }
    Ok(out)
}

struct FetchJob {
    fly_dt: DateTime<Utc>,
    pressure_pa: u32,
    var: Var,
    url: String,
    bbox: BoundingBox,
}

struct FetchResult {
    fly_dt: DateTime<Utc>,
    pressure_pa: u32,
    var: Var,
    grid: Grid,
}

async fn fetch_one(client: Client, job: FetchJob) -> Result<FetchResult, AppError> {
    let resp = client
        .get(&job.url)
        .send()
        .await
        .map_err(|e| AppError::HttpRequest { url: job.url.clone(), source: e })?;
    if !resp.status().is_success() {
        tracing::error!(url = %job.url, status = %resp.status(), "icon-eu HTTP error");
        return Err(AppError::HttpStatus { url: job.url.clone(), status: resp.status().as_u16() });
    }
    let compressed = resp
        .bytes()
        .await
        .map_err(|e| AppError::HttpRequest { url: job.url.clone(), source: e })?;
    // bz2 decompress + GRIB parse are CPU-bound — offload so the reactor stays
    // free to drive the other buffer_unordered fetches.
    let url = job.url.clone();
    let bbox = job.bbox;
    let grid = tokio::task::spawn_blocking(move || -> Result<Grid, AppError> {
        let decompressed = decompress_bz2(&url, &compressed)?;
        parse_grib_trimmed(&decompressed, bbox)
    })
    .await
    .map_err(|e| AppError::Config(format!("icon-eu decode panicked: {e}")))??;
    Ok(FetchResult { fly_dt: job.fly_dt, pressure_pa: job.pressure_pa, var: job.var, grid })
}

// Working cell: (u-wind, v-wind), each filled in as fetches arrive.
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

pub async fn download(
    forecast_issued: DateTime<Utc>,
    bbox: BoundingBox,
    hours: u16,
) -> Result<WeatherMap, AppError> {
    let client = Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| AppError::HttpRequest { url: String::new(), source: e })?;

    let levels = levels_to_fetch(MAX_ALTITUDE_KM);
    let jobs: Vec<FetchJob> = (0..hours)
        .flat_map(|h| {
            let fly_dt = forecast_issued + Duration::hours(h.into());
            let step_signed = fly_dt.signed_duration_since(forecast_issued).num_hours();
            let step = step_signed.max(0) as u16;
            levels.iter().flat_map(move |&level_hpa| {
                [Var::U, Var::V].into_iter().map(move |var| FetchJob {
                    fly_dt,
                    pressure_pa: u32::from(level_hpa) * 100,
                    var,
                    url: build_url(forecast_issued, step, var, level_hpa),
                    bbox,
                })
            })
        })
        .collect();

    tracing::info!(jobs = jobs.len(), "icon-eu: dispatching fetches");

    let (working, failures) = stream::iter(jobs)
        .map(|job| {
            let client = client.clone();
            async move { fetch_one(client, job).await }
        })
        .buffer_unordered(CONCURRENT_REQUESTS)
        .fold((WorkingMap::new(), 0usize), |(mut acc, mut fails), result| async move {
            match result {
                Ok(r) => merge_into(&mut acc, r),
                Err(_) => fails += 1, // already logged in fetch_one
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

    #[test]
    fn build_url_matches_verified_pattern() {
        let run = Utc.with_ymd_and_hms(2026, 5, 20, 0, 0, 0).unwrap();
        let url = build_url(run, 6, Var::U, 500);
        assert_eq!(
            url,
            "https://opendata.dwd.de/weather/nwp/icon-eu/grib/00/u/\
             icon-eu_europe_regular-lat-lon_pressure-level_2026052000_006_500_U.grib2.bz2"
        );
    }

    #[test]
    fn levels_sorted_ascending_by_altitude() {
        let mut prev = -1.0_f64;
        for &(_, alt) in ICON_EU_LEVELS {
            assert!(alt > prev, "altitudes must be strictly ascending: {alt} after {prev}");
            prev = alt;
        }
    }

    #[test]
    fn levels_to_fetch_at_20km_includes_all_levels() {
        // The whole 20-level table fits under the 20 km ceiling once we add the
        // bracketing level above (50 hPa at 20.58 km).
        let levels = levels_to_fetch(20.0);
        assert_eq!(levels.len(), 20);
        assert!(levels.contains(&50), "50 hPa bracketing level missing: {levels:?}");
        assert!(levels.contains(&1000), "1000 hPa surface level missing: {levels:?}");
    }

    #[test]
    fn levels_to_fetch_at_5km_picks_low_levels_with_bracket() {
        // Altitudes ≤ 5 km: 1000, 950, 925, 900, 875, 850, 825, 800, 775, 700, 600 (=
        // 11) Bracketing level above 5 km: 500 hPa at 5.57 km (= 1)
        let levels = levels_to_fetch(5.0);
        assert_eq!(levels.len(), 12, "got {levels:?}");
        assert!(levels.contains(&500), "missing bracketing level 500 hPa");
        assert!(!levels.contains(&400), "should not include 400 hPa (above bracket)");
    }

    #[test]
    fn levels_to_fetch_at_zero_picks_only_bracket() {
        // No level is at altitude ≤ 0, so only the first above (1000 hPa at 0.11 km) is
        // fetched.
        let levels = levels_to_fetch(0.0);
        assert_eq!(levels, vec![1000]);
    }

    #[test]
    fn levels_to_fetch_above_atmosphere_returns_all() {
        // Above every entry → no bracketing addition needed.
        let levels = levels_to_fetch(100.0);
        assert_eq!(levels.len(), ICON_EU_LEVELS.len());
    }
}
