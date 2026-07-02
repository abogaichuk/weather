//! International Standard Atmosphere altitude→pressure conversion.

use crate::winds::{G, P0, R};

const T0: f64 = 288.15; // K, sea-level standard temperature
const L0: f64 = 0.0065; // K/m, tropospheric lapse rate (0–11 km)
const P11: f64 = 22_632.06; // Pa, ISA pressure at 11 km
const T11: f64 = 216.65; // K, isothermal layer temp (11–20 km); also the base temp at 20 km
const P20: f64 = 5_474.89; // Pa, ISA pressure at 20 km
const L20: f64 = 0.001; // K/m, lapse rate (20–32 km, temperature rises)

/// Pressure (Pa) at a geometric altitude (m), piecewise ISA up to 32 km.
/// Approximate — intended for selecting/interpolating the wind grid's pressure
/// level, not for precise altimetry.
pub fn pressure_at_altitude(altitude_m: f64) -> f64 {
    if altitude_m <= 11_000.0 {
        P0 * (1.0 - L0 * altitude_m / T0).powf(G / (R * L0))
    } else if altitude_m <= 20_000.0 {
        P11 * (-G * (altitude_m - 11_000.0) / (R * T11)).exp()
    } else {
        P20 * (T11 / (T11 + L20 * (altitude_m - 20_000.0))).powf(G / (R * L20))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sea_level_is_p0() {
        assert!((pressure_at_altitude(0.0) - P0).abs() < 1e-6);
    }

    #[test]
    fn eleven_km_matches_isa_anchor() {
        // ISA anchor at 11 km is 22632.06 Pa; our formula lands within ~1 Pa,
        // so a tight tolerance still guards against a wrong exponent/constant.
        assert!((pressure_at_altitude(11_000.0) - 22_632.06).abs() < 2.0);
    }

    #[test]
    fn pressure_decreases_with_altitude() {
        let samples = [0.0, 5_000.0, 11_000.0, 15_000.0, 25_000.0, 31_000.0];
        for w in samples.windows(2) {
            assert!(
                pressure_at_altitude(w[0]) > pressure_at_altitude(w[1]),
                "pressure must fall from {} m to {} m",
                w[0],
                w[1]
            );
        }
    }
}
