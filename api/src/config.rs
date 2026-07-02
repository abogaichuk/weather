use std::path::PathBuf;

use crate::errors::AppError;

#[derive(Debug, Clone, Copy)]
pub struct BoundingBox {
    pub top_lat: f64,
    pub bottom_lat: f64,
    pub left_lon: f64,
    pub right_lon: f64,
}

// Intentionally NOT `#[derive(Debug)]`: `api_keys` holds secret bearer tokens,
// and a derived `Debug` would print them in any `{:?}`/panic output.
pub struct Config {
    pub weather_dir: PathBuf,
    /// Accepted bearer tokens (parsed from the semicolon-separated `API_KEYS`).
    pub api_keys: Vec<String>,
    pub bbox: BoundingBox,
    /// Forecast window (hours from issue) to download for the ECMWF provider.
    /// ICON-EU/GFS are capped by a fixed in-code window; ECMWF (AIFS) is global
    /// and long-range, so its window is configurable to keep the Pi's storage
    /// and bandwidth in check. Defaults to a modest 24 h.
    pub ecmwf_forecast_hours: u16,
}

impl Config {
    /// Build the config from the process environment.
    ///
    /// `.env` is loaded once in `main`, *before* tracing is initialised, so
    /// `RUST_LOG` from `.env` is honoured (see `main.rs`). By the time this
    /// runs the environment is already populated, so it just delegates to
    /// [`Config::from_env_vars`] — no second `dotenv()` load.
    pub fn from_env() -> Result<Self, AppError> {
        Self::from_env_vars()
    }

    /// Build the config purely from process environment variables.
    ///
    /// Kept separate from [`Config::from_env`] so unit tests can exercise the
    /// parsing/defaulting logic deterministically without loading `.env`.
    fn from_env_vars() -> Result<Self, AppError> {
        Ok(Self {
            weather_dir: PathBuf::from(
                std::env::var("WEATHER_DIR").unwrap_or_else(|_| "./data/winds/".into()),
            ),
            api_keys: parse_api_keys(
                &std::env::var("API_KEYS")
                    .map_err(|_| AppError::Config("API_KEYS must be set".into()))?,
            )?,
            bbox: BoundingBox {
                top_lat: parse_f64_env("TOP_LAT", 58.5)?,
                bottom_lat: parse_f64_env("BOTTOM_LAT", 46.25)?,
                left_lon: parse_f64_env("LEFT_LON", 22.5)?,
                right_lon: parse_f64_env("RIGHT_LON", 50.75)?,
            },
            ecmwf_forecast_hours: parse_u16_env("ECMWF_FORECAST_HOURS", 24)?,
        })
    }
}

fn parse_u16_env(key: &str, default: u16) -> Result<u16, AppError> {
    match std::env::var(key) {
        Ok(v) => v.parse().map_err(|_| AppError::Config(format!("{key} must be a valid u16"))),
        Err(_) => Ok(default),
    }
}

fn parse_f64_env(key: &str, default: f64) -> Result<f64, AppError> {
    match std::env::var(key) {
        Ok(v) => v.parse().map_err(|_| AppError::Config(format!("{key} must be a valid float"))),
        Err(_) => Ok(default),
    }
}

/// Parse the semicolon-separated `API_KEYS` value into a list of accepted
/// bearer tokens. Whitespace around each token is trimmed and empty entries
/// are dropped. Returns an error if no non-empty token remains, so a
/// misconfigured deployment fails fast instead of silently accepting nobody.
fn parse_api_keys(raw: &str) -> Result<Vec<String>, AppError> {
    let keys: Vec<String> =
        raw.split(';').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect();

    if keys.is_empty() {
        return Err(AppError::Config("API_KEYS must contain at least one non-empty token".into()));
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_api_keys_splits_trims_and_drops_empties() {
        assert_eq!(parse_api_keys("a; b ;;c").unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_api_keys_single_token() {
        assert_eq!(parse_api_keys("only-token").unwrap(), vec!["only-token"]);
    }

    #[test]
    fn parse_api_keys_empty_is_error() {
        assert!(matches!(parse_api_keys(""), Err(AppError::Config(_))));
        assert!(matches!(parse_api_keys("  ;  ; "), Err(AppError::Config(_))));
    }
}
