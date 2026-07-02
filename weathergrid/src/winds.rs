//! The binary wire representation of a winds grid, shared by the API (which
//! serializes it) and client backends (which deserialize the `/api/winds`
//! payload). Keeping these types here means the bincode contract — the wind
//! cell and the nested map of f32-keyed grid points — has a single definition
//! both ends compile against, so it can't drift.

use std::collections::BTreeMap;
use std::f64::consts::PI;
use std::fmt;

use chrono::{DateTime, Utc};
use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};

pub(crate) const P0: f64 = 101325.0; // Sea level standard pressure in Pascals
pub(crate) const R: f64 = 287.05; // Specific gas constant for dry air in J/(kg·K) == Universal gas constant(8.3144598) / Molar mass of Earth's air(0.0289644)
pub(crate) const G: f64 = 9.80665; // Standard gravity in m/s²

/// A single wind cell: the eastward/northward components at one grid point.
///
/// Components are **stored as f32** — the wire/cache size win — but every
/// derived quantity (`speed`, the `direction_*` family) **computes in f64**:
/// the `f32→f64` upcast is exact and free, so storage precision and compute
/// precision stay deliberately separate (winds in m/s have no meaningful f64
/// precision to lose at this grid resolution).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Weather {
    // pub data: f64, //Kelvin
    pub u_wind: f32, //(in m/s)
    pub v_wind: f32, //(in m/s)
}

impl fmt::Debug for Weather {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "uw: {}, vw: {}, dir: {:.2}, speed: {:.2}",
            self.u_wind,
            self.v_wind,
            self.direction_going_to(),
            self.speed()
        )
    }
}

/// Wind components must be finite. `0.0` (calm) is valid and preserved; only
/// NaN/±inf are replaced with `0.0`, logged so corrupt inputs stay visible.
fn sanitize(component: &str, value: f64) -> f64 {
    if value.is_finite() {
        value
    } else {
        tracing::warn!(%component, ?value, "non-finite wind component; defaulting to 0.0");
        0.0
    }
}

impl Weather {
    pub fn new(u_wind: f64, v_wind: f64) -> Self {
        // GRIB decoders emit finite f64 reals, and 0.0 (dead calm) is valid data.
        // Only NaN/±inf are bogus; sanitise those to 0.0 (warn via tracing), then
        // downcast to the stored f32 — the quantization point for the whole grid.
        Self {
            u_wind: sanitize("u_wind", u_wind) as f32,
            v_wind: sanitize("v_wind", v_wind) as f32,
        }
    }

    /// Calculate wind speed in m/s (computed in f64 from the f32 components).
    pub fn speed(&self) -> f64 {
        let (u, v) = (f64::from(self.u_wind), f64::from(self.v_wind));
        (u.powi(2) + v.powi(2)).sqrt()
    }

    /// Compass-style direction "coming from" (meteorological) in radians
    pub fn direction_coming_from(&self) -> f64 {
        (-f64::from(self.v_wind)).atan2(-f64::from(self.u_wind))
    }

    /// Vector-based direction "going to", math-style (0 = east, 90 = north) in
    /// radians in range π -> -π ([-180°, 180°])
    pub fn direction_going_to(&self) -> f64 {
        f64::from(self.v_wind).atan2(f64::from(self.u_wind))
    }

    /// Adjust from math-style to nav-style bearing: 0 = north, 90 = east, etc.
    pub fn nav_style_bearing(&self) -> f64 {
        let bearing_math = self.direction_going_to(); // 0 = east, π/2 = north
        (PI / 2. - bearing_math).rem_euclid(2. * PI)
    }

    // pub fn get_altitude(&self, pa: u32) -> f64 {
    //     let pa_f64 = if pa > 0 { pa as f64 } else { f64::MIN };
    //     (R * self.temp / G) * (P0 / pa_f64).ln()
    // }
}

/// Serde-friendly wrapper around an [`OrderedFloat`] map key: the grid uses
/// `OrderedFloat<f32>` lat/lon keys (so they're `Ord` for `BTreeMap`),
/// travelling the wire as plain `f32`. f32 keys are exact for this grid —
/// 0.25°/0.125° EU coords have no f32 collisions — at half the byte cost of
/// f64.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SerOrderedFloat(pub OrderedFloat<f32>);

impl Serialize for SerOrderedFloat {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_f32(self.0.into_inner())
    }
}

impl<'de> Deserialize<'de> for SerOrderedFloat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let val = f32::deserialize(deserializer)?;
        Ok(SerOrderedFloat(OrderedFloat(val)))
    }
}

impl From<OrderedFloat<f32>> for SerOrderedFloat {
    fn from(of: OrderedFloat<f32>) -> Self {
        SerOrderedFloat(of)
    }
}

