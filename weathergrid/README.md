# weathergrid

Wire-contract types for the [weather](https://github.com/abogaichuk/weather)
forecast API — use this crate to build a Rust client against a self-hosted
`weather-api` server with the exact types the server was compiled with.

The core model is a 4-dimensional wind grid: **time × pressure level (Pa) ×
latitude × longitude → wind** (u/v components in m/s).

```sh
cargo add weathergrid
```

## What's inside

- `SerWeatherMap` — the serializable wind grid (the `/api/get_winds` payload)
- `Weather` — one grid cell: u/v components, `speed()`, bearing helpers
- `codec` — the bincode encode/decode used on the wire and on disk
  (`decode_winds_with_run` for API responses)
- `grid::get_weather` — point lookup with interpolation between grid nodes
  (the same math the server's `/api/get_weather` uses)
- `slice_winds` — cut a pressure/area/time box out of a grid
- `Provider` — forecast source identifier (`icon_eu`, `noaa`, `ecmwf`).
  Deliberately exhaustive: a new provider is a compile-breaking change, so
  server and clients must agree before they build.
- `RunInfo` — forecast-run metadata (the `/api/info` payload)

## Versioning

`0.x`: minor releases may change the wire format. Pin the version your server
was built with.

## License

MIT OR Apache-2.0, at your option.
