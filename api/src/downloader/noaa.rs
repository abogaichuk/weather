use std::collections::BTreeMap;
use std::io::Cursor;

use bytes::Bytes;
use chrono::{DateTime, TimeDelta, Timelike, Utc};
use futures::{StreamExt, stream};
// `latlons()` is a trait method as of grib 0.15 (was inherent in 0.10).
use grib::{LatLons, ProdDefinition};
use ordered_float::OrderedFloat;
use reqwest::Client;
use tokio::try_join;

use crate::config::BoundingBox;
use crate::downloader::CONCURRENT_REQUESTS;
use crate::errors::AppError;
use crate::forecast::WeatherMap;
use crate::forecast::weather::Weather;

pub async fn download(
    forecast_issued: DateTime<Utc>,
    bbox: BoundingBox,
    hours: u16,
) -> Result<WeatherMap, AppError> {
    let client = Client::new();

    let responses = stream::iter(0..hours)
        .map(|flight_hour| {
            let client = client.clone();
            let fly_date_time = forecast_issued + TimeDelta::hours(flight_hour.into());
            let pgrb2f_url = generate_pgrb2f_url(fly_date_time, forecast_issued, bbox);
            let pgrb2bf_url = generate_pgrb2bf_url(fly_date_time, forecast_issued, bbox);

            async move {
                let (resp1, resp2) =
                    try_join!(client.get(&pgrb2f_url).send(), client.get(&pgrb2bf_url).send())
                        .map_err(|e| AppError::HttpRequest {
                            url: format!("{pgrb2f_url} / {pgrb2bf_url}"),
                            source: e,
                        })?;

                // Guard HTTP status before reading the body: a 404/500 error page
                // must surface as HttpStatus, not get fed to the GRIB parser and
                // misreported as a grib error.
                let bytes1 = read_grib_bytes(resp1, &pgrb2f_url).await?;
                let bytes2 = read_grib_bytes(resp2, &pgrb2bf_url).await?;

                // GRIB decode + merge is CPU-bound — keep it off the reactor so
                // buffer_unordered's in-flight fetches aren't starved.
                let merged = tokio::task::spawn_blocking(move || -> Result<_, AppError> {
                    let map1 = grab_data(bytes1)?;
                    let map2 = grab_data(bytes2)?;
                    Ok(merge_grib_maps(map1, map2))
                })
                .await
                .map_err(|e| AppError::Config(format!("noaa decode panicked: {e}")))??;

                Ok::<_, AppError>((fly_date_time, merged))
            }
        })
        .buffer_unordered(CONCURRENT_REQUESTS);

    let (result, failures) = responses
        .fold((BTreeMap::new(), 0usize), |(mut map, mut fails), result| async move {
            match result {
                Ok((datetime, data)) => {
                    map.insert(datetime, data);
                }
                Err(e) => {
                    tracing::error!(error = ?e, "noaa flight hour failed");
                    fails += 1;
                }
            }
            (map, fails)
        })
        .await;

    if failures > 0 {
        return Err(AppError::PartialDownload { count: failures });
    }
    Ok(WeatherMap(result))
}

/// Read a GRIB response body, failing with [`AppError::HttpStatus`] on a
/// non-success status so an error page never reaches the GRIB parser.
async fn read_grib_bytes(resp: reqwest::Response, url: &str) -> Result<Bytes, AppError> {
    if !resp.status().is_success() {
        tracing::error!(url = %url, status = %resp.status(), "noaa HTTP error");
        return Err(AppError::HttpStatus { url: url.to_string(), status: resp.status().as_u16() });
    }
    resp.bytes().await.map_err(|e| AppError::HttpRequest { url: url.to_string(), source: e })
}

type LevelMap = BTreeMap<u32, BTreeMap<OrderedFloat<f64>, BTreeMap<OrderedFloat<f64>, Weather>>>;

