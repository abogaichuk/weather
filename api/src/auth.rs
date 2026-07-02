use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use subtle::{Choice, ConstantTimeEq};

/// The set of accepted bearer tokens, shared as middleware state.
///
/// Cheap to clone per request (two `Arc` refcount bumps), so Axum can clone it
/// into every request without copying the tokens themselves.
#[derive(Clone)]
pub struct ApiKeys(pub Arc<[Arc<str>]>);

/// Reject any request whose `Authorization: Bearer <token>` header does not
/// carry one of the configured tokens. On any failure — missing header, wrong
/// scheme, unknown token — the response is a bare `401` with no detail, so the
/// client learns nothing about *why* it was rejected or how many tokens exist.
pub async fn require_api_key(
    State(keys): State<ApiKeys>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Decide before moving `req` into the handler so the borrow of its headers
    // is released first.
    let accepted = bearer_token(&req).is_some_and(|token| is_accepted(token, &keys));
    if accepted { Ok(next.run(req).await) } else { Err(StatusCode::UNAUTHORIZED) }
}

/// Extract the token from an `Authorization: Bearer <token>` header. Returns
/// `None` when the header is absent, non-UTF-8, not the `Bearer` scheme, or has
/// an empty token.
fn bearer_token(req: &Request) -> Option<&str> {
    let value = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("Bearer") && !token.is_empty()).then_some(token)
}

/// Constant-time membership test: compare the presented token against *every*
/// configured token without short-circuiting, so request timing reveals neither
/// which token matched nor how many tokens exist.
fn is_accepted(token: &str, keys: &ApiKeys) -> bool {
    let token = token.as_bytes();
    let mut matched = Choice::from(0u8);
    for key in keys.0.iter() {
        matched |= token.ct_eq(key.as_bytes());
    }
    matched.into()
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*; // for `oneshot`

    /// Router protected by the middleware with two valid tokens configured.
    fn app() -> Router {
        let keys = ApiKeys(vec![Arc::from("token-a"), Arc::from("token-b")].into());
        Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(keys, require_api_key))
    }

    /// Drive one request through the router with an optional `Authorization`
    /// header value and return the resulting status.
    async fn status_for(auth: Option<&str>) -> StatusCode {
        let mut builder = HttpRequest::builder().uri("/");
        if let Some(value) = auth {
            builder = builder.header(header::AUTHORIZATION, value);
        }
        let req = builder.body(Body::empty()).unwrap();
        app().oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn missing_header_is_unauthorized() {
        assert_eq!(status_for(None).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_token_is_unauthorized() {
        assert_eq!(status_for(Some("Bearer not-a-real-token")).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn non_bearer_scheme_is_unauthorized() {
        assert_eq!(status_for(Some("Basic token-a")).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn first_token_is_authorized() {
        assert_eq!(status_for(Some("Bearer token-a")).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn second_token_is_authorized() {
        assert_eq!(status_for(Some("Bearer token-b")).await, StatusCode::OK);
    }
}