impl From<SerOrderedFloat> for OrderedFloat<f32> {
    fn from(sf: SerOrderedFloat) -> Self {
        sf.0
    }
}

/// The wire shape of a full winds grid: time → pressure (Pa) → lat → lon →
/// [`Weather`]. This is the structure inside the `/api/winds` binary envelope
/// (preceded by the run `DateTime<Utc>`).
pub type SerWeatherMap = BTreeMap<
    DateTime<Utc>,
    BTreeMap<u32, BTreeMap<SerOrderedFloat, BTreeMap<SerOrderedFloat, Weather>>>,
>;

#[cfg(test)]
mod tests {
    use std::f64;

    use insta::assert_snapshot;
    use rstest::*;

    use super::*;

    /// Scope an insta snapshot-name suffix for the duration of the test, so the
    /// parameterized cases each land in their own `.snap` file.
    macro_rules! set_snapshot_suffix {
        ($($expr:expr),*) => {
            let mut settings = insta::Settings::clone_current();
            settings.set_snapshot_suffix(format!($($expr,)*));
            let _guard = settings.bind_to_scope();
        }
    }

    #[fixture]
    fn weather(#[default(0.)] u_wind: f64, #[default(0.)] v_wind: f64) -> Weather {
        Weather::new(u_wind, v_wind)
    }

    #[rstest]
    #[case::north(weather(0., 1.), "N -> 0°")]
    #[case::north_east(
        weather(f64::consts::FRAC_1_SQRT_2, f64::consts::FRAC_1_SQRT_2),
        "NE -> 45°"
    )]
    #[case::east(weather(1., 0.), "E -> 90°")]
    #[case::south_east(weather(f64::consts::FRAC_1_SQRT_2, -f64::consts::FRAC_1_SQRT_2), "SE -> 135°")]
    #[case::south(weather(0., -1.), "S -> 180°")]
    #[case::south_west(weather(-f64::consts::FRAC_1_SQRT_2, -f64::consts::FRAC_1_SQRT_2), "SW -> 225°")]
    #[case::west(weather(-1., 0.), "W -> 270°")]
    #[case::north_west(weather(-f64::consts::FRAC_1_SQRT_2, f64::consts::FRAC_1_SQRT_2), "NW -> 315°")]
    fn direction_test(#[case] weather: Weather, #[case] direction: &str) {
        // println!("bearing: {}", Weather::new(280.15, 1.37729526393496,
        // -7.452751532084774).unwrap().nav_style_bearing().to_degrees()); //bearing:
        // 189.77831567501633
        set_snapshot_suffix!("{}", direction);
        assert_snapshot!(weather.nav_style_bearing().to_degrees())
    }

    #[test]
    fn new_preserves_calm_wind_zero() {
        let w = Weather::new(0.0, 0.0);
        assert_eq!(w.u_wind, 0.0, "calm u_wind must stay 0.0");
        assert_eq!(w.v_wind, 0.0, "calm v_wind must stay 0.0");
        assert_eq!(w.speed(), 0.0, "calm wind speed must be 0.0");
    }

    #[test]
    fn new_preserves_finite_values_including_negative_and_subnormal() {
        // Smallest positive *normal* f32 — a tiny finite value that survives the
        // f64→f32 store (an f64 subnormal like f64::MIN_POSITIVE would underflow).
        let w = Weather::new(-3.5, f64::from(f32::MIN_POSITIVE));
        assert_eq!(w.u_wind, -3.5);
        assert_eq!(w.v_wind, f32::MIN_POSITIVE);
    }

    #[test]
    fn new_sanitizes_non_finite_to_zero() {
        let w = Weather::new(f64::NAN, f64::INFINITY);
        assert_eq!(w.u_wind, 0.0, "NaN must be sanitized to 0.0");
        assert_eq!(w.v_wind, 0.0, "inf must be sanitized to 0.0");
        let w = Weather::new(f64::NEG_INFINITY, 4.0);
        assert_eq!(w.u_wind, 0.0, "-inf must be sanitized to 0.0");
        assert_eq!(w.v_wind, 4.0, "finite component must be untouched");
    }

    #[rstest]
    #[case(weather(1., 1.))]
    #[case(weather(-5., -5.))]
    #[case(weather(10., 10.))]
    #[case(weather(20., -20.))]
    #[case(weather(-30., 30.))]
    #[case(weather(-30., 15.))]
    #[case(weather(-15., 30.))]
    fn speed_test(#[case] weather: Weather) {
        set_snapshot_suffix!("{}-{}", weather.u_wind, weather.v_wind);
        assert_snapshot!(weather.speed())
    }
}
