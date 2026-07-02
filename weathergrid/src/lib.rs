//! Shared wire-contract types for the winds API.
//!
//! These response DTOs are depended on by both the `winds` API crate (server
//! side, which `Serialize`s them) and the Tauri app's Rust backend (client
//! side, which `Deserialize`s them). Because both ends compile against the same
//! definitions, the HTTP contract can't drift: changing a field here forces
//! both the server and every client to be updated before they build.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod atmosphere;
pub mod codec;
pub mod grid;
pub(crate) mod interpolation;
pub mod provider;
pub mod slice;
pub mod winds;
pub use atmosphere::pressure_at_altitude;
pub use provider::Provider;
pub use slice::slice_winds;
pub use winds::{SerOrderedFloat, SerWeatherMap, Weather};

pub const EARTH_RADIUS: f64 = 6371009.; // Earth's radius in meters

/// JSON body for `/api/get_weather`: the interpolated wind components plus the
/// derived speed and navigational bearing, so clients needn't recompute them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeatherResponse {
    /// Eastward wind component (m/s).
    pub u_wind: f64,
    /// Northward wind component (m/s).
    pub v_wind: f64,
    /// Wind speed (m/s).
    pub speed: f64,
    /// Direction the wind blows *towards*, nav-style: 0°=N, 90°=E.
    pub bearing_deg: f64,
}

/// Derive the `/api/get_weather` JSON body from a [`Weather`] cell, computing
/// the speed and nav-style bearing. Lives here (rather than in the API) because
/// both [`Weather`] and [`WeatherResponse`] are defined in this crate — the
/// orphan rule requires the impl to be local to one of them.
impl From<&Weather> for WeatherResponse {
    fn from(w: &Weather) -> Self {
        Self {
            u_wind: f64::from(w.u_wind),
            v_wind: f64::from(w.v_wind),
            speed: w.speed(),
            bearing_deg: w.nav_style_bearing().to_degrees(),
        }
    }
}

/// One entry in the `/api/info` response: the run timestamp of the provider's
/// currently-served forecast, plus the forecast instants and pressure levels it
/// covers. The full response is a JSON object keyed by provider slug
/// (`"icon_eu"`, `"noaa"`); a provider with no run available yet appears as
/// `null`.
///
/// The in-memory cache is the single source of truth for what is "current", so
/// everything here is derived from the exact run that `/api/get_winds` and
/// `/api/get_weather` will serve at this moment. `times`/`pressures` come from
/// the cached grid's keys — no decode — so a client can render its scrubber and
/// drive a per-time download *before* fetching any wind cells.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunInfo {
    /// UTC timestamp of the forecast run currently being served.
    pub run: DateTime<Utc>,
    /// Forecast instants available in the run (ascending).
    pub times: Vec<DateTime<Utc>>,
    /// Pressure levels (Pa) present across the run (ascending).
    pub pressures: Vec<u32>,
}
