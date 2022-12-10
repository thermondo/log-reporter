use crate::{config::Config, extractors::LogplexDrainToken, reporter::process_logs};
use axum::{
    extract::{RawBody, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router, TypedHeader,
};
use axum_extra::routing::RouterExt;
use hyper::body;
use std::sync::Arc;
use tracing::{debug, instrument, warn};

pub(crate) fn build_app(config: Arc<Config>) -> Router {
    Router::new()
        .route_with_tsr("/ht", get(health_check))
        .route("/", post(handle_logs))
        .with_state(config)
}

pub(crate) async fn health_check() -> impl IntoResponse {
    StatusCode::OK
}

#[instrument(skip(body, config))]
pub(crate) async fn handle_logs(
    TypedHeader(logplex_token): TypedHeader<LogplexDrainToken>,
    State(config): State<Arc<Config>>,
    RawBody(body): RawBody,
) -> impl IntoResponse {
    let sentry_client = match config.sentry_clients.get(logplex_token.as_str()) {
        Some(client) => client,
        None => {
            debug!(?logplex_token, "unknown logplex token");
            return StatusCode::BAD_REQUEST;
        }
    };

    let body = match body::to_bytes(body).await {
        Ok(body) => body,
        Err(err) => {
            warn!("could not fetch POST body: {:?}", err);
            return StatusCode::BAD_REQUEST;
        }
    };

    let body_text = match std::str::from_utf8(&body) {
        Ok(body) => body,
        Err(err) => {
            warn!("invalid UTF-8 in body: {:?}", err);
            return StatusCode::BAD_REQUEST;
        }
    };

    if let Err(err) = process_logs(sentry_client.clone(), body_text) {
        warn!("error processing logs: {:?}", err);
    }

    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{extractors::LOGPLEX_DRAIN_TOKEN, test_utils::initialize_tracing};
    use axum::{
        body::Body,
        http::{self, Request, StatusCode},
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_health_check() {
        let app = build_app(Arc::new(Config::default()));

        let response = app
            .oneshot(Request::builder().uri("/ht").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK)
    }

    #[tokio::test]
    async fn test_get_fails() {
        let app = build_app(Arc::new(Config::default()));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn test_post_parse_errors_dont_lead_to_server_error() {
        let _ = initialize_tracing();
        let config = Config::default();

        config
            .with_captured_sentry_events_async("something", |_, config| async move {
                let app = build_app(config.clone());
                let response = app
                    .oneshot(
                        Request::builder()
                            .method(http::Method::POST)
                            .uri("/")
                            .header(&LOGPLEX_DRAIN_TOKEN, "something")
                            .body(Body::from("some text"))
                            .unwrap(),
                    )
                    .await
                    .unwrap();

                assert_eq!(response.status(), StatusCode::OK);
                let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
                assert!(bytes.is_empty());
            })
            .await;
    }

    #[tokio::test]
    async fn test_post_wrong_drain_token() {
        let _ = initialize_tracing();
        let config = Config::default();

        config
            .with_captured_sentry_events_async("real_token", |_, config| async move {
                let app = build_app(config.clone());
                let response = app
                    .oneshot(
                        Request::builder()
                            .method(http::Method::POST)
                            .uri("/")
                            .header(&LOGPLEX_DRAIN_TOKEN, "other_token")
                            .body(Body::from("some text"))
                            .unwrap(),
                    )
                    .await
                    .unwrap();

                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
                let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
                assert!(bytes.is_empty());
            })
            .await;
    }

    #[tokio::test]
    async fn test_post_missing_drain_token() {
        let _ = initialize_tracing();
        let config = Arc::new(Config::default());
        let app = build_app(config.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();

        assert_eq!(body, "Header of type `logplex-drain-token` was missing");
    }
}
