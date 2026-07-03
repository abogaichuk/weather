# weather

Self-hostable weather-forecast service and its typed wire contract.

| Crate | Kind | What it is |
|---|---|---|
| [`weathergrid`](weathergrid/) | library | Wire-contract types: gridded wind data (time × pressure level × lat × lon), point interpolation, grid slicing, and the bincode codec. Published on [crates.io](https://crates.io/crates/weathergrid). |
| [`weather-api`](api/) | binary | axum HTTP server: downloads GRIB2 forecasts from multiple providers (ICON-EU, NOAA GFS, ECMWF Open Data), caches the latest run, serves regional slices and point queries. |

## Prebuilt binaries

Each `weather-api` release publishes prebuilt Linux binaries on the
[Releases page](https://github.com/abogaichuk/weather/releases), tagged
`weather-api-vX.Y.Z`:

| Artifact | For |
|---|---|
| `…-x86_64-unknown-linux-gnu.tar.gz` | 64-bit x86 Linux servers |
| `…-aarch64-unknown-linux-gnu.tar.gz` | 64-bit ARM — **Raspberry Pi OS Bookworm (64-bit) only** |

> **Compatibility.** The binaries are built on Ubuntu 22.04, so they require
> **glibc ≥ 2.35** (Debian 12 / Ubuntu 22.04+). For a Raspberry Pi that means the
> **64-bit** edition of **Bookworm** — they will **not** run on Bullseye
> (glibc 2.31) or on any 32-bit Raspberry Pi OS. Check with `uname -m` (expect
> `aarch64`) and `ldd --version` (expect ≥ 2.35). On any older or different
> platform, [build from source](#build) instead.

Each tarball ships a matching `.sha256`; verify with `sha256sum -c`, then:

```sh
tar -xzf weather-api-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz
./weather-api-vX.Y.Z-aarch64-unknown-linux-gnu/weather-api   # needs a .env — see Run
```

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
