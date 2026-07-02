use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use ordered_float::OrderedFloat;

use crate::forecast::weather::Weather;

pub mod compression;
pub mod deserializer;
pub mod weather;

/// Innermost map: longitude → wind cell.
pub type LonMap = BTreeMap<OrderedFloat<f64>, Weather>;
/// Latitude → [`LonMap`].
pub type LatMap = BTreeMap<OrderedFloat<f64>, LonMap>;
/// Pressure level (Pa) → [`LatMap`].
pub type LevelMap = BTreeMap<u32, LatMap>;

/// A four-dimensional wind grid keyed by time → pressure → lat → lon.
///
/// A newtype (rather than a bare type alias) so query-slicing logic can hang
/// off it as [`WeatherMap::filter_into`]: a single by-reference filter that
/// walks only the surviving key ranges and clones just the cells that pass.
#[derive(Debug, Clone, Default)]
pub struct WeatherMap(pub BTreeMap<DateTime<Utc>, LevelMap>);

impl WeatherMap {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Build a filtered copy by reference: keep times `>= since` (floored to
    /// the largest forecast time `<= since`), pressures `>= min_pa` (floored
    /// to the largest pressure level `<= min_pa`; higher pressure = closer to
    /// the ground), and cells within `±range` of `(lat, lon)`.
    ///
    /// The time and pressure bounds floor to the nearest grid key so a query
    /// at e.g. 11:57 still picks up the 11:00 slot, and a query for 83000 Pa
    /// still picks up the 82500 Pa bracket. If no key sits at-or-below the
    /// bound (the whole axis is above it), the original bound is used — the
    /// branch then either keeps everything `>= bound` or collapses empty.
    ///
    /// Each level is narrowed with a [`BTreeMap::range`] query, so pruned
    /// branches are never visited and only surviving [`Weather`] cells are
    /// cloned. Empty lon/lat/level/time branches are dropped as they collapse,
    /// so the result never carries hollow nodes.
    ///
    /// This is the by-reference counterpart to a consuming `clone()`-then-prune
    /// pipeline: the cache hands out an `Arc<WeatherMap>`, and a small-bbox
    /// query against a full grid would otherwise deep-clone the entire grid
    /// only to discard ~99% of it.
    pub fn filter_into(
        &self,
        since: DateTime<Utc>,
        min_pa: u32,
        lat: f64,
        lon: f64,
        range: f64,
    ) -> WeatherMap {
        let (min_lat, max_lat) = (OrderedFloat(lat - range), OrderedFloat(lat + range));
        let (min_lon, max_lon) = (OrderedFloat(lon - range), OrderedFloat(lon + range));
        let effective_since = floor_key(&self.0, since).unwrap_or(since);

        let mut out: BTreeMap<DateTime<Utc>, LevelMap> = BTreeMap::new();
        for (&time, levels) in self.0.range(effective_since..) {
            let effective_pa = floor_key(levels, min_pa).unwrap_or(min_pa);
            let mut out_levels = LevelMap::new();
            for (&pa, lats) in levels.range(effective_pa..) {
                let mut out_lats = LatMap::new();
                for (&la, lons) in lats.range(min_lat..=max_lat) {
                    let out_lons: LonMap =
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
        WeatherMap(out)
    }
}

/// Read, inflate, and decode a run file into a [`WeatherMap`]. Runs are
/// zstd-compressed on disk (see [`compression`]). Synchronous and CPU-bound
/// (zstd inflate + bincode decode), so async callers should run it via
/// `spawn_blocking`.
pub fn load(path: &Path) -> Result<WeatherMap, crate::errors::AppError> {
    let stored = std::fs::read(path)?;
    let bytes = compression::decompress(&stored)?;
    deserializer::from_binary(&bytes)
}

impl WeatherMap {
    /// Interpolate a wind value at an arbitrary (time, pressure, lat, lon)
    /// using trilinear interpolation within a single forecast hour, or
    /// four-dimensional interpolation when the query falls between two
    /// forecast hours.
    ///
    /// Latitude and longitude are bracketed *strictly*: a coordinate outside
    /// the grid's covered box returns `None` rather than clamping to an edge.
    /// Time and pressure clamp to the nearest level. `None` therefore means
    /// the coordinate is off-grid, or the grid is empty (no run decoded yet).
    pub fn get_weather(
        &self,
        time: DateTime<Utc>,
        pressure: u32,
        lat: f64,
        lon: f64,
    ) -> Option<Weather> {
        weathergrid::grid::get_weather(&self.0, time, pressure, lat, lon)
    }
}

/// Largest key `<= bound`, if any — i.e. `bound`'s lower bracket in `map`.
/// Used by [`WeatherMap::filter_into`] to floor a `since`/`min_pa` query to
/// the grid key that sits at or below it.
fn floor_key<K: Ord + Copy, V>(map: &BTreeMap<K, V>, bound: K) -> Option<K> {
    map.range(..=bound).next_back().map(|(&k, _)| k)
}

/// Emit the run's time slots and total cell count at debug level. Replaces the
/// old `println!`-based dump so weather-map sizing respects `RUST_LOG` like the
/// rest of the service.
pub fn log_total(data: &WeatherMap) {
    let total: usize = data
        .0
        .values()
        .flat_map(|levels| levels.values())
        .flat_map(|lats| lats.values())
        .map(|lons| lons.len())
        .sum();
    let time_slots: Vec<_> = data.0.keys().collect();
    tracing::debug!(?time_slots, total_cells = total, "weather map built");
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[macro_export]
    macro_rules! set_snapshot_suffix {
        ($($expr:expr),*) => {
            let mut settings = insta::Settings::clone_current();
            settings.set_snapshot_suffix(format!($($expr,)*));
            let _guard = settings.bind_to_scope();
        }
    }

    fn dt(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 25, hour, 0, 0).unwrap()
    }

    /// Build a small grid: two times (10:00, 11:00), two pressures (85000,
    /// 50000), two lats (50.0, 52.0), two lons (30.0, 32.0); every cell
    /// present.
    fn fixture() -> WeatherMap {
        let mut map = BTreeMap::new();
        for hour in [10u32, 11] {
            let mut levels = LevelMap::new();
            for pa in [85_000u32, 50_000] {
                let mut lats = LatMap::new();
                for lat in [50.0f64, 52.0] {
                    let mut lons = LonMap::new();
                    for lon in [30.0f64, 32.0] {
                        lons.insert(OrderedFloat(lon), Weather::new(1.0, 2.0));
                    }
                    lats.insert(OrderedFloat(lat), lons);
                }
                levels.insert(pa, lats);
            }
            map.insert(dt(hour), levels);
        }
        WeatherMap(map)
    }

    /// Permissive sentinels to isolate a single `filter_into` dimension: a
    /// `since` at/below the earliest slot, `pa = 0`, and a box wide enough to
    /// admit every lat/lon in the fixture.
    const ALL_TIMES: u32 = 9; // hour <= earliest fixture slot (10:00)
    const ALL_PA: u32 = 0;
    const WIDE: f64 = 100.0;

    #[test]
    fn since_drops_past_keeps_boundary() {
        // Boundary at 11:00 is kept (>=); 10:00 is dropped.
        let filtered = fixture().filter_into(dt(11), ALL_PA, 51.0, 31.0, WIDE);
        let times: Vec<_> = filtered.0.keys().copied().collect();
        assert_eq!(times, vec![dt(11)]);
    }

    #[test]
    fn min_pressure_floors_to_lower_bracket() {
        // pa = 60000 floors to 50000 (largest level <= 60000), so 50000 is
        // included alongside 85000 — caller can interpolate across the bracket.
        let filtered = fixture().filter_into(dt(ALL_TIMES), 60_000, 51.0, 31.0, WIDE);
        for levels in filtered.0.values() {
            assert!(levels.contains_key(&50_000));
            assert!(levels.contains_key(&85_000));
        }
        assert!(!filtered.is_empty());
    }

    #[test]
    fn min_pressure_at_exact_level_does_not_pull_lower() {
        // pa = 85000 sits on a grid line; floor returns 85000 itself, so the
        // 50000 level below stays excluded.
        let filtered = fixture().filter_into(dt(ALL_TIMES), 85_000, 51.0, 31.0, WIDE);
        for levels in filtered.0.values() {
            assert!(levels.contains_key(&85_000));
            assert!(!levels.contains_key(&50_000));
        }
    }

    #[test]
    fn min_pressure_above_every_level_floors_to_top() {
        // pa above every level still keeps the highest pressure (85000): floor
        // catches it. Under the old strict semantics this would have emptied
        // the whole map; with floor semantics any non-empty grid keeps at
        // least one level.
        let filtered = fixture().filter_into(dt(ALL_TIMES), 200_000, 51.0, 31.0, WIDE);
        for levels in filtered.0.values() {
            assert_eq!(levels.keys().copied().collect::<Vec<_>>(), vec![85_000]);
        }
        assert!(!filtered.is_empty());
    }

    #[test]
    fn min_pressure_below_every_level_keeps_all() {
        // pa below every level has no floor key; range(..) falls back to the
        // raw bound, which is below 50000, so both levels survive.
        let filtered = fixture().filter_into(dt(ALL_TIMES), 10_000, 51.0, 31.0, WIDE);
        for levels in filtered.0.values() {
            assert_eq!(levels.keys().copied().collect::<Vec<_>>(), vec![50_000, 85_000]);
        }
    }

    #[test]
    fn within_keeps_box_and_prunes_empty_branches() {
        // ±0.5 around (50.0, 30.0) keeps only that single cell.
        let filtered = fixture().filter_into(dt(ALL_TIMES), ALL_PA, 50.0, 30.0, 0.5);
        for levels in filtered.0.values() {
            for lats in levels.values() {
                assert_eq!(lats.len(), 1);
                let lons = lats.get(&OrderedFloat(50.0)).expect("lat 50.0 kept");
                assert_eq!(lons.len(), 1);
                assert!(lons.contains_key(&OrderedFloat(30.0)));
            }
        }
    }

    #[test]
    fn within_out_of_range_yields_empty() {
        let filtered = fixture().filter_into(dt(ALL_TIMES), ALL_PA, 0.0, 0.0, 1.0);
        assert!(filtered.is_empty());
    }

    #[test]
    fn within_prunes_lat_whose_lons_all_fall_outside_box() {
        // Lats 50.0 & 52.0 are both within ±1.5 of 51.0, but the lon box
        // (40.0 ± 1.0) admits neither 30.0 nor 32.0 → every branch prunes empty.
        let filtered = fixture().filter_into(dt(ALL_TIMES), ALL_PA, 51.0, 40.0, 1.5);
        assert!(filtered.is_empty(), "no hollow lat/level/time branches survive");
    }

    #[test]
    fn since_floors_to_previous_grid_hour() {
        // now=10:30 floors to 10:00 (largest forecast time <= 10:30), so the
        // 10:00 slot is included even though the request landed mid-hour.
        let filtered =
            fixture().filter_into(dt(10) + chrono::Duration::minutes(30), ALL_PA, 51.0, 31.0, WIDE);
        let times: Vec<_> = filtered.0.keys().copied().collect();
        assert_eq!(times, vec![dt(10), dt(11)]);
    }

    #[test]
    fn since_below_every_grid_time_keeps_all() {
        // since below every grid time has no floor key; range(..) falls back
        // to the raw bound, which is below 10:00, so both slots survive.
        let filtered = fixture().filter_into(dt(0), ALL_PA, 51.0, 31.0, WIDE);
        let times: Vec<_> = filtered.0.keys().copied().collect();
        assert_eq!(times, vec![dt(10), dt(11)]);
    }

    #[test]
    fn filters_chain() {
        // now=10:30 floors to 10:00 (so 10:00 is kept too); pa=60000 floors to
        // 50000 (so 50000 is kept alongside 85000); box keeps one cell per level.
        let filtered =
            fixture().filter_into(dt(10) + chrono::Duration::minutes(30), 60_000, 50.0, 30.0, 0.5);

        assert_eq!(filtered.0.keys().copied().collect::<Vec<_>>(), vec![dt(10), dt(11)]);
        let levels = &filtered.0[&dt(11)];
        assert_eq!(levels.keys().copied().collect::<Vec<_>>(), vec![50_000, 85_000]);
        let lats = &levels[&85_000];
        assert_eq!(lats.len(), 1);
        assert_eq!(lats[&OrderedFloat(50.0)].len(), 1);
    }

    #[test]
    fn since_floors_eleven_fifty_seven_to_eleven_oclock() {
        // Spec from the get_winds requirement: with hourly forecast slots, a
        // request made at 11:57 must include the 11:00 slot — the bound floors
        // down rather than rounding up to 12:00.
        let now = dt(11) + chrono::Duration::minutes(57);
        let filtered = fixture().filter_into(now, ALL_PA, 51.0, 31.0, WIDE);
        let times: Vec<_> = filtered.0.keys().copied().collect();
        assert_eq!(times, vec![dt(11)]);
    }

    #[test]
    fn min_pressure_eighty_three_thousand_floors_to_eighty_two_five_hundred() {
        // Spec from the get_winds requirement: with pressure steps at 82500
        // and 85000 Pa, a request for 83000 Pa must include the 82500 bracket.
        let mut levels = LevelMap::new();
        for &pa in &[82_500u32, 85_000] {
            let mut lats = LatMap::new();
            let mut lons = LonMap::new();
            lons.insert(OrderedFloat(30.0), Weather::new(1.0, 2.0));
            lats.insert(OrderedFloat(50.0), lons);
            levels.insert(pa, lats);
        }
        let mut map = BTreeMap::new();
        map.insert(dt(10), levels);
        let grid = WeatherMap(map);

        let filtered = grid.filter_into(dt(10), 83_000, 50.0, 30.0, WIDE);
        let levels = filtered.0.get(&dt(10)).expect("time slot kept");
        assert_eq!(levels.keys().copied().collect::<Vec<_>>(), vec![82_500, 85_000]);
    }

    #[test]
    fn filter_into_borrows_leaving_source_intact() {
        // The cache hands out an `Arc<WeatherMap>`; filtering must not consume
        // the shared grid. After filtering, the source is still fully intact.
        let source = fixture();
        let filtered = source.filter_into(dt(11), 60_000, 50.0, 30.0, 0.5);
        assert_eq!(source.0.len(), 2, "source keeps both original time slots");
        assert_eq!(source.0[&dt(10)].len(), 2, "source keeps both pressure levels");
        assert!(!filtered.is_empty());
    }

    #[test]
    fn get_weather_returns_exact_grid_cell() {
        // Every cell in `fixture()` is (1.0, 2.0); a query landing exactly on a
        // grid point makes every axis degenerate, so it must return that cell.
        let w = fixture().get_weather(dt(10), 85_000, 50.0, 30.0).expect("grid point exists");
        assert_eq!(w, Weather::new(1.0, 2.0));
    }

    #[test]
    fn get_weather_interpolates_along_one_axis() {
        // A grid that varies only in longitude (30.0 -> u=10, 32.0 -> u=20) at a
        // single time/pressure/lat. Querying lon=31.0 (halfway) must blend to
        // u=15.0; the other three axes are degenerate. Proves the lon corners
        // and axis are wired correctly through `get_weather`.
        let mut lons = LonMap::new();
        lons.insert(OrderedFloat(30.0), Weather::new(10.0, 0.0));
        lons.insert(OrderedFloat(32.0), Weather::new(20.0, 0.0));
        let mut lats = LatMap::new();
        lats.insert(OrderedFloat(50.0), lons);
        let mut levels = LevelMap::new();
        levels.insert(85_000, lats);
        let mut map = BTreeMap::new();
        map.insert(dt(10), levels);
        let map = WeatherMap(map);

        let w = map.get_weather(dt(10), 85_000, 50.0, 31.0).expect("within lon range");
        assert_eq!(w, Weather::new(15.0, 0.0));
    }

    #[test]
    fn get_weather_out_of_range_lat_is_none() {
        // lat 0.0 sits below every grid latitude (50.0, 52.0). Coordinates are
        // bracketed strictly: with no lower bracket the query is outside coverage,
        // so it returns None (handler -> 404) rather than clamping to the edge.
        assert!(fixture().get_weather(dt(10), 85_000, 0.0, 30.0).is_none(), "lat below grid");
        assert!(fixture().get_weather(dt(10), 85_000, 90.0, 30.0).is_none(), "lat above grid");
    }

    #[test]
    fn get_weather_out_of_range_lon_is_none() {
        // lon 0.0 / 180.0 sit outside the grid longitudes (30.0, 32.0).
        assert!(fixture().get_weather(dt(10), 85_000, 50.0, 0.0).is_none(), "lon west of grid");
        assert!(fixture().get_weather(dt(10), 85_000, 50.0, 180.0).is_none(), "lon east of grid");
    }

    #[test]
    fn get_weather_on_grid_edge_is_in_range() {
        // The far corner (52.0, 32.0) is the grid's max edge — inclusive, so it
        // resolves rather than tripping the out-of-range guard.
        let w = fixture().get_weather(dt(10), 85_000, 52.0, 32.0).expect("max edge is in range");
        assert_eq!(w, Weather::new(1.0, 2.0));
    }

    #[test]
    fn get_weather_empty_map_is_none() {
        // An empty grid has no time keys, so the query short-circuits to None
        // (e.g. no run decoded yet) — the handler maps it to 404.
        assert!(WeatherMap::default().get_weather(dt(10), 85_000, 50.0, 30.0).is_none());
    }

    #[test]
    fn run_survives_compressed_disk_round_trip() {
        // The full durable path: encode (the downloader's `to_binary`) →
        // compress → write, then `load` (read → decompress → decode). The
        // fixture's coords (whole degrees) and (1.0, 2.0) cells are f32-exact,
        // so the grid must come back bit-for-bit identical.
        let original = fixture();
        let bytes = deserializer::to_binary(&original).expect("encode");
        let packed = compression::compress(&bytes).expect("compress");
        assert!(packed.len() < bytes.len(), "the run should be smaller on disk");

        let file = tempfile::NamedTempFile::new().expect("temp run file");
        std::fs::write(file.path(), &packed).expect("write run");

        let loaded = load(file.path()).expect("load round-trips the compressed run");
        assert_eq!(loaded.0, original.0, "compressed disk round-trip must be lossless");
    }

    #[test]
    fn load_rejects_uncompressed_legacy_run() {
        // A pre-compression run is raw bincode with no zstd frame. `load` must
        // surface an `Err` (not panic) so a stale on-disk run fails loudly and
        // gets wiped on deploy rather than silently corrupting the cache.
        let raw = deserializer::to_binary(&fixture()).expect("encode");
        let file = tempfile::NamedTempFile::new().expect("temp run file");
        std::fs::write(file.path(), &raw).expect("write uncompressed run");
        assert!(load(file.path()).is_err(), "raw bincode is not a valid zstd run");
    }
}
