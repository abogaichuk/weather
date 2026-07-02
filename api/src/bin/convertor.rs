//! Dev tool: convert a bincode weather file into pretty JSON for inspection.
//!
//! Reads the file at `<path_to_a_file>`, inflates it (runs are zstd-compressed
//! on disk), decodes it with the shared [`weathergrid::codec`] (the same
//! decoder the API and clients use) into a [`SerWeatherMap`], and writes a
//! human-readable JSON rendering to `<path_to_a_file>.json`.
//!
//! Run with: `cargo run --bin convertor -- <path_to_a_file>`

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;
use weathergrid::SerWeatherMap;
use weathergrid::codec::decode_winds;

/// One grid cell, flattened so lat/lon stay numeric. JSON object keys can't be
/// floats, so we render the lat/lon levels as an array of records rather than
/// nested float-keyed maps.
#[derive(Serialize)]
struct Cell {
    lat: f64,
    lon: f64,
    u_wind: f64,
    v_wind: f64,
}

/// time (RFC 3339) → pressure (Pa) → cells, in ascending grid order.
type JsonMap = BTreeMap<String, BTreeMap<u32, Vec<Cell>>>;

fn to_records(map: &SerWeatherMap) -> JsonMap {
    map.iter()
        .map(|(dt, levels)| {
            let levels = levels
                .iter()
                .map(|(pa, lats)| {
                    let cells = lats
                        .iter()
                        .flat_map(|(lat, lons)| {
                            lons.iter().map(move |(lon, w)| Cell {
                                // SerOrderedFloat(OrderedFloat(f32)) → f64 for JSON
                                lat: f64::from(lat.0.0),
                                lon: f64::from(lon.0.0),
                                u_wind: f64::from(w.u_wind),
                                v_wind: f64::from(w.v_wind),
                            })
                        })
                        .collect();
                    (*pa, cells)
                })
                .collect();
            (dt.to_rfc3339(), levels)
        })
        .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: convertor <path_to_a_file>  (writes <path_to_a_file>.json)")?;

    let in_path = PathBuf::from(&path);
    let out_path = PathBuf::from(format!("{path}.json"));

    let stored = std::fs::read(&in_path)
        .map_err(|e| format!("failed to read {}: {e}", in_path.display()))?;
    // Runs are zstd-compressed on disk; inflate before handing to the codec.
    let bytes = weather_api::forecast::compression::decompress(&stored)?;
    let map = decode_winds(&bytes)?;

    let json = serde_json::to_string_pretty(&to_records(&map))?;

    if let Some(parent) = out_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&out_path, json)?;

    println!("converted {} ({} time slots) → {}", in_path.display(), map.len(), out_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use ordered_float::OrderedFloat;
    use weathergrid::{SerOrderedFloat, Weather};

    use super::*;

    #[test]
    fn records_render_to_json_without_float_key_errors() {
        // f32-exact values (grid coords are multiples of 0.25/0.125) so the
        // f32→f64 upcast for JSON renders cleanly, not as 50.400001525878906.
        let mut lons = BTreeMap::new();
        lons.insert(SerOrderedFloat(OrderedFloat(30.5)), Weather::new(3.5, -4.25));
        let mut lats = BTreeMap::new();
        lats.insert(SerOrderedFloat(OrderedFloat(50.25)), lons);
        let mut levels = BTreeMap::new();
        levels.insert(85_000u32, lats);
        let mut map: SerWeatherMap = BTreeMap::new();
        map.insert(Utc.with_ymd_and_hms(2026, 5, 25, 13, 0, 0).unwrap(), levels);

        let json = serde_json::to_string_pretty(&to_records(&map)).unwrap();
        assert!(json.contains("\"lat\": 50.25"), "{json}");
        assert!(json.contains("\"u_wind\": 3.5"), "{json}");
        assert!(json.contains("2026-05-25T13:00:00+00:00"), "{json}");
    }
}
