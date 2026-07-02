//! Bridge between the API's domain [`WeatherMap`] and the shared bincode codec.
//!
//! The bincode work itself lives in [`weathergrid::codec`] — the single
//! encode/decode every consumer (API, convertor, clients) uses. The functions
//! here only translate between that wire grid ([`SerWeatherMap`]) and the
//! domain [`WeatherMap`], mapping the shared [`codec::CodecError`] to
//! [`AppError`].

use chrono::{DateTime, Utc};
use ordered_float::OrderedFloat;
use weathergrid::{SerOrderedFloat, SerWeatherMap, codec};

use super::WeatherMap;
use crate::errors::AppError;

/// Decode the on-disk run format into a domain [`WeatherMap`].
pub fn from_binary(raw_bytes: &[u8]) -> Result<WeatherMap, AppError> {
    Ok(unwrap_weather_map(codec::decode_winds(raw_bytes)?))
}

/// Encode a [`WeatherMap`] in the on-disk run format.
pub fn to_binary(map: &WeatherMap) -> Result<Vec<u8>, AppError> {
    Ok(codec::encode_winds(&wrap_weather_map(map))?)
}

/// Encode a run timestamp together with its full [`WeatherMap`] as the
/// `/api/winds` envelope. The timestamp travels *with* the data so a client
/// can't pair the wrong run with the wrong grid.
pub fn to_binary_with_run(run: DateTime<Utc>, map: &WeatherMap) -> Result<Vec<u8>, AppError> {
    Ok(codec::encode_winds_with_run(run, &wrap_weather_map(map))?)
}

/// Inverse of [`to_binary_with_run`]: recover the run timestamp and the decoded
/// [`WeatherMap`] from one envelope payload.
pub fn from_binary_with_run(raw_bytes: &[u8]) -> Result<(DateTime<Utc>, WeatherMap), AppError> {
    let (run, ser_map) = codec::decode_winds_with_run(raw_bytes)?;
    Ok((run, unwrap_weather_map(ser_map)))
}

fn unwrap_weather_map(wrapped: SerWeatherMap) -> WeatherMap {
    let inner = wrapped
        .into_iter()
        .map(|(dt, level_map)| {
            let level_map = level_map
                .into_iter()
                .map(|(level, lat_map)| {
                    let lat_map = lat_map
                        .into_iter()
                        .map(|(lat, lon_map)| {
                            // Wire keys are f32; the API domain map keys are f64.
                            // Upcast each key back into the f64 domain (exact).
                            let lon_map = lon_map
                                .into_iter()
                                .map(|(lon, weather)| {
                                    (OrderedFloat(f64::from(lon.0.into_inner())), weather)
                                })
                                .collect();
                            (OrderedFloat(f64::from(lat.0.into_inner())), lon_map)
                        })
                        .collect();
                    (level, lat_map)
                })
                .collect();
            (dt, level_map)
        })
        .collect();
    WeatherMap(inner)
}

fn wrap_weather_map(map: &WeatherMap) -> SerWeatherMap {
    map.0
        .iter()
        .map(|(dt, level_map)| {
            let level_map = level_map
                .iter()
                .map(|(level, lat_map)| {
                    let lat_map = lat_map
                        .iter()
                        .map(|(lat, lon_map)| {
                            // Domain keys are f64; quantize to the f32 wire keys.
                            let lon_map = lon_map
                                .iter()
                                .map(|(lon, weather)| {
                                    let key =
                                        SerOrderedFloat(OrderedFloat(lon.into_inner() as f32));
                                    (key, weather.clone())
                                })
                                .collect();
                            (SerOrderedFloat(OrderedFloat(lat.into_inner() as f32)), lon_map)
                        })
                        .collect();
                    (*level, lat_map)
                })
                .collect();
            (*dt, level_map)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ordered_float::OrderedFloat;

    use super::*;
    use crate::forecast::weather::Weather;

    #[test]
    fn from_binary_rejects_foreign_input_without_aborting() {
        // The 404 body text — what a user might accidentally feed the convertor.
        // Its leading bytes decode to an absurd length prefix; the size limit
        // must turn that into an `Err`, not a multi-exabyte allocation abort.
        let garbage = b"no winds match the requested filters";
        assert!(from_binary(garbage).is_err());
    }

    #[test]
    fn from_binary_round_trips_to_binary() {
        let empty = WeatherMap::default();
        let bytes = to_binary(&empty).unwrap();
        let decoded = from_binary(&bytes).unwrap();
        assert!(decoded.is_empty());
    }

    /// A one-cell grid keyed time → pressure → lat → lon, for envelope tests.
    fn one_cell_map(dt: DateTime<Utc>) -> WeatherMap {
        // Use f32-exact grid coords (multiples of 0.25/0.125, as real grids are)
        // so the lat/lon keys survive the f64→f32→f64 wire round-trip exactly.
        let mut lon = BTreeMap::new();
        lon.insert(OrderedFloat(30.5_f64), Weather::new(3.4, -1.2));
        let mut lat = BTreeMap::new();
        lat.insert(OrderedFloat(50.25_f64), lon);
        let mut level = BTreeMap::new();
        level.insert(85_000_u32, lat);
        let mut top = BTreeMap::new();
        top.insert(dt, level);
        WeatherMap(top)
    }

    #[test]
    fn with_run_round_trips_timestamp_and_cells() {
        use chrono::TimeZone;

        // Both halves of the envelope must survive the round-trip: the run
        // timestamp the client uses to label the data, and the wind cell.
        let run = Utc.with_ymd_and_hms(2026, 5, 25, 6, 0, 0).unwrap();
        let map = one_cell_map(run);

        let bytes = to_binary_with_run(run, &map).unwrap();
        let (decoded_run, decoded_map) = from_binary_with_run(&bytes).unwrap();

        assert_eq!(decoded_run, run, "run timestamp must survive");
        let cell = decoded_map
            .0
            .get(&run)
            .and_then(|lvl| lvl.get(&85_000))
            .and_then(|lat| lat.get(&OrderedFloat(50.25_f64)))
            .and_then(|lon| lon.get(&OrderedFloat(30.5_f64)))
            .expect("the one cell must survive");
        assert_eq!(cell.u_wind, 3.4_f32);
        assert_eq!(cell.v_wind, -1.2_f32);
    }

    #[test]
    fn from_binary_with_run_rejects_foreign_input() {
        // Garbage bytes must surface as an Err (mapped to 500 upstream), never
        // a panic or an absurd allocation from a bogus length prefix.
        let garbage = b"not a valid winds envelope";
        assert!(from_binary_with_run(garbage).is_err());
    }
}