// Working cell: (u-wind, v-wind), each filled in as submessages are decoded —
// mirrors icon_eu's order-independent staging so U and V may arrive in any
// order within the GRIB file.
type WorkingCell = (Option<f64>, Option<f64>);
type WorkingMap =
    BTreeMap<u32, BTreeMap<OrderedFloat<f64>, BTreeMap<OrderedFloat<f64>, WorkingCell>>>;

/// Stage one wind component for `pressure` at `(lat, lon)`. Uses `or_insert`,
/// so neither component relies on the other (or on temperature) having been
/// seen first.
fn stage(working: &mut WorkingMap, pressure: u32, is_u: bool, lat: f64, lon: f64, value: f64) {
    let cell = working
        .entry(pressure)
        .or_default()
        .entry(OrderedFloat(lat))
        .or_default()
        .entry(OrderedFloat(lon))
        .or_insert((None, None));
    if is_u {
        cell.0 = Some(value);
    } else {
        cell.1 = Some(value);
    }
}

/// Collapse staged cells into a [`LevelMap`], keeping only cells where both U
/// and V arrived; cells missing a component are dropped.
fn finalize(working: WorkingMap) -> LevelMap {
    working
        .into_iter()
        .map(|(pressure, lat_map)| {
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
            (pressure, lat_map)
        })
        .collect()
}

fn merge_grib_maps(mut map1: LevelMap, map2: LevelMap) -> LevelMap {
    for (level, lat_map2) in map2 {
        let lat_map1 = map1.entry(level).or_default();
        for (lat, lon_map) in lat_map2 {
            lat_map1.entry(lat).or_insert(lon_map);
        }
    }
    map1
}

/// Fill the run/bbox placeholders shared by both GFS filter-CGI templates.
///
/// The two templates differ in endpoint, file name, and `var_*`/`lev_*` params
/// (the `pgrb2` and `pgrb2b` files cover disjoint pressure levels), so they
/// stay separate; only this placeholder substitution is common to both.
fn fill_gfs_template(
    template: &str,
    fly_start: DateTime<Utc>,
    forecast_issued_dt: DateTime<Utc>,
    bbox: BoundingBox,
) -> String {
    let forecast_length = fly_start.signed_duration_since(forecast_issued_dt).num_hours();
    template
        .replace("YYYYMMDD", forecast_issued_dt.date_naive().format("%Y%m%d").to_string().as_str())
        .replace("START_HOUR", format!("{:02}", forecast_issued_dt.hour()).as_str())
        .replace("FORECAST", format!("{:03}", forecast_length).as_str())
        .replace("TOP_LAT", bbox.top_lat.to_string().as_str())
        .replace("BOTTOM_LAT", bbox.bottom_lat.to_string().as_str())
        .replace("LEFT_LON", bbox.left_lon.to_string().as_str())
        .replace("RIGHT_LON", bbox.right_lon.to_string().as_str())
}

fn generate_pgrb2f_url(
    fly_start: DateTime<Utc>,
    forecast_issued_dt: DateTime<Utc>,
    bbox: BoundingBox,
) -> String {
    // https://noaa-gfs-bdp-pds.s3.amazonaws.com/gfs.20250721/18/atmos/gfs.t18z.pgrb2.0p25.f000
    const PGRB2F_TEMPLATE: &str = "https://nomads.ncep.noaa.gov/cgi-bin/filter_gfs_0p25.pl?dir=\
        %2Fgfs.YYYYMMDD%2FSTART_HOUR%2Fatmos&file=gfs.tSTART_HOURz.pgrb2.0p25.fFORECAST&var_UGRD=on\
        &var_VGRD=on&lev_10_m_above_ground=on&lev_1000_mb=on&lev_975_mb=on\
        &lev_950_mb=on&lev_925_mb=on&lev_900_mb=on&lev_850_mb=on&lev_800_mb=on&lev_750_mb=on&lev_700_mb=on\
        &lev_650_mb=on&lev_600_mb=on&lev_550_mb=on&lev_500_mb=on&lev_450_mb=on&lev_400_mb=on&lev_350_mb=on\
        &lev_300_mb=on&lev_250_mb=on&lev_200_mb=on&lev_150_mb=on&lev_100_mb=on&lev_70_mb=on&lev_50_mb=on\
        &subregion=&toplat=TOP_LAT&leftlon=LEFT_LON&rightlon=RIGHT_LON&bottomlat=BOTTOM_LAT";

    fill_gfs_template(PGRB2F_TEMPLATE, fly_start, forecast_issued_dt, bbox)
}

