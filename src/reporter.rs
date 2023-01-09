use crate::log_parser::{parse_key_value_pairs, parse_log_line, Kind};
use anyhow::{anyhow, Result};
use axum::http::uri::Uri;
use sentry::{Client, Hub, Level, Scope};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info, instrument, warn};
use uuid::Uuid;

use crate::log_parser::{parse_project_reference, parse_sfid, LogLine};

#[derive(Debug)]
struct SentryMessage {
    tags: HashMap<String, String>,
    fingerprint: Vec<String>,
    message: String,
}

/// generate a route-name from a URL path.
/// Replaces elements in the URL that are
/// - positive integers
/// - UUIDs
fn route_from_path(path: &str) -> String {
    let elements: Vec<_> = path
        .split('/')
        .map(|el| {
            if el.parse::<u64>().is_ok() {
                "{number}"
            } else if Uuid::try_parse(el).is_ok() {
                "{uuid}"
            } else if parse_sfid(el).is_ok() {
                "{sfid}"
            } else if parse_project_reference(el).is_ok() {
                "{project_reference}"
            } else {
                el
            }
        })
        .collect();
    elements.join("/")
}

fn generate_timeout_message(
    logline: &LogLine,
    items: &HashMap<String, String>,
) -> Option<SentryMessage> {
    let mut tags: HashMap<String, String> = HashMap::new();

    let path = items.get("path")?.as_str();

    let full_url = Uri::builder()
        .scheme("https")
        .authority(items.get("host")?.as_str())
        .path_and_query(path)
        .build()
        .ok()?;

    let route_name = route_from_path(full_url.path());

    tags.insert("transaction".into(), route_name.clone());
    tags.insert("url".into(), full_url.to_string());

    if let Some(request_id) = items.get("request_id") {
        tags.insert("request_id".into(), request_id.into());
    }

    if let Some(dyno) = items.get("dyno") {
        tags.insert("server_name".into(), dyno.into());
    }

    Some(SentryMessage {
        tags,
        fingerprint: vec!["heroku-router-request-timeout".into(), route_name.clone()],
        message: format!("request timeout on {}\n{}", route_name, logline.text),
    })
}

#[instrument(fields(dsn=?sentry_client.dsn()), skip(sentry_client))]
fn send_to_sentry(sentry_client: Arc<Client>, message: SentryMessage) {
    info!(?message, "reporting timeout to sentry");

    let mut scope = Scope::default();
    scope.set_level(Some(Level::Error));
    for (key, value) in message.tags {
        scope.set_tag(&key, &value);
    }

    let fingerprint: Vec<_> = message.fingerprint.iter().map(String::as_str).collect();

    scope.set_fingerprint(Some(&fingerprint));

    let hub = Hub::new(Some(sentry_client), Arc::new(scope));
    let uuid = hub.capture_message(&message.message, Level::Error);
    info!(?uuid, last_event_id = ?hub.last_event_id(), "captured message");
}

#[instrument(fields(dsn=?sentry_client.dsn()), skip(sentry_client))]
pub(crate) fn process_logs(sentry_client: Arc<Client>, input: &str) -> Result<()> {
    for line in input.lines() {
        debug!("handling log line: {}", line);

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let (_, log) =
            parse_log_line(line).map_err(|err| anyhow!("could not parse log line: {:?}", err))?;

        if log.kind == Kind::Heroku && log.source == "router" {
            let (_, pairs) = parse_key_value_pairs(&log.text).map_err(|err| {
                anyhow!("could not parse key value pairs: {:?}\n{}", err, log.text)
            })?;

            let map: HashMap<String, String> = HashMap::from_iter(pairs.into_iter());

            debug!(?map, "got router log");

            let at = if let Some(at) = map.get("at") {
                at
            } else {
                warn!(?line, "missing `at` in router log line");
                continue;
            };

            if at != "error" {
                continue;
            }

            let code = if let Some(code) = map.get("code") {
                code
            } else {
                warn!(?line, "missing `code` in router `error` log line");
                continue;
            };

            if code == "H12" {
                if let Some(msg) = generate_timeout_message(&log, &map) {
                    send_to_sentry(sentry_client.clone(), msg);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, test_utils::initialize_tracing};
    use test_case::test_case;

    #[test]
    fn test_process_log() {
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

        let events =
            config.with_captured_sentry_events_sync("logplex_token", |sentry_client, _config| {
                process_logs(sentry_client, input).expect("error processing logs");
            });

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

    #[test]
    fn test_generate_full_timeout_message() {
        let msg = generate_timeout_message(
            &LogLine {
                timestamp: "2022-12-05T08:59:21.850424+00:00".parse().unwrap(),
                source: "heroku".into(),
                kind: Kind::Heroku,
                text: "doesn't matter here".into(),
            },
            &HashMap::from_iter([
                ("path".into(), "/path/".into()),
                ("dyno".into(), "web.1".into()),
                ("host".into(), "www.thermondo.de".into()),
                (
                    "request_id".into(),
                    "8601b555-6a83-4c12-8269-97c8e32cdb22".into(),
                ),
            ]),
        )
        .unwrap();
        assert_eq!(
            msg.message,
            "request timeout on /path/\ndoesn't matter here"
        );
        assert_eq!(
            msg.fingerprint,
            vec!["heroku-router-request-timeout", "/path/"]
        );
        assert_eq!(
            msg.tags,
            HashMap::from_iter([
                ("transaction".into(), "/path/".into()),
                ("url".into(), "https://www.thermondo.de/path/".into()),
                (
                    "request_id".into(),
                    "8601b555-6a83-4c12-8269-97c8e32cdb22".into()
                ),
                ("server_name".into(), "web.1".into()),
            ])
        );
    }

    #[test]
    fn test_generate_minimal_timeout_message() {
        let msg = generate_timeout_message(
            &LogLine {
                timestamp: "2022-12-05T08:59:21.850424+00:00".parse().unwrap(),
                source: "heroku".into(),
                kind: Kind::Heroku,
                text: "doesn't matter here".into(),
            },
            &HashMap::from_iter([
                ("path".into(), "/path/1234/".into()),
                ("host".into(), "www.thermondo.de".into()),
            ]),
        )
        .unwrap();
        assert_eq!(
            msg.message,
            "request timeout on /path/{number}/\ndoesn't matter here"
        );
        assert_eq!(
            msg.fingerprint,
            vec!["heroku-router-request-timeout", "/path/{number}/"]
        );
        assert_eq!(
            msg.tags,
            HashMap::from_iter([
                ("transaction".into(), "/path/{number}/".into()),
                ("url".into(), "https://www.thermondo.de/path/1234/".into()),
            ])
        );
    }

    #[test_case("", ""; "1")]
    #[test_case("/", "/")]
    #[test_case("/asdf", "/asdf")]
    #[test_case("/asdf/ddd", "/asdf/ddd")]
    #[test_case("/asdf/1234/something/", "/asdf/{number}/something/")]
    #[test_case(
        "/asdf/8601b555-6a83-4c12-8269-97c8e32cdb22/something/",
        "/asdf/{uuid}/something/"
    )]
    fn test_route_from_path(input: &str, expected: &str) {
        assert_eq!(route_from_path(input), expected);
    }
}
