//! By-reference slicing of a [`SerWeatherMap`] to a pressure-floor + area +
//! time box — the client-side counterpart to the API's
//! `WeatherMap::filter_into`.
//!
//! The drone requests winds for a minimum pressure, a coordinate box, and a
//! time; a client holds the whole grid in memory and slices it to just that
//! request before sending it over MavFTP. This is the same floor-key algorithm
//! the API uses for `/api/get_winds`, ported to the wire-shaped `SerWeatherMap`
//! (whose lat/lon keys are [`SerOrderedFloat`] rather than `OrderedFloat`).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use ordered_float::OrderedFloat;

use crate::SerWeatherMap;
use crate::winds::{SerOrderedFloat, Weather};

/// Build a filtered copy by reference: keep times `>= since` (floored to the
/// largest forecast time `<= since`), pressures `>= min_pa` (floored to the
/// largest pressure level `<= min_pa`; higher pressure = closer to the ground,
/// so this keeps every level from `min_pa` down to the surface), and cells
/// within `±range` of `(lat, lon)`.
///
/// The time and pressure bounds floor to the nearest grid key so a query at
/// e.g. 11:57 still picks up the 11:00 slot, and a query for 83000 Pa still
/// picks up the 82500 Pa bracket. If no key sits at-or-below the bound, the raw
/// bound is used — the branch then either keeps everything `>= bound` or
/// collapses empty.
///
/// Each level is narrowed with a [`BTreeMap::range`] query, so pruned branches
/// are never visited and only surviving [`Weather`] cells are cloned. Empty
/// lon/lat/level/time branches are dropped as they collapse, so the result
/// never carries hollow nodes.
pub fn slice_winds(
    map: &SerWeatherMap,
    since: DateTime<Utc>,
    min_pa: u32,
    lat: f64,
    lon: f64,
    range: f64,
) -> SerWeatherMap {
    let (min_lat, max_lat) = (
        SerOrderedFloat(OrderedFloat((lat - range) as f32)),
        SerOrderedFloat(OrderedFloat((lat + range) as f32)),
    );
    let (min_lon, max_lon) = (
        SerOrderedFloat(OrderedFloat((lon - range) as f32)),
        SerOrderedFloat(OrderedFloat((lon + range) as f32)),
    );
    let effective_since = floor_key(map, since).unwrap_or(since);

    let mut out = SerWeatherMap::new();
    for (&time, levels) in map.range(effective_since..) {
        let effective_pa = floor_key(levels, min_pa).unwrap_or(min_pa);
        let mut out_levels = BTreeMap::new();
        for (&pa, lats) in levels.range(effective_pa..) {
            let mut out_lats = BTreeMap::new();
            for (&la, lons) in lats.range(min_lat..=max_lat) {
                let out_lons: BTreeMap<SerOrderedFloat, Weather> =
                    lons.range(min_lon..=max_lon).map(|(&lo, w)| (lo, w.clone())).collect();
                if !out_lons.is_empty() {
                    out_lats.insert(la, out_lons);
                }
            }
            if !out_lats.is_empty() {
                out_levels.insert(pa, out_lats);
            }
        }
        if !out_levels.is_empty() {
            out.insert(time, out_levels);
        }
    }
    out
}

