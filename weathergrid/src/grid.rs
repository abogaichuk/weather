//! Key-generic 4-D wind lookup over a `time → pressure → lat → lon → Weather`
//! grid, shared by the api (OrderedFloat keys) and clients (SerOrderedFloat).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use ordered_float::OrderedFloat;

use crate::interpolation::{self, Axis};
use crate::winds::{SerOrderedFloat, Weather};

/// A lat/lon grid-axis key that can be read as / built from an `f64`.
pub trait Coord: Ord + Copy {
    fn as_f64(self) -> f64;
    fn from_f64(v: f64) -> Self;
}

impl Coord for OrderedFloat<f64> {
    fn as_f64(self) -> f64 {
        self.into_inner()
    }

    fn from_f64(v: f64) -> Self {
        OrderedFloat(v)
    }
}

impl Coord for SerOrderedFloat {
    fn as_f64(self) -> f64 {
        // Upcast the stored f32 key into the f64 query/interpolation domain.
        f64::from(self.0.into_inner())
    }

    fn from_f64(v: f64) -> Self {
        // Downcast a query coordinate to the f32 grid-key domain. Grid keys are
        // exact in f32, so a query that lands on a key matches it.
        SerOrderedFloat(OrderedFloat(v as f32))
    }
}

/// Nearest bracketing pair, clamping to a single key when `key` is outside the
/// covered range (used for time + pressure).
fn get_nearest_keys<K: Ord + Copy, V>(map: &BTreeMap<K, V>, key: K) -> Option<(K, K)> {
    let lower = map.range(..=key).next_back().map(|(k, _)| *k);
    let upper = map.range(key..).next().map(|(k, _)| *k);
    match (lower, upper) {
        (Some(lo), Some(hi)) => Some((lo, hi)),
        (Some(lo), None) => Some((lo, lo)),
        (None, Some(hi)) => Some((hi, hi)),
        (None, None) => None,
    }
}

/// Strict bracketing pair: `None` when `key` is outside the covered range
/// (used for lat/lon so off-grid queries are misses, not edge-clamps).
fn get_bracketing_keys<K: Ord + Copy, V>(map: &BTreeMap<K, V>, key: K) -> Option<(K, K)> {
    let lower = map.range(..=key).next_back().map(|(k, _)| *k)?;
    let upper = map.range(key..).next().map(|(k, _)| *k)?;
    Some((lower, upper))
}

