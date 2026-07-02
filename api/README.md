# weather-api

Weather/wind forecast API. Downloads GRIB2 data from multiple providers
(ICON-EU, NOAA GFS, ECMWF Open Data), caches the latest run in memory and on
disk, and serves it over HTTP. Request/response types live in the
[`weathergrid`](../weathergrid/) crate, so any Rust client can consume the API with
the exact wire contract the server was built against.


## Build

`cargo build --release -p weather-api`

## Endpoints

All endpoints are under `/api` and require authentication (see below).

| Endpoint            | Method | Description                                            |
| ------------------- | ------ | ------------------------------------------------------ |
| `/api/get_winds`    | GET    | Filtered winds data for a region (bincode binary).     |
| `/api/get_weather`  | GET    | Interpolated wind at a single point (JSON).            |
| `/api/info`         | GET    | Current forecast run + cache status per provider (JSON).|


## Authentication

The API accepts a **set of bearer tokens** configured via the `API_KEYS`
environment variable — a semicolon-separated list, one token per user. Every
request must send:

```
Authorization: Bearer <token>
```

Any token in the set is accepted; an unknown or missing token gets `401`. There
is **no user management** — a token is simply valid or not, with no per-token
identity. Tokens are compared in constant time.

The server fails to start if `API_KEYS` is unset or contains no non-empty token.
Generate tokens with e.g. `openssl rand -hex 32`. See `.env.example`.

## Security & Deployment

This service is designed to run as a **shared, remotely-hosted server**. A few
deployment requirements live outside the application code:

- **No CORS.** CORS is intentionally not configured: the only HTTP client is the
  desktop app's native Rust backend, which does not use CORS. A browser-based
  client is not a supported consumer.
- An optional unauthenticated `/health` endpoint for load balancers is not
  implemented yet — all routes are currently authenticated.

### TLS (mandatory)

A bearer token sent over plain HTTP is sniffable, so this service **must not** be
exposed directly. Terminate HTTPS at a reverse proxy and forward plaintext to the
app over loopback. The app keeps binding `0.0.0.0` (or `127.0.0.1`) behind the
proxy — it does not handle TLS itself.

With [Caddy](https://caddyserver.com), TLS certificates are automatic:

```caddyfile
api.example.com {
    reverse_proxy 127.0.0.1:3001
}
```

(nginx or Cloudflare in front work equally well — the only requirement is that
clients reach the API exclusively over HTTPS.)

### Rate limiting (recommended)

Now that the API is public, add rate limiting to blunt brute-force token guessing
and abuse. Two options:

- **At the reverse proxy** — simplest; e.g. Caddy's `rate_limit` or nginx
  `limit_req`. Preferred if you already run a proxy for TLS.
- **In the app** — add the [`tower_governor`](https://crates.io/crates/tower-governor)
  middleware as another layer on the router (it composes with the existing auth
  layer). Sketch:

  ```rust
  use tower_governor::{governor::GovernorConfigBuilder, GovernorLayer};

  let governor = GovernorConfigBuilder::default()
      .per_second(2)        // sustained rate
      .burst_size(10)       // allowed burst
      .finish()
      .expect("valid governor config");

  let app = Router::new()
      // ... routes ...
      .layer(GovernorLayer { config: governor.into() })
      .layer(axum::middleware::from_fn_with_state(api_keys, auth::require_api_key))
      .with_state(state);
  ```

  Not implemented yet — kept out of the initial auth change to stay minimal.

## Configuration

See `.env.example` for all environment variables (`API_KEYS`, `WEATHER_DIR`,
bounding box, `RUST_LOG`).
