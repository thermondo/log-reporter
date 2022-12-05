use std::collections::HashMap;

use crate::{
    config::Config,
    extractors::LogplexDrainToken,
    log_parser::{parse_key_value_pairs, parse_log_line, Kind},
    reporter::report_to_sentry,
};
use anyhow::Context as _;
use axum::{
    extract::{RawBody, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router, TypedHeader,
};
use axum_extra::routing::RouterExt;
use hyper::body;
use sentry_anyhow::capture_anyhow;
use std::sync::Arc;
use tracing::{debug, error, info, instrument, warn};

pub(crate) fn build_app(config: Config) -> Router {
    Router::new()
        .route_with_tsr("/ht", get(health_check))
        .route("/", post(handle_logs))
        .with_state(Arc::new(config))
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

    for line in body_text.lines() {
        debug!("handling log line: {}", line);

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        match parse_log_line(line) {
            Ok((_, log)) => {
                if log.kind == Kind::Heroku && log.source == "router" {
                    match parse_key_value_pairs(&log.text) {
                        Ok((_, pairs)) => {
                            let map: HashMap<String, String> =
                                HashMap::from_iter(pairs.into_iter());

                            debug!(?map, "got router log");

                            if map.get("at") == Some(&"error".into())
                                && map.get("code") == Some(&"H12".into())
                            {
                                info!(path=?map.get("path"), "got timeout ");
                                if let Err(err) =
                                    report_to_sentry(sentry_client.clone(), &log, &map)
                                        .context("error sending error to sentry")
                                {
                                    error!(?err, "error trying to report timeout to sentry");
                                    capture_anyhow(&err);
                                }
                            }
                        }
                        Err(err) => {
                            warn!("could not parse key value pairs: {:?}\n{}", err, log.text);
                        }
                    }
                }
            }
            Err(err) => {
                warn!("could not parse log line: {:?}\n{}", err, line);
            }
        }
    }

    StatusCode::OK
}

//
// 277 <158>1 2012-10-11T03:47:20+00:00 host heroku router - at=error code=H12 desc="Request
//     timeout" method=GET path=/ host=myapp.herokuapp.com
//     request_id=8601b555-6a83-4c12-8269-97c8e32cdb22 fwd="204.204.204.204" dyno=web.1 connect=
//     service=30000ms status=503 bytes=0 protocol=http

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extractors::LOGPLEX_DRAIN_TOKEN;
    use axum::{
        body::Body,
        http::{self, Request, StatusCode},
    };
    use tower::ServiceExt;

    #[must_use]
    fn initialize_tracing() -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(tracing_subscriber::fmt().with_test_writer().finish())
    }

    #[tokio::test]
    async fn test_health_check() {
        let app = build_app(Config::default());

        let response = app
            .oneshot(Request::builder().uri("/ht").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK)
    }

    #[tokio::test]
    async fn test_get_fails() {
        let app = build_app(Config::default());

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn test_post_parse_errors_dont_lead_to_error() {
        let _ = initialize_tracing();
        let app = build_app(Config::default().with_fake_sentry_client("something"));

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
    }

    #[tokio::test]
    async fn test_post_missing_drain_token() {
        let _ = initialize_tracing();
        let app = build_app(Config::default());

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