fn generate_pgrb2bf_url(
    fly_start: DateTime<Utc>,
    forecast_issued_dt: DateTime<Utc>,
    bbox: BoundingBox,
) -> String {
    const PGRB2BF_TEMPLATE: &str = "https://nomads.ncep.noaa.gov/cgi-bin/filter_gfs_0p25b.pl?dir=\
        %2Fgfs.YYYYMMDD%2FSTART_HOUR%2Fatmos&file=gfs.tSTART_HOURz.pgrb2b.0p25.fFORECAST&lev_125_mb=on&lev_175_mb=on\
        &lev_225_mb=on&lev_275_mb=on&lev_325_mb=on&lev_375_mb=on&lev_425_mb=on&lev_475_mb=on&lev_525_mb=on\
        &lev_575_mb=on&lev_625_mb=on&lev_675_mb=on&lev_725_mb=on&lev_775_mb=on&lev_825_mb=on&lev_875_mb=on\
        &subregion=&leftlon=LEFT_LON&rightlon=RIGHT_LON&toplat=TOP_LAT&bottomlat=BOTTOM_LAT";

    fill_gfs_template(PGRB2BF_TEMPLATE, fly_start, forecast_issued_dt, bbox)
}

fn grab_data(content: Bytes) -> Result<LevelMap, AppError> {
    let cursor = Cursor::new(content);
    let grib2 = grib::from_reader(cursor).map_err(|e| AppError::Grib(e.to_string()))?;

    // Stage U (param 2) and V (param 3) per (pressure, lat, lon) as they arrive;
    // `finalize` keeps only cells where both landed. No temperature submessage is
    // requested or read — the join no longer depends on it as a grid skeleton,
    // nor on the U/V submessages arriving in any particular order.
    let mut working: WorkingMap = BTreeMap::new();
    for (_, submessage) in grib2.iter() {
        let Some((pressure, param)) = grab_pressure(submessage.prod_def()) else {
            continue;
        };
        // GRIB2 momentum category: 2 = UGRD, 3 = VGRD. Skip anything else.
        let is_u = match param {
            2 => true,
            3 => false,
            _ => continue,
        };

        let latlons = submessage.latlons().map_err(|e| AppError::Grib(e.to_string()))?;
        let decoder = grib::Grib2SubmessageDecoder::from(submessage)
            .map_err(|e| AppError::Grib(e.to_string()))?;
        let values = decoder.dispatch().map_err(|e| AppError::Grib(e.to_string()))?;

        for ((lat, lon), value) in latlons.zip(values) {
            stage(&mut working, pressure, is_u, lat.into(), lon.into(), value.into());
        }
    }
    Ok(finalize(working))
}