/// 4-D interpolated wind at `(time, pressure, lat, lon)`, or `None` if the
/// coordinate is off-grid / the grid is empty.
#[allow(clippy::type_complexity)]
pub fn get_weather<C: Coord>(
    grid: &BTreeMap<DateTime<Utc>, BTreeMap<u32, BTreeMap<C, BTreeMap<C, Weather>>>>,
    time: DateTime<Utc>,
    pressure: u32,
    lat: f64,
    lon: f64,
) -> Option<Weather> {
    let (lat_key, lon_key) = (C::from_f64(lat), C::from_f64(lon));
    let (t1, t2) = get_nearest_keys(grid, time)?;
    let (p1, p2) = get_nearest_keys(grid.get(&t1)?, pressure)?;
    let (x1, x2) = get_bracketing_keys(grid.get(&t1)?.get(&p1)?, lat_key)?;
    let (y1, y2) = get_bracketing_keys(grid.get(&t1)?.get(&p1)?.get(&x1)?, lon_key)?;

    // Corner cube [pressure][lat][lon] for a given time slab. Re-brackets this
    // slab's own pressure/lat/lon keys; within a single forecast run the grid
    // geometry is identical across time, so these equal the t1 brackets used for
    // the interpolation axes below.
    let cube = |t: &DateTime<Utc>| -> Option<[[[&Weather; 2]; 2]; 2]> {
        let (pa_lo, pa_hi) = get_nearest_keys(grid.get(t)?, pressure)?;
        let (xa1, xa2) = get_bracketing_keys(grid.get(t)?.get(&pa_lo)?, lat_key)?;
        let (ya1, ya2) = get_bracketing_keys(grid.get(t)?.get(&pa_lo)?.get(&xa1)?, lon_key)?;
        Some([
            [
                [
                    grid.get(t)?.get(&pa_lo)?.get(&xa1)?.get(&ya1)?,
                    grid.get(t)?.get(&pa_lo)?.get(&xa1)?.get(&ya2)?,
                ],
                [
                    grid.get(t)?.get(&pa_lo)?.get(&xa2)?.get(&ya1)?,
                    grid.get(t)?.get(&pa_lo)?.get(&xa2)?.get(&ya2)?,
                ],
            ],
            [
                [
                    grid.get(t)?.get(&pa_hi)?.get(&xa1)?.get(&ya1)?,
                    grid.get(t)?.get(&pa_hi)?.get(&xa1)?.get(&ya2)?,
                ],
                [
                    grid.get(t)?.get(&pa_hi)?.get(&xa2)?.get(&ya1)?,
                    grid.get(t)?.get(&pa_hi)?.get(&xa2)?.get(&ya2)?,
                ],
            ],
        ])
    };

    let lat_axis = Axis::new(lat, x1.as_f64(), x2.as_f64());
    let lon_axis = Axis::new(lon, y1.as_f64(), y2.as_f64());
    // Pressure levels are identical across time within a run, so the t1 bracket
    // (p1, p2) is the right axis for every slab.
    let p_axis = Axis::new(pressure as f64, p1 as f64, p2 as f64);

    let t1_cube = cube(&t1)?;
    let interp_t1 = interpolation::trilinear(lat_axis, lon_axis, p_axis, t1_cube);
    if t1 == t2 {
        return Some(interp_t1);
    }
    let t2_cube = cube(&t2)?;
    let t_axis = Axis::new(time.timestamp() as f64, t1.timestamp() as f64, t2.timestamp() as f64);
    Some(interpolation::quadrilinear(lat_axis, lon_axis, p_axis, t_axis, [t1_cube, t2_cube]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::winds::SerWeatherMap;

    fn dt(h: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2026, 5, 25, h, 0, 0).unwrap()
    }

    // Single time, single pressure; lats 50/52, lons 30/32. Corner winds encode
    // position: u rises with latitude (0→u_apex over 50→52), v rises with
    // longitude (0→v_apex over 30→32), so a lat/lon axis swap changes the result.
    fn varied_grid(time: DateTime<Utc>, u_apex: f64, v_apex: f64) -> SerWeatherMap {
        let lat = |v: f64| SerOrderedFloat(OrderedFloat(v as f32));
        let mut lons_lo = BTreeMap::new(); // lat 50
        lons_lo.insert(lat(30.0), Weather::new(0.0, 0.0));
        lons_lo.insert(lat(32.0), Weather::new(0.0, v_apex));
        let mut lons_hi = BTreeMap::new(); // lat 52
        lons_hi.insert(lat(30.0), Weather::new(u_apex, 0.0));
        lons_hi.insert(lat(32.0), Weather::new(u_apex, v_apex));
        let mut lats = BTreeMap::new();
        lats.insert(lat(50.0), lons_lo);
        lats.insert(lat(52.0), lons_hi);
        let mut levels = BTreeMap::new();
        levels.insert(85_000u32, lats);
        let mut grid = SerWeatherMap::new();
        grid.insert(time, levels);
        grid
    }

    #[test]
    fn interpolates_lat_and_lon_within_a_cell() {
        // Query lat 51 (50% of 50→52) and lon 30.5 (25% of 30→32):
        // u = 50% * 100 = 50, v = 25% * 100 = 25.
        let g = varied_grid(dt(10), 100.0, 100.0);
        let w = get_weather(&g, dt(10), 85_000, 51.0, 30.5).expect("in-grid");
        assert!((w.u_wind - 50.0).abs() < 1e-3, "u={}", w.u_wind);
        assert!((w.v_wind - 25.0).abs() < 1e-3, "v={}", w.v_wind);
    }

    #[test]
    fn interpolates_across_time() {
        // t1=10:00 all-zero, t2=12:00 apex corner (52,32) = (20,40). Query that
        // corner at 11:00 (50% of the way): pure time blend → u=10, v=20.
        let mut g = varied_grid(dt(10), 0.0, 0.0);
        g.extend(varied_grid(dt(12), 20.0, 40.0));
        let w = get_weather(&g, dt(11), 85_000, 52.0, 32.0).expect("in-grid");
        assert!((w.u_wind - 10.0).abs() < 1e-3, "u={}", w.u_wind);
        assert!((w.v_wind - 20.0).abs() < 1e-3, "v={}", w.v_wind);
    }

    #[test]
    fn off_grid_is_none() {
        let g = varied_grid(dt(10), 100.0, 100.0);
        assert!(get_weather(&g, dt(10), 85_000, 80.0, 31.0).is_none(), "lat off grid");
    }
}
