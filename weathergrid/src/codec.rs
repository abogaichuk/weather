//! The bincode wire codec for winds grids — the single encode/decode
//! implementation shared by the API (server), the `convertor` dev-tool, and the
//! client backends. It operates on [`SerWeatherMap`] (the wire shape)
//! rather than any one crate's domain type, so every consumer turns bytes into
//! the same structure the same way and the binary contract can't fork.

use chrono::{DateTime, Utc};

use crate::SerWeatherMap;

/// Failure to encode or decode a winds payload.
// non_exhaustive: error variants may grow without a breaking release. `Provider`
// deliberately stays exhaustive — a new provider MUST break consumers' builds
// (see provider.rs module docs).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CodecError {
    #[error("winds codec error: {0}")]
    Bincode(String),
}

/// Encode a grid alone (no timestamp) — the on-disk run format.
pub fn encode_winds(map: &SerWeatherMap) -> Result<Vec<u8>, CodecError> {
    bincode::serde::encode_to_vec(map, bincode::config::standard())
        .map_err(|e| CodecError::Bincode(e.to_string()))
}

/// Decode the on-disk run format produced by [`encode_winds`].
pub fn decode_winds(bytes: &[u8]) -> Result<SerWeatherMap, CodecError> {
    let (map, _): (SerWeatherMap, usize) =
        bincode::serde::decode_from_slice(bytes, bincode::config::standard())
            .map_err(|e| CodecError::Bincode(e.to_string()))?;
    Ok(map)
}

/// Encode a run timestamp together with its grid as one envelope — the
/// `/api/winds` format, so the data and its `date_time` can't drift apart.
pub fn encode_winds_with_run(
    run: DateTime<Utc>,
    map: &SerWeatherMap,
) -> Result<Vec<u8>, CodecError> {
    bincode::serde::encode_to_vec((run, map), bincode::config::standard())
        .map_err(|e| CodecError::Bincode(e.to_string()))
}

/// Decode the `/api/winds` envelope produced by [`encode_winds_with_run`].
pub fn decode_winds_with_run(bytes: &[u8]) -> Result<(DateTime<Utc>, SerWeatherMap), CodecError> {
    let ((run, map), _): ((DateTime<Utc>, SerWeatherMap), usize) =
        bincode::serde::decode_from_slice(bytes, bincode::config::standard())
            .map_err(|e| CodecError::Bincode(e.to_string()))?;
    Ok((run, map))
}
