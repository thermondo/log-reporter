use std::collections::HashMap;

use crate::{
    extractors::LogplexDrainToken,
    log_parser::{parse_key_value_pairs, parse_log_line, Kind},
};
use axum::{
    extract::RawBody, http::StatusCode, response::IntoResponse, routing::post, Router, TypedHeader,
};
use hyper::body;
use tracing::{debug, error, instrument, warn};

pub(crate) fn get_app() -> Router {
    Router::new().route("/", post(handle_logs))
}

#[instrument(skip(body))]
pub(crate) async fn handle_logs(
    TypedHeader(logplex_token): TypedHeader<LogplexDrainToken>,
    RawBody(body): RawBody,
) -> impl IntoResponse {
    // FIXME: change to     extract::BodyStream,

    let body = match body::to_bytes(body).await {
        Ok(body) => body,
        Err(err) => {
            // FIXME: report to sentry?
            error!("could not fetch POST body: {:?}", err);
            return StatusCode::BAD_REQUEST;
        }
    };

    let body_text = match std::str::from_utf8(&body) {
        Ok(body) => body,
        Err(err) => {
            // FIXME: report to sentry?
            error!("invalid UTF-8 in body: {:?}", err);
            return StatusCode::BAD_REQUEST;
        }
    };

    // FIXME: validate logplex token

    for line in body_text.lines() {
        debug!("handling log line: {}", line);

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
                                todo!();
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
