use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use tower_http::compression::CompressionLayer;
use weather_api::auth::{self, ApiKeys};
use weather_api::cache::WindsCache;
use weather_api::downloader::{self, Provider};
use weather_api::scheduler::start_scheduler;
use weather_api::{AppState, api, config, errors};

#[tokio::main]
async fn main() -> Result<(), errors::AppError> {
    // Load `.env` BEFORE initialising tracing: `fmt::init()` reads `RUST_LOG`
    // once, here, to build its env filter. If `.env` were loaded later (e.g.
    // only inside `Config::from_env`), the filter would already be frozen and
    // `RUST_LOG` from `.env` would be ignored, silencing all logs.
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt::init();

    let config = config::Config::from_env()?;
    tracing::info!("weather_dir: {:?}, bbox: {:?}", config.weather_dir, config.bbox);

    // One cache, shared by the scheduler (writes) and the handlers (read).
    let cache = WindsCache::new();

    // Cold-start warm: populate the cache from whatever is already on disk so
    // requests succeed before the first scheduler tick (which can be up to 15
    // minutes away). Failures are logged but non-fatal — a provider with no
    // run yet just returns 404 until the scheduler downloads one.
    for &provider in Provider::all() {
        let weather_dir = config.weather_dir.clone();
        let cache_for_warm = cache.clone();
        match tokio::task::spawn_blocking(move || {
            downloader::warm_cache(&weather_dir, provider, &cache_for_warm)
        })
        .await
        {
            Ok(Ok(true)) => tracing::info!(provider = %provider.slug(), "cache warmed from disk"),
            Ok(Ok(false)) => {
                tracing::info!(provider = %provider.slug(), "no run on disk yet, cache empty")
            }
            Ok(Err(err)) => {
                tracing::warn!(?err, provider = %provider.slug(), "cold-start warm failed")
            }
            Err(err) => {
                tracing::warn!(?err, provider = %provider.slug(), "cold-start warm task panicked")
            }
        }
    }

    start_scheduler(
        config.weather_dir.clone(),
        config.bbox,
        config.ecmwf_forecast_hours,
        cache.clone(),
    )
    .await?;

    let state = AppState::with_cache(config.weather_dir, cache);

    let api_keys = ApiKeys(config.api_keys.iter().map(|k| Arc::from(k.as_str())).collect());

    let app = Router::new()
        .route("/api/get_winds", get(api::get_winds))
        .route("/api/winds", get(api::get_all_winds))
        .route("/api/get_weather", get(api::get_weather))
        .route("/api/info", get(api::get_info))
        .layer(axum::middleware::from_fn_with_state(api_keys, auth::require_api_key))
        // Outermost: zstd-compress every response body on the way out (octet-stream
        // winds payloads + JSON). Clients that don't advertise `Accept-Encoding: zstd`
        // still get the raw bincode, so the wire contract is unchanged.
        .layer(CompressionLayer::new())
        .with_state(state);

    let addr = "0.0.0.0:3001";
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!(addr = %addr, "server listening");
    axum::serve(listener, app).await?;

    Ok(())
}
