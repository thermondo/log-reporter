use crate::{config::Config, extractors::LogplexDrainToken, reporter::process_logs};
use anyhow::Context as _;
use axum::{
    extract::{RawBody, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router, TypedHeader,
};
use hyper::body;
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

    let body = match body::to_bytes(body)
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
    // into the "blocking task" threadpool of tokio.
    // See
    // https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html
    // When the tokio runtime is shut down / dropped, it
    // will wait until all spawned tasks are finished.
    //
    // FIXME: since this is CPU bound, we actually should use a semaphore to
    // limit the number of tasks, as recommended by the documentation.
    tokio::task::spawn_blocking({
        let sentry_client = sentry_client.clone();
        move || {
            let body_text = match std::str::from_utf8(&body).context("invalid UTF-8 in body") {
                Ok(body) => body,
                Err(err) => {
                    warn!("{:?}", err);
                    return;
                }
            };

            if let Err(err) = process_logs(sentry_client, body_text) {
                warn!("error processing logs: {:?}", err);
            }
        }
    });

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
                        Request::post("/")
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
            .oneshot(Request::post("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();

        assert_eq!(body, "Header of type `logplex-drain-token` was missing");
    }

    #[test]
    fn test_end_to_end_with_shutdown() {
        let _ = initialize_tracing();
        let config = Config::default();

        let input = "
            111 <158>1 2022-12-05T08:59:21.850424+00:00 host heroku router - \
            at=error code=H12 desc=\"Request timeout\" method=GET \
            path=/ host=myapp.herokuapp.com \
            request_id=8601b555-6a83-4c12-8269-97c8e32cdb22 \
            fwd=\"204.204.204.204\" dyno=web.1 connect=0ms service=30000ms \
            status=503 bytes=0 protocol=https\
            ";

        // not using `#[tokio::test]` so we can drop the runtime below
        // and force the pending blocking tasks to be finished.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let test_sentry_transport = runtime.block_on(config.with_captured_sentry_transport_async(
            "real_token",
            |_, config| async move {
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
                let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
                assert!(bytes.is_empty());
            },
        ));

        // this should wait for all pending `spawn_blocking` tasks from handling
        // the request above.
        drop(runtime);

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
