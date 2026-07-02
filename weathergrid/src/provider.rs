//! Forecast provider identifier, shared by the API server and client
//! backend so the `provider` wire value can't drift between them.
//!
//! The serde representation is the stable slug used both in the API `provider`
//! query param and in the on-disk cache filename (`icon_eu`, `noaa`, `ecmwf`).
//! Because both ends compile against this one definition, adding or renaming a
//! provider forces the server and every client to agree before they build.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A weather data source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// ICON-EU forecast (`"icon_eu"`).
    #[default]
    IconEu,
    /// NOAA GFS forecast (`"noaa"`).
    Noaa,
    /// ECMWF Open Data forecast (`"ecmwf"`) — currently the AIFS model.
    Ecmwf,
}

impl Provider {
    /// Every defined provider. Update this when adding a new variant.
    pub fn all() -> &'static [Provider] {
        &[Provider::IconEu, Provider::Noaa, Provider::Ecmwf]
    }

    /// Stable on-disk and API identifier (matches the serde rename).
    pub fn slug(&self) -> &'static str {
        match self {
            Provider::IconEu => "icon_eu",
            Provider::Noaa => "noaa",
            Provider::Ecmwf => "ecmwf",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}