fn grab_pressure(prod_def: &ProdDefinition) -> Option<(u32, u8)> {
    prod_def
        .fixed_surfaces()
        .filter(|surface| surface.0.surface_type == 100)
        .and_then(|surface| u32::try_from(surface.0.scaled_value).ok())
        .zip(prod_def.parameter_number())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn finalize_keeps_only_cells_with_both_components() {
        let mut working: WorkingMap = BTreeMap::new();
        // Complete cell: both U and V present.
        stage(&mut working, 50_000, true, 50.0, 30.0, 1.0);
        stage(&mut working, 50_000, false, 50.0, 30.0, 2.0);
        // Incomplete cell: only U arrived.
        stage(&mut working, 50_000, true, 51.0, 30.0, 9.0);

        let level_map = finalize(working);
        let lats = level_map.get(&50_000).expect("level 50000 should exist");
        assert_eq!(
            lats[&OrderedFloat(50.0)][&OrderedFloat(30.0)],
            Weather::new(1.0, 2.0),
            "cell with both components must survive with the right winds"
        );
        assert!(
            lats.get(&OrderedFloat(51.0)).is_none_or(|lons| lons.is_empty()),
            "cell missing its V component must not produce a Weather"
        );
    }

    #[test]
    fn staging_is_order_independent() {
        // U before V.
        let mut u_first: WorkingMap = BTreeMap::new();
        stage(&mut u_first, 85_000, true, 50.0, 30.0, 3.0);
        stage(&mut u_first, 85_000, false, 50.0, 30.0, 4.0);

        // V before U — the order the old temp-skeleton code could not handle.
        let mut v_first: WorkingMap = BTreeMap::new();
        stage(&mut v_first, 85_000, false, 50.0, 30.0, 4.0);
        stage(&mut v_first, 85_000, true, 50.0, 30.0, 3.0);

        let from_u_first = finalize(u_first);
        assert_eq!(from_u_first, finalize(v_first), "result must not depend on arrival order");
        assert_eq!(
            from_u_first[&85_000][&OrderedFloat(50.0)][&OrderedFloat(30.0)],
            Weather::new(3.0, 4.0)
        );
    }

    #[test]
    fn merge_grib_maps_merges_lats_within_shared_level() {
        use ordered_float::OrderedFloat;

        use crate::forecast::weather::Weather;

        let mut map1: LevelMap = BTreeMap::new();
        map1.entry(50_000u32)
            .or_default()
            .entry(OrderedFloat(50.0f64))
            .or_default()
            .insert(OrderedFloat(30.0f64), Weather::new(1.0, 2.0));

        let mut map2: LevelMap = BTreeMap::new();
        map2.entry(50_000u32)
            .or_default()
            .entry(OrderedFloat(51.0f64))
            .or_default()
            .insert(OrderedFloat(30.0f64), Weather::new(3.0, 4.0));

        let merged = merge_grib_maps(map1, map2);

        // In production the pgrb2 and pgrb2b files request *disjoint* pressure
        // levels, so a level (the outer key) never appears in both maps and the
        // shallow `or_insert` per-lat never collides. This synthetic case forces
        // a shared level to confirm distinct lats from both maps are preserved.
        let level = merged.get(&50_000).expect("level 50000 should exist");
        assert!(level.contains_key(&OrderedFloat(50.0)), "lat 50.0 from map1 should survive merge");
        assert!(level.contains_key(&OrderedFloat(51.0)), "lat 51.0 from map2 should survive merge");
    }

    #[test]
    fn pgrb2bf_url_has_no_malformed_separators() {
        use crate::config::BoundingBox;
        let run = Utc.with_ymd_and_hms(2026, 5, 20, 6, 0, 0).unwrap();
        let bbox =
            BoundingBox { top_lat: 58.5, bottom_lat: 46.25, left_lon: 22.5, right_lon: 50.75 };
        let url = generate_pgrb2bf_url(run, run, bbox);
        assert!(!url.contains("=onlev"), "URL has missing & separator (=onlev found): {url}");
    }

    #[test]
    fn pgrb2f_url_requests_winds_but_not_temperature() {
        use crate::config::BoundingBox;
        let run = Utc.with_ymd_and_hms(2026, 5, 20, 6, 0, 0).unwrap();
        let bbox =
            BoundingBox { top_lat: 58.5, bottom_lat: 46.25, left_lon: 22.5, right_lon: 50.75 };
        let url = generate_pgrb2f_url(run, run, bbox);

        assert!(url.contains("var_UGRD=on"), "U-wind must still be requested: {url}");
        assert!(url.contains("var_VGRD=on"), "V-wind must still be requested: {url}");
        // Temperature values are decoded then discarded, so we no longer fetch
        // them (the U/V join no longer needs a temperature skeleton).
        assert!(!url.contains("var_TMP"), "temperature must not be requested: {url}");
        // Dropping a var must not orphan a separator (e.g. `=onvar` / `onvar`).
        assert!(!url.contains("=onvar"), "missing & separator after a var (=onvar): {url}");
    }
}
