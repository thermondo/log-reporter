use crate::log_parser::{
    parse_key_value_pairs, parse_log_line, parse_offer_extension_number, parse_offer_number,
    parse_project_reference, parse_sfid, Kind, LogLine,
};
use anyhow::{Context as _, Result};
use axum::http::uri::Uri;
use sentry::{Client, Hub, Level, Scope};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info, instrument, warn};
use uuid::Uuid;

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
/// - Salesforce IDs
/// - thermondo project references
/// - thermondo offer & offer-extension numbers
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
            } else if parse_offer_number(el).is_ok() {
                "{offer_number}"
            } else if parse_offer_extension_number(el).is_ok() {
                "{offer_extension_number}"
            } else {
                el
            }
        })
        .collect();
    elements.join("/")
}

fn generate_boot_timeout_message(logline: &LogLine) -> Option<SentryMessage> {
    let server_name = logline.source.clone();
    Some(SentryMessage {
        tags: HashMap::from_iter(vec![("server_name".into(), server_name.clone())]),
        fingerprint: vec!["heroku-dyno-boot-timeout".into(), server_name.clone()],
        message: format!("boot timeout on {}\n{}", server_name, logline.text),
    })
}

fn generate_request_timeout_message(
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

    // uses an empty & new scope instead of the
    // standard scope which would include details of
    // this specific service.
    let mut scope = Scope::default();
    scope.set_level(Some(Level::Error));
    for (key, value) in message.tags {
        scope.set_tag(&key, &value);
    }

    // the fingerprint is used for grouping the messages in sentry.
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

        let (_, log) = parse_log_line(line)
            .map_err(|err| err.to_owned())
            .context("could not parse log line")?;

        if !matches!(log.kind, Kind::Heroku) {
            continue;
        }

        if log.source == "router" {
            let (_, pairs) = parse_key_value_pairs(&log.text)
                .map_err(|err| err.to_owned())
                .with_context(|| format!("could not parse key value pairs from {}", log.text))?;

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
                if let Some(msg) = generate_request_timeout_message(&log, &map) {
                    send_to_sentry(sentry_client.clone(), msg);
                }
            }
        } else if log.text.starts_with("Error R10 (Boot timeout) ") {
            if let Some(msg) = generate_boot_timeout_message(&log) {
                send_to_sentry(sentry_client.clone(), msg);
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
    fn test_dyno_boot_timeout_process_log() {
        let _ = initialize_tracing();
        let config = Config::default();

        let input = "
            152 <134>1 2023-04-29T23:11:12.604871+00:00 host heroku web.1 - \
            Error R10 (Boot timeout) -> \
            Web process failed to bind to $PORT within 60 seconds of launch\
            ";

        let events =
            config.with_captured_sentry_events_sync("logplex_token", |sentry_client, _config| {
                process_logs(sentry_client, input).expect("error processing logs");
            });

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].message.as_ref().unwrap(),
            "boot timeout on web.1\n\
            Error R10 (Boot timeout) -> \
            Web process failed to bind to $PORT within 60 seconds of launch"
        );
    }

    #[test]
    fn test_generate_boot_timeout_message() {
        let msg = generate_boot_timeout_message(
            &LogLine {
                timestamp: "2022-12-05T08:59:21.850424+00:00".parse().unwrap(),
                source: "web.1".into(),
                kind: Kind::App,
                text: "Error R10 (Boot timeout) -> Web process failed to bind to $PORT within 60 seconds of launch".into()
            }).unwrap();
        assert_eq!(
            msg.message,
            "boot timeout on web.1\nError R10 (Boot timeout) -> Web process failed to bind to $PORT within 60 seconds of launch",
        );
        assert_eq!(msg.fingerprint, vec!["heroku-dyno-boot-timeout", "web.1"]);
        assert_eq!(
            msg.tags,
            HashMap::from_iter([("server_name".into(), "web.1".into()),])
        );
    }

    #[test]
    fn test_generate_full_timeout_message() {
        let msg = generate_request_timeout_message(
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
        let msg = generate_request_timeout_message(
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
