//! zstd compression for the on-disk run format — the single place that defines
//! how a serialized run is squeezed before it hits disk and inflated on the way
//! back. It sits *outside* the bincode codec on purpose: that codec
//! ([`weathergrid::codec`]) also produces the HTTP wire payload, which is
//! already zstd-compressed transparently by the `tower-http`
//! `CompressionLayer`. Compressing inside the codec would double-compress the
//! wire and break the clients' wire contract, so compression lives here, at the
//! disk byte-boundary, and is applied only by the disk read/write paths.

use crate::errors::AppError;

/// zstd level for on-disk runs. Level 3 is the zstd default: low CPU on the Pi
/// host, while still capturing most of the ~3-6x ratio the repeated f32 keys +
/// smooth wind fields allow. Saves are infrequent (one per scheduler tick) and
/// zstd decompress speed is roughly level-independent, so a higher level would
/// only trade Pi CPU for marginal disk savings.
const ZSTD_LEVEL: i32 = 3;

/// Compress raw bincode run bytes for durable storage.
pub fn compress(raw: &[u8]) -> Result<Vec<u8>, AppError> {
    // `zstd::encode_all` returns `io::Error`, which `AppError` absorbs via `?`.
    Ok(zstd::encode_all(raw, ZSTD_LEVEL)?)
}

/// Inflate a stored run back to the raw bincode bytes the codec expects.
pub fn decompress(stored: &[u8]) -> Result<Vec<u8>, AppError> {
    Ok(zstd::decode_all(stored)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compress_then_decompress_round_trips() {
        // Repetitive, compressible bytes — like the real grid's repeated keys.
        let raw: Vec<u8> = (0..10_000u32).map(|n| (n % 7) as u8).collect();
        let packed = compress(&raw).unwrap();
        let unpacked = decompress(&packed).unwrap();
        assert_eq!(unpacked, raw, "round-trip must reproduce the input exactly");
        assert!(packed.len() < raw.len(), "compressible input should shrink");
    }

    #[test]
    fn decompress_rejects_non_zstd_bytes() {
        // Raw (uncompressed) bincode or any garbage lacks the zstd magic number,
        // so decompression must surface an `Err`, never a panic. This is exactly
        // what a stale pre-compression run on disk would look like.
        let garbage = b"this is not a zstd frame";
        assert!(decompress(garbage).is_err());
    }
}
