//! The `Weather` wind cell now lives in the shared `weathergrid` crate so the
//! API and client backends compile against one definition of the wire
//! contract. It is re-exported here to keep the established
//! `crate::forecast::weather::Weather` import path stable across the API.
//!
//! Its unit tests (direction/speed snapshots, sanitisation) live with the type
//! in `weathergrid` (`weathergrid/src/winds.rs`).

pub use weathergrid::Weather;
