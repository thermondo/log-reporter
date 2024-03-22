use crate::{config::Config, extractors::LogplexDrainToken, reporter::process_logs};
use anyhow::Context as _;
use axum::{
    body::{self, Body},
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use axum_extra::TypedHeader;
use std::sync::Arc;
use tracing::{debug, instrument, warn};

pub(crate) fn build_app(config: Arc<Config>) -> Router {
    Router::new()
        .route("/ht", get(health_check))
        .route("/", post(handle_logs))
        .with_state(config)
}

pub(crate) async fn health_check() -> impl IntoResponse {
    StatusCode::OK
}

#[allow(
    // open bug in tokio/tracing, see:
    // https://github.com/tokio-rs/tracing/issues/2503
    clippy::let_with_type_underscore
)]
#[instrument(skip(body, config))]
pub(crate) async fn handle_logs(
    TypedHeader(logplex_token): TypedHeader<LogplexDrainToken>,
    State(config): State<Arc<Config>>,
    body: Body,
) -> impl IntoResponse {
    let sentry_client = match config.sentry_clients.get(logplex_token.as_str()) {
        Some(client) => client,
        None => {
            debug!(?logplex_token, "unknown logplex token");
            return StatusCode::BAD_REQUEST;
        }
    };

    let body = match body::to_bytes(body, usize::MAX)
        .await
        .context("could not fetch POST body")
    {
        Ok(body) => body,
        Err(err) => {
            warn!("{:?}", err);
            return StatusCode::BAD_REQUEST;
        }
    };

    // move decoding, parsing and creating the logmessage
    // into the main background rayon threadpool.
    //
    // By default, When the app is shut down, pending tasks
    // would be dropped by rayon.
    //
    // By using a [`WaitGroup`](crossbeam_utils::sync::WaitGroup),
    // we can wait for any task that holds a cloned instance of it.
    {
        let sentry_client = sentry_client.clone();
        let config = config.clone();
        let task_wait_ticket = config.waitgroup.clone();
        rayon::spawn(move || {
            let body_text = match std::str::from_utf8(&body).context("invalid UTF-8 in body") {
                Ok(body) => body,
                Err(err) => {
                    warn!("{:?}", err);
                    return;
                }
            };

            if let Err(err) = process_logs(&config, sentry_client, body_text) {
                warn!("error processing logs: {:?}", err);
            }
            // we actually don't need the `drop` here,
            // we only use it so `task_wait_ticket` will be moved into
            // the closure.
            drop(task_wait_ticket);
        });
    }

    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{extractors::LOGPLEX_DRAIN_TOKEN, test_utils::initialize_tracing};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use crossbeam_utils::sync::WaitGroup;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_health_check() {
        let app = build_app(Arc::new(Config::default()));

        let response = app
            .oneshot(Request::get("/ht").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK)
    }

    #[tokio::test]
    async fn test_get_fails() {
        let app = build_app(Arc::new(Config::default()));

        let response = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
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
                        Request::post("/")
                            .header(&LOGPLEX_DRAIN_TOKEN, "something")
                            .body(Body::from("some text"))
                            .unwrap(),
                    )
                    .await
                    .unwrap();

                assert_eq!(response.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
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
                        Request::post("/")
                            .header(&LOGPLEX_DRAIN_TOKEN, "other_token")
                            .body(Body::from("some text"))
                            .unwrap(),
                    )
                    .await
                    .unwrap();

                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
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
            .oneshot(Request::post("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();

        assert_eq!(body, "Header of type `logplex-drain-token` was missing");
    }

    #[tokio::test]
    async fn test_end_to_end_with_shutdown() {
        let _ = initialize_tracing();
        let wg = WaitGroup::new();
        let config = Config::default().with_waitgroup(wg.clone());

        let input = "
            111 <158>1 2022-12-05T08:59:21.850424+00:00 host heroku router - \
            at=error code=H12 desc=\"Request timeout\" method=GET \
            path=/ host=myapp.herokuapp.com \
            request_id=8601b555-6a83-4c12-8269-97c8e32cdb22 \
            fwd=\"204.204.204.204\" dyno=web.1 connect=0ms service=30000ms \
            status=503 bytes=0 protocol=https\
            ";

        let test_sentry_transport = config
            .with_captured_sentry_transport_async("real_token", |_, config| async move {
                let app = build_app(config.clone());
                let response = app
                    .oneshot(
                        Request::post("/")
                            .header(&LOGPLEX_DRAIN_TOKEN, "real_token")
                            .body(Body::from(input))
                            .unwrap(),
                    )
                    .await
                    .unwrap();

                assert_eq!(response.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                assert!(bytes.is_empty());
            })
            .await;

        // wait for async tasks to finish
        wg.wait();

        let events: Vec<sentry::protocol::Event<'static>> = test_sentry_transport
            .fetch_and_clear_envelopes()
            .iter()
            .filter_map(|envelope| envelope.event().cloned())
            .collect();

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].message.as_ref().unwrap(),
            "request timeout on /\n\
             at=error code=H12 desc=\"Request timeout\" \
             method=GET path=/ host=myapp.herokuapp.com \
             request_id=8601b555-6a83-4c12-8269-97c8e32cdb22 \
             fwd=\"204.204.204.204\" dyno=web.1 connect=0ms \
             service=30000ms status=503 bytes=0 protocol=https"
        );
    }
}
