# weather

Self-hostable weather-forecast service and its typed wire contract.

| Crate | Kind | What it is |
|---|---|---|
| [`weathergrid`](weathergrid/) | library | Wire-contract types: gridded wind data (time × pressure level × lat × lon), point interpolation, grid slicing, and the bincode codec. Published on [crates.io](https://crates.io/crates/weathergrid). |
| [`weather-api`](api/) | binary | axum HTTP server: downloads GRIB2 forecasts from multiple providers (ICON-EU, NOAA GFS, ECMWF Open Data), caches the latest run, serves regional slices and point queries. |

## Build

Requires `cmake` and a C compiler (GRIB2 CCSDS/JPEG2000 decoding). The Rust
toolchain is pinned by `rust-toolchain.toml`.

```sh
cargo build --release -p weather-api
```

## Run

```sh
cp .env.example .env   # set at least API_KEYS
cargo run -p weather-api
```

See [`api/README.md`](api/README.md) for endpoints, authentication, and
deployment notes (TLS, rate limiting).

## Use the types crate

```sh
cargo add weathergrid
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
