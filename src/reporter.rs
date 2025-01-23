use crate::{
    config::Destination,
    log_parser::{
        parse_dyno_error_code, parse_key_value_pairs, parse_log_line, parse_offer_extension_number,
        parse_offer_number, parse_project_reference, parse_scaling_event, parse_sfid, Kind,
        LogLine, LogMap,
    },
    metrics::generate_librato_scaling_metrics,
};
use anyhow::{Context as _, Result};
use axum::http::uri::Uri;
use sentry::{Client, Hub, Level, Scope};
use std::collections::{HashMap, HashSet};
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

fn generate_dyno_error_message(code: &str, name: &str, logline: &LogLine) -> Option<SentryMessage> {
    let server_name = logline.source;
    Some(SentryMessage {
        tags: HashMap::from_iter(vec![("server_name".into(), server_name.into())]),
        fingerprint: vec![
            format!("heroku-dyno-error-{}", code.to_lowercase()),
            server_name.into(),
        ],
        message: format!("{} ({}) on {}\n{}", name, code, server_name, logline.text),
    })
}

fn generate_request_timeout_message(logline: &LogLine, items: &LogMap) -> Option<SentryMessage> {
    let mut tags: HashMap<String, String> = HashMap::new();

    let path = items.get("path")?;

    let full_url = Uri::builder()
        .scheme("https")
        .authority(*items.get("host")?)
        .path_and_query(*path)
        .build()
        .ok()?;

    let route_name = route_from_path(full_url.path());

    tags.insert("transaction".into(), route_name.clone());
    tags.insert("url".into(), full_url.to_string());

    if let Some(request_id) = items.get("request_id") {
        tags.insert("request_id".into(), request_id.to_string());
    }

    if let Some(dyno) = items.get("dyno") {
        tags.insert("server_name".into(), dyno.to_string());
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

#[instrument(fields(dsn=?destination.sentry_client.dsn()), skip(destination))]
pub(crate) fn process_logs(destination: Arc<Destination>, input: &str) -> Result<()> {
    let mut seen_sources: HashSet<&str> = HashSet::new();
    for line in input.lines() {
        debug!("handling log line: {}", line);

        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (_, log) = parse_log_line(line)
            .map_err(|err| err.to_owned())
            .context("could not parse log line")?;

        let parse_pairs = || {
            parse_key_value_pairs(log.text)
                .map_err(|err| err.to_owned())
                .with_context(|| format!("could not parse key value pairs from {}", log.text))
                .map(|(_, pairs)| pairs)
        };

        seen_sources.insert(log.source);

        if matches!(log.kind, Kind::Heroku) && log.source == "router" {
            let map = parse_pairs()?;

            debug!(?map, "got router log");

            let Some(at) = map.get("at") else {
                warn!(?line, "missing `at` in router log line");
                continue;
            };

            if *at != "error" {
                continue;
            }

            let Some(code) = map.get("code") else {
                warn!(?line, "missing `code` in router `error` log line");
                continue;
            };

            if *code == "H12" {
                if let Some(msg) = generate_request_timeout_message(&log, &map) {
                    send_to_sentry(destination.sentry_client.clone(), msg);
                }
            }
        } else if let Ok((_, (code, name))) = parse_dyno_error_code(log.text) {
            if let Some(msg) = generate_dyno_error_message(code, name, &log) {
                send_to_sentry(destination.sentry_client.clone(), msg);
            }
        } else if matches!(log.kind, Kind::App)
            && log.source == "api"
            && destination.librato_client.is_some()
        {
            let Ok((_, (events, _user))) = parse_scaling_event(log.text) else {
                continue;
            };

            let Some(ref librato_client) = destination.librato_client else {
                continue;
            };

            debug!("trying to report scaling metrics");

            // store the scaling events in a cache so we can regularly re-send them.
            let mut last_events = destination.last_scaling_events.lock().unwrap();
            *last_events = Some(events.iter().map(Into::into).collect());

            for measurement in generate_librato_scaling_metrics(&log.timestamp, &events) {
                librato_client.add_measurement(measurement);
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
            config.with_captured_sentry_events_sync("logplex_token", |sentry_client, _cfg| {
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
            config.with_captured_sentry_events_sync("logplex_token", |sentry_client, _cfg| {
                process_logs(sentry_client, input).expect("error processing logs");
            });

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].message.as_ref().unwrap(),
            "Boot timeout (R10) on web.1\n\
            Error R10 (Boot timeout) -> \
            Web process failed to bind to $PORT within 60 seconds of launch"
        );
    }

    #[test]
    fn test_generate_boot_timeout_message() {
        let msg = generate_dyno_error_message(
            "R10",
            "Boot timeout",
            &LogLine {
                timestamp: "2022-12-05T08:59:21.850424+00:00".parse().unwrap(),
                source: "web.1",
                kind: Kind::App,
                text: "Error R10 (Boot timeout) -> Web process failed to bind to $PORT within 60 seconds of launch"
            }).unwrap();
        assert_eq!(
            msg.message,
            "Boot timeout (R10) on web.1\nError R10 (Boot timeout) -> Web process failed to bind to $PORT within 60 seconds of launch",
        );
        assert_eq!(msg.fingerprint, vec!["heroku-dyno-error-r10", "web.1"]);
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
                source: "heroku",
                kind: Kind::Heroku,
                text: "doesn't matter here",
            },
            &LogMap::from_iter([
                ("path", "/path/"),
                ("dyno", "web.1"),
                ("host", "www.thermondo.de"),
                ("request_id", "8601b555-6a83-4c12-8269-97c8e32cdb22"),
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
                source: "heroku",
                kind: Kind::Heroku,
                text: "doesn't matter here",
            },
            &LogMap::from_iter([("path", "/path/1234/"), ("host", "www.thermondo.de")]),
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
