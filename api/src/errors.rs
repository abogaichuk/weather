use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("http request error for {url}: {source}")]
    HttpRequest {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("http {status} for {url}")]
    HttpStatus { url: String, status: u16 },

    #[error("bz2 decompress error for {url}: {source}")]
    Decompress {
        url: String,
        #[source]
        source: std::io::Error,
    },

    #[error("grib error: {0}")]
    Grib(String),

    #[error("{count} fetch(es) failed — discarding partial result")]
    PartialDownload { count: usize },

    #[error("{0} download produced an empty weather map")]
    EmptyResult(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("codec error: {0}")]
    Codec(String),

    #[error("scheduler error: {0}")]
    Scheduler(#[from] tokio_cron_scheduler::JobSchedulerError),
}

impl From<weathergrid::codec::CodecError> for AppError {
    fn from(e: weathergrid::codec::CodecError) -> Self {
        AppError::Codec(e.to_string())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match &self {
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()).into_response(),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()).into_response(),
            other => {
                tracing::error!(error = %other, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal server error").into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_not_found_status() {
        let err = AppError::NotFound("building-x".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_bad_request_status() {
        let err = AppError::BadRequest("range must be positive".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_io_error_returns_500() {
        let err = AppError::Io(std::io::Error::other("disk fail"));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