/// Largest key `<= bound`, if any — i.e. `bound`'s lower bracket in `map`. Used
/// to floor a `since`/`min_pa` query down to the grid key at or below it.
fn floor_key<K: Ord + Copy, V>(map: &BTreeMap<K, V>, bound: K) -> Option<K> {
    map.range(..=bound).next_back().map(|(&k, _)| k)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn sof(x: f64) -> SerOrderedFloat {
        SerOrderedFloat(OrderedFloat(x as f32))
    }

    fn dt(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 25, hour, 0, 0).unwrap()
    }

    /// Build a small grid: two times (10:00, 11:00), two pressures (85000,
    /// 50000), two lats (50.0, 52.0), two lons (30.0, 32.0); every cell
    /// present.
    fn fixture() -> SerWeatherMap {
        let mut map = SerWeatherMap::new();
        for hour in [10u32, 11] {
            let mut levels = BTreeMap::new();
            for pa in [85_000u32, 50_000] {
                let mut lats = BTreeMap::new();
                for lat in [50.0f64, 52.0] {
                    let mut lons = BTreeMap::new();
                    for lon in [30.0f64, 32.0] {
                        lons.insert(sof(lon), Weather::new(1.0, 2.0));
                    }
                    lats.insert(sof(lat), lons);
                }
                levels.insert(pa, lats);
            }
            map.insert(dt(hour), levels);
        }
        map
    }

    /// Permissive sentinels to isolate a single dimension.
    const ALL_TIMES: u32 = 9; // hour <= earliest fixture slot (10:00)
    const ALL_PA: u32 = 0;
    const WIDE: f64 = 100.0;

    #[test]
    fn since_drops_past_keeps_boundary() {
        // Boundary at 11:00 is kept (>=); 10:00 is dropped.
        let filtered = slice_winds(&fixture(), dt(11), ALL_PA, 51.0, 31.0, WIDE);
        let times: Vec<_> = filtered.keys().copied().collect();
        assert_eq!(times, vec![dt(11)]);
    }

    #[test]
    fn since_floors_to_previous_grid_hour() {
        // now=10:30 floors to 10:00, so the 10:00 slot is included.
        let now = dt(10) + chrono::Duration::minutes(30);
        let filtered = slice_winds(&fixture(), now, ALL_PA, 51.0, 31.0, WIDE);
        let times: Vec<_> = filtered.keys().copied().collect();
        assert_eq!(times, vec![dt(10), dt(11)]);
    }

    #[test]
    fn since_below_every_grid_time_keeps_all() {
        let filtered = slice_winds(&fixture(), dt(0), ALL_PA, 51.0, 31.0, WIDE);
        let times: Vec<_> = filtered.keys().copied().collect();
        assert_eq!(times, vec![dt(10), dt(11)]);
    }

    #[test]
    fn since_floors_eleven_fifty_seven_to_eleven_oclock() {
        // A request at 11:57 must include the 11:00 slot — floors down.
        let now = dt(11) + chrono::Duration::minutes(57);
        let filtered = slice_winds(&fixture(), now, ALL_PA, 51.0, 31.0, WIDE);
        let times: Vec<_> = filtered.keys().copied().collect();
        assert_eq!(times, vec![dt(11)]);
    }

    #[test]
    fn min_pressure_floors_to_lower_bracket() {
        // pa = 60000 floors to 50000, so both 50000 and 85000 are kept.
        let filtered = slice_winds(&fixture(), dt(ALL_TIMES), 60_000, 51.0, 31.0, WIDE);
        for levels in filtered.values() {
            assert!(levels.contains_key(&50_000));
            assert!(levels.contains_key(&85_000));
        }
        assert!(!filtered.is_empty());
    }

    #[test]
    fn min_pressure_at_exact_level_does_not_pull_lower() {
        // pa = 85000 sits on a grid line; the 50000 level below stays excluded.
        let filtered = slice_winds(&fixture(), dt(ALL_TIMES), 85_000, 51.0, 31.0, WIDE);
        for levels in filtered.values() {
            assert!(levels.contains_key(&85_000));
            assert!(!levels.contains_key(&50_000));
        }
    }

    #[test]
    fn min_pressure_above_every_level_floors_to_top() {
        // pa above every level still keeps the highest pressure (85000).
        let filtered = slice_winds(&fixture(), dt(ALL_TIMES), 200_000, 51.0, 31.0, WIDE);
        for levels in filtered.values() {
            assert_eq!(levels.keys().copied().collect::<Vec<_>>(), vec![85_000]);
        }
        assert!(!filtered.is_empty());
    }

    #[test]
    fn min_pressure_below_every_level_keeps_all() {
        let filtered = slice_winds(&fixture(), dt(ALL_TIMES), 10_000, 51.0, 31.0, WIDE);
        for levels in filtered.values() {
            assert_eq!(levels.keys().copied().collect::<Vec<_>>(), vec![50_000, 85_000]);
        }
    }

    #[test]
    fn min_pressure_eighty_three_thousand_floors_to_eighty_two_five_hundred() {
        // pressure steps at 82500 and 85000: a request for 83000 includes 82500.
        let mut levels = BTreeMap::new();
        for &pa in &[82_500u32, 85_000] {
            let mut lats = BTreeMap::new();
            let mut lons = BTreeMap::new();
            lons.insert(sof(30.0), Weather::new(1.0, 2.0));
            lats.insert(sof(50.0), lons);
            levels.insert(pa, lats);
        }
        let mut map = SerWeatherMap::new();
        map.insert(dt(10), levels);

        let filtered = slice_winds(&map, dt(10), 83_000, 50.0, 30.0, WIDE);
        let levels = filtered.get(&dt(10)).expect("time slot kept");
        assert_eq!(levels.keys().copied().collect::<Vec<_>>(), vec![82_500, 85_000]);
    }

    #[test]
    fn within_keeps_box_and_prunes_empty_branches() {
        // ±0.5 around (50.0, 30.0) keeps only that single cell.
        let filtered = slice_winds(&fixture(), dt(ALL_TIMES), ALL_PA, 50.0, 30.0, 0.5);
        for levels in filtered.values() {
            for lats in levels.values() {
                assert_eq!(lats.len(), 1);
                let lons = lats.get(&sof(50.0)).expect("lat 50.0 kept");
                assert_eq!(lons.len(), 1);
                assert!(lons.contains_key(&sof(30.0)));
            }
        }
    }

    #[test]
    fn within_out_of_range_yields_empty() {
        let filtered = slice_winds(&fixture(), dt(ALL_TIMES), ALL_PA, 0.0, 0.0, 1.0);
        assert!(filtered.is_empty());
    }

    #[test]
    fn within_prunes_lat_whose_lons_all_fall_outside_box() {
        // Lats are within ±1.5 of 51.0, but the lon box (40.0 ± 1.0) admits
        // neither 30.0 nor 32.0 → every branch prunes empty.
        let filtered = slice_winds(&fixture(), dt(ALL_TIMES), ALL_PA, 51.0, 40.0, 1.5);
        assert!(filtered.is_empty(), "no hollow lat/level/time branches survive");
    }

    #[test]
    fn filters_chain() {
        // now=10:30 floors to 10:00; pa=60000 floors to 50000; box keeps one cell.
        let now = dt(10) + chrono::Duration::minutes(30);
        let filtered = slice_winds(&fixture(), now, 60_000, 50.0, 30.0, 0.5);

        assert_eq!(filtered.keys().copied().collect::<Vec<_>>(), vec![dt(10), dt(11)]);
        let levels = &filtered[&dt(11)];
        assert_eq!(levels.keys().copied().collect::<Vec<_>>(), vec![50_000, 85_000]);
        let lats = &levels[&85_000];
        assert_eq!(lats.len(), 1);
        assert_eq!(lats[&sof(50.0)].len(), 1);
    }

    #[test]
    fn slice_borrows_leaving_source_intact() {
        let source = fixture();
        let filtered = slice_winds(&source, dt(11), 60_000, 50.0, 30.0, 0.5);
        assert_eq!(source.len(), 2, "source keeps both original time slots");
        assert_eq!(source[&dt(10)].len(), 2, "source keeps both pressure levels");
        assert!(!filtered.is_empty());
    }
}
