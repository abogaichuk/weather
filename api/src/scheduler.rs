use std::path::PathBuf;
use std::sync::Arc;

use chrono::{Duration, Utc};
use tokio_cron_scheduler::{Job, JobScheduler};

use crate::cache::WindsCache;
use crate::config::BoundingBox;
use crate::downloader;
use crate::errors::AppError;

/// How many days of saved runs to keep on disk. Anything older is unreachable
/// by the running system (the cache holds the latest run; `warm_cache` only
/// loads the freshest path), and forecasts older than 24 h are stale data we
/// wouldn't serve anyway — retention exists purely as an inspection window
/// sized to the host's available free space.
const RETENTION_DAYS: i64 = 7;

pub async fn start_scheduler(
    weather_dir: PathBuf,
    bbox: BoundingBox,
    ecmwf_hours: u16,
    cache: WindsCache,
) -> Result<Arc<JobScheduler>, AppError> {
    let scheduler = JobScheduler::new().await?;

    // Every 15 minutes: "0 */15 * * * *"
    let download_dir = weather_dir.clone();
    let download_job = Job::new_async("0 */15 * * * *", move |_uuid, _lock| {
        let weather_dir = download_dir.clone();
        // Cheap clone: shares the one inner Arc<Mutex<..>> with the handlers, so
        // a download warms the same cache requests read from.
        let cache = cache.clone();
        Box::pin(async move {
            let now = Utc::now();
            tracing::info!("scheduled job started at: {:?}", now);

            if let Err(err) =
                downloader::download_all(now, &weather_dir, bbox, ecmwf_hours, &cache).await
            {
                tracing::error!(?err, "scheduled job failed");
            }

            tracing::info!("scheduled job finished");
        })
    })?;

    // Daily at 00:10 UTC: "0 10 0 * * *". Offset 10 minutes from the
    // download job's :00/:15/:30/:45 ticks so the two never compete on the
    // same provider directory.
    let prune_job = Job::new_async("0 10 0 * * *", move |_uuid, _lock| {
        let weather_dir = weather_dir.clone();
        Box::pin(async move {
            let cutoff = Utc::now() - Duration::days(RETENTION_DAYS);
            tracing::info!(?cutoff, "pruning runs older than cutoff");

            // Directory traversal + unlinks are blocking — off the reactor.
            match tokio::task::spawn_blocking(move || {
                downloader::prune_old_runs(&weather_dir, cutoff)
            })
            .await
            {
                Ok(deleted) => tracing::info!(deleted, "prune finished"),
                Err(err) => tracing::error!(?err, "prune task panicked"),
            }
        })
    })?;

    scheduler.add(download_job).await?;
    scheduler.add(prune_job).await?;

    scheduler.start().await?;

    Ok(Arc::new(scheduler))
}
