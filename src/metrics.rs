use anyhow::Result;
use nom::{
    branch::alt,
    bytes::complete::{tag, tag_no_case},
    character::complete::char,
    combinator::{all_consuming, complete, eof, map, rest, value},
    number::complete::double,
    sequence::tuple,
    IResult,
};
use sentry::{
    metrics::{DurationUnit, InformationUnit, Metric, MetricUnit, MetricValue},
    Client,
};
use std::{borrow::Cow, collections::HashMap};
use tracing::{debug, warn};

use crate::log_parser::{LogMap, ScalingEvent};

const PAGES: MetricUnit = MetricUnit::Custom(Cow::Borrowed("pages"));

/// return a proc identifier (like `web`) from a source / dyno identifier (like `web.12`)
pub(crate) fn proc_from_source(source: &str) -> &str {
    if let Some((proc, _)) = source.split_once('.') {
        proc
    } else {
        source
    }
}

#[derive(Debug)]
pub(crate) struct SentryMetric<'a> {
    name: &'a str,
    value: MetricValue,
    unit: MetricUnit,
    tags: HashMap<&'a str, &'a str>,
}

impl<'a> Default for SentryMetric<'a> {
    fn default() -> Self {
        SentryMetric {
            name: "",
            value: MetricValue::Counter(0.0),
            unit: MetricUnit::None,
            tags: HashMap::new(),
        }
    }
}
impl<'a> From<SentryMetric<'a>> for sentry::metrics::Metric {
    fn from(metric: SentryMetric<'a>) -> Self {
        let mut builder =
            Metric::build(metric.name.to_owned(), metric.value).with_unit(metric.unit);

        for (tk, tv) in metric.tags.iter() {
            builder = builder.with_tag(tk.to_string(), tv.to_string());
        }

        builder.finish()
    }
}

impl PartialEq for SentryMetric<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.unit == other.unit
            && (match (&self.value, &other.value) {
                (MetricValue::Counter(lhs), MetricValue::Counter(rhs)) => lhs == rhs,
                (MetricValue::Distribution(lhs), MetricValue::Distribution(rhs)) => lhs == rhs,
                (MetricValue::Gauge(lhs), MetricValue::Gauge(rhs)) => lhs == rhs,
                _ => false,
            })
            && self.tags == other.tags
    }
}

fn is_metric(key: &str) -> bool {
    key.contains('#')
}

fn parse_metric_unit(unit: &str) -> IResult<&str, MetricUnit> {
    alt((
        value(
            MetricUnit::Information(InformationUnit::MebiByte),
            all_consuming(tag_no_case("mb")),
        ),
        value(
            MetricUnit::Information(InformationUnit::KibiByte),
            all_consuming(tag_no_case("kb")),
        ),
        value(
            MetricUnit::Information(InformationUnit::Byte),
            all_consuming(tag_no_case("bytes")),
        ),
        value(PAGES, all_consuming(tag_no_case("pages"))),
        value(
            MetricUnit::Duration(DurationUnit::MilliSecond),
            all_consuming(tag_no_case("ms")),
        ),
        value(
            MetricUnit::Duration(DurationUnit::Second),
            all_consuming(tag_no_case("s")),
        ),
        value(MetricUnit::None, eof),
        map(rest, |unit: &str| {
            warn!(unit, "got custom metric unit");
            MetricUnit::Custom(Cow::Owned(unit.to_owned()))
        }),
    ))(unit)
}

/// splits a text like `196.79MB` into the numeric value and an optional unit
fn parse_metric_value_and_unit(value: &str) -> IResult<&str, (f64, MetricUnit)> {
    tuple((double, parse_metric_unit))(value)
}

fn split_metric_value_and_unit(value: &str) -> Result<(f64, MetricUnit)> {
    complete(parse_metric_value_and_unit)(value)
        .map_err(|err| err.to_owned().into())
        .map(|(_, result)| result)
}

/// parses a key-value pair into a metric
/// example source:
///     key => sample#memory_total
///     value => 196.79MB
/// is already prevously split into key and value,
/// here we're combining both again into a SentryMetric with name, value, unit.
fn parse_metric_from_kv<'a>(key: &'a str, value: &'a str) -> Result<SentryMetric<'a>> {
    let (_, (metric_value, unit)) =
        complete(parse_metric_value_and_unit)(value).map_err(|err| err.to_owned())?;

    let (_, metric) = complete(map(
        tuple((
            alt((tag("sample"), tag("count"), tag("measure"))),
            char('#'),
            rest,
        )),
        move |(kind, _, name): (&str, _, &str)| SentryMetric {
            name,
            value: match kind {
                "sample" => MetricValue::Gauge(metric_value),
                "count" => MetricValue::Counter(metric_value),
                "measure" => MetricValue::Distribution(metric_value),
                _ => unreachable!(),
            },
            unit: unit.clone(),
            ..Default::default()
        },
    ))(key)
    .map_err(nom::Err::<nom::error::Error<&str>>::to_owned)?;

    Ok(metric)
}

/// generate metrics from scaling events
pub(crate) fn generate_scaling_metrics<'a>(
    events: &[ScalingEvent<'a>],
    user: &'a str,
) -> Vec<SentryMetric<'a>> {
    let mut result = Vec::with_capacity(events.len());

    for event in events {
        result.push(SentryMetric {
            name: "dyno_count",
            value: MetricValue::Gauge(event.count as f64),
            tags: HashMap::from_iter([("proc", event.proc), ("user", user), ("size", event.size)]),
            ..Default::default()
        })
    }

    result
}

/// generate router metrics from key/value pairs.
/// These don't come in the metric format, but are just generated metrics based on the router log.
pub(crate) fn generate_router_metrics<'a>(pairs: &'a LogMap<'a>) -> Vec<SentryMetric<'a>> {
    let tags: HashMap<&str, &str> = ["at", "method", "dyno", "protocol", "code"]
        .into_iter()
        .filter_map(|tagname| pairs.get(tagname).map(|tag| (tagname, *tag)))
        .collect();

    let mut result = Vec::with_capacity(4);

    if let Some(value) = pairs.get("bytes") {
        if let Ok(value) = value.parse::<u32>() {
            result.push(SentryMetric {
                name: "router.bytes",
                value: MetricValue::Distribution(value as f64),
                unit: MetricUnit::Information(InformationUnit::Byte),
                ..Default::default()
            })
        } else {
            warn!(value, "could not parse router.bytes value");
        }
    }

    if let Some(value) = pairs.get("connect") {
        match split_metric_value_and_unit(value) {
            Ok((metric_value, unit)) => result.push(SentryMetric {
                name: "router.connect",
                value: MetricValue::Distribution(metric_value),
                unit,
                ..Default::default()
            }),
            Err(err) => {
                warn!(?err, value, "could not parse router.connect value");
            }
        }
    }

    if let Some(value) = pairs.get("service") {
        match split_metric_value_and_unit(value) {
            Ok((metric_value, unit)) => result.push(SentryMetric {
                name: "router.service",
                value: MetricValue::Distribution(metric_value),
                unit,
                ..Default::default()
            }),
            Err(err) => {
                warn!(?err, value, "could not parse router.service value");
            }
        }
    }

    if let Some(value) = pairs.get("status") {
        if let Ok(status) = value.parse::<u16>() {
            result.push(SentryMetric {
                name: match status {
                    200..=299 => "router.status.2xx",
                    300..=399 => "router.status.3xx",
                    400..=499 => "router.status.4xx",
                    500..=599 => "router.status.5xx",
                    _ => "router.status.xxx",
                },
                value: MetricValue::Counter(1.0),
                unit: MetricUnit::None,
                ..Default::default()
            })
        } else {
            warn!(value, "could not parse status value");
        }
    }

    for m in result.iter_mut() {
        m.tags.clone_from(&tags);
    }

    result
}

/// generate metrics from key-value pairs
/// Should understand:
/// - native log-based metrics: https://devcenter.heroku.com/articles/librato#native-log-based-metrics
/// - custom log-based metrics: https://devcenter.heroku.com/articles/librato#custom-log-based-metrics
///
/// Everything that is not a metric in the line like the dyno or source will be converted
/// into tags.
///
/// We don't support annotations yet.
pub(crate) fn generate_metrics<'a>(pairs: &'a LogMap) -> impl Iterator<Item = SentryMetric<'a>> {
    let mut tags: HashMap<&str, &str> = pairs
        .iter()
        .filter(|(key, _)| !is_metric(key))
        .map(|(key, value)| (*key, *value))
        .collect();

    // in case of custom metrics, the `source` tag is a dyno identifier like `web.1`
    // We extract the proc type (`web`) from it if possible for easier filtering.
    if let Some(source) = tags.get("source") {
        tags.insert("proc", proc_from_source(source));
    }

    pairs
        .iter()
        .filter(|(key, _)| is_metric(key))
        .filter_map(move |(key, value)| {
            debug!(key, value, "got metric");
            let mut metric = match parse_metric_from_kv(key, value) {
                Ok(result) => result,
                Err(err) => {
                    warn!(key, value, ?err, "couldn't parse metric");
                    return None;
                }
            };

            metric.tags.clone_from(&tags);

            Some(metric)
        })
}

pub(crate) fn report_metrics<'a>(
    client: &Client,
    metrics: impl IntoIterator<Item = SentryMetric<'a>>,
) {
    for metric in metrics {
        debug!(?metric, "sending metric");
        client.add_metric(metric.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(
        "sample#memory_total",
        "196.79MB",
        "memory_total",
        MetricValue::Gauge(196.79),
        MetricUnit::Information(InformationUnit::MebiByte)
    )]
    #[test_case(
        "sample#memory-dash",
        "196.79MB",
        "memory-dash",
        MetricValue::Gauge(196.79),
        MetricUnit::Information(InformationUnit::MebiByte)
    )]
    #[test_case(
        "count#webhook.message.created.accepted",
        "1",
        "webhook.message.created.accepted",
        MetricValue::Counter(1.0),
        MetricUnit::None
    )]
    fn test_parse_metric_from_kv(
        key: &str,
        value: &str,
        expected_name: &str,
        expected_value: MetricValue,
        expected_unit: MetricUnit,
    ) {
        assert_eq!(
            parse_metric_from_kv(key, value).unwrap(),
            SentryMetric {
                name: expected_name,
                value: expected_value,
                unit: expected_unit,
                ..Default::default()
            }
        );
    }

    #[test_case("", MetricUnit::None)]
    #[test_case("some_custom_value", MetricUnit::Custom("some_custom_value".into()))]
    #[test_case("s", MetricUnit::Duration(DurationUnit::Second))]
    #[test_case("mb", MetricUnit::Information(InformationUnit::MebiByte))]
    #[test_case("kb", MetricUnit::Information(InformationUnit::KibiByte))]
    #[test_case("bytes", MetricUnit::Information(InformationUnit::Byte))]
    #[test_case("pages", MetricUnit::Custom("pages".into()))]
    fn test_parse_metric_unit(unit: &str, expected_unit: MetricUnit) {
        let (remainder, result) = parse_metric_unit(unit).unwrap();
        assert!(remainder.is_empty(), "{}", remainder);
        assert_eq!(result, expected_unit);
    }

    #[test_case("connect")]
    #[test_case("service")]
    #[test_case("status")]
    #[test_case("bytes")]
    fn test_router_metrics_skips_over_invalid_value(metric_name: &str) {
        let lm = LogMap::from_iter([(metric_name, "invalid_value")]);
        let result: Vec<_> = generate_router_metrics(&lm);
        assert!(result.is_empty());
    }

    #[test]
    fn test_generate_router_metrics_normal() {
        let lm = LogMap::from_iter([
            ("at", "info"),
            ("method", "GET"),
            ("path", "/api/disposition/service/?hub=33"),
            ("host", "thermondo-backend.herokuapp.com"),
            ("request_id", "60fbbe6e-0ea5-4013-ab6a-9d6851fe1c95"),
            ("fwd", "80.187.107.115,167.82.231.29"),
            ("dyno", "web.10"),
            ("connect", "2ms"),
            ("service", "864ms"),
            ("status", "200"),
            ("bytes", "15055"),
            ("protocol", "https"),
        ]);
        let result: Vec<_> = generate_router_metrics(&lm);
        let expected_tags: HashMap<&str, &str> = HashMap::from_iter([
            ("at", "info"),
            ("method", "GET"),
            ("dyno", "web.10"),
            ("protocol", "https"),
        ]);
        assert_eq!(
            result,
            vec![
                SentryMetric {
                    name: "router.bytes",
                    value: MetricValue::Distribution(15055.0),
                    unit: MetricUnit::Information(InformationUnit::Byte),
                    tags: expected_tags.clone(),
                    ..Default::default()
                },
                SentryMetric {
                    name: "router.connect",
                    value: MetricValue::Distribution(2.0),
                    unit: MetricUnit::Duration(DurationUnit::MilliSecond),
                    tags: expected_tags.clone(),
                    ..Default::default()
                },
                SentryMetric {
                    name: "router.service",
                    value: MetricValue::Distribution(864.0),
                    unit: MetricUnit::Duration(DurationUnit::MilliSecond),
                    tags: expected_tags.clone(),
                    ..Default::default()
                },
                SentryMetric {
                    name: "router.status.2xx",
                    value: MetricValue::Counter(1.0),
                    unit: MetricUnit::None,
                    tags: expected_tags.clone(),
                    ..Default::default()
                },
            ]
        );
    }
    #[test]
    fn test_generate_router_metrics_timeout() {
        let lm = LogMap::from_iter([
            ("at", "error"),
            ("code", "H12"),
            ("desc", "Request timeout"),
            ("method", "GET"),
            ("path", "/"),
            ("host", "myapp.herokuapp.com"),
            ("request_id", "8601b555-6a83-4c12-8269-97c8e32cdb22"),
            ("fwd", "204.204.204.204"),
            ("dyno", "web.1"),
            ("connect", "0ms"),
            ("service", "30000ms"),
            ("status", "503"),
            ("bytes", "0"),
            ("protocol", "https"),
        ]);
        let result: Vec<_> = generate_router_metrics(&lm);
        let expected_tags: HashMap<&str, &str> = HashMap::from_iter([
            ("at", "error"),
            ("code", "H12"),
            ("method", "GET"),
            ("dyno", "web.1"),
            ("protocol", "https"),
        ]);
        assert_eq!(
            result,
            vec![
                SentryMetric {
                    name: "router.bytes",
                    value: MetricValue::Distribution(0.0),
                    unit: MetricUnit::Information(InformationUnit::Byte),
                    tags: expected_tags.clone(),
                    ..Default::default()
                },
                SentryMetric {
                    name: "router.connect",
                    value: MetricValue::Distribution(0.0),
                    unit: MetricUnit::Duration(DurationUnit::MilliSecond),
                    tags: expected_tags.clone(),
                    ..Default::default()
                },
                SentryMetric {
                    name: "router.service",
                    value: MetricValue::Distribution(30000.0),
                    unit: MetricUnit::Duration(DurationUnit::MilliSecond),
                    tags: expected_tags.clone(),
                    ..Default::default()
                },
                SentryMetric {
                    name: "router.status.5xx",
                    value: MetricValue::Counter(1.0),
                    unit: MetricUnit::None,
                    tags: expected_tags.clone(),
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn test_generate_scaling_metrics() {
        let result = generate_scaling_metrics(
            &vec![ScalingEvent {
                proc: "web",
                count: 99,
                size: "huuuuge",
            }],
            "some-user@thermondo.de",
        );

        let wanted_tags = HashMap::from_iter([
            ("proc", "web"),
            ("user", "some-user@thermondo.de"),
            ("size", "huuuuge"),
        ]);

        assert_eq!(
            result,
            vec![SentryMetric {
                name: "dyno_count",
                value: MetricValue::Gauge(99.0),
                unit: MetricUnit::None,
                tags: wanted_tags.clone(),
            },]
        );
    }

    #[test]
    fn test_generate_metrics() {
        let pairs = LogMap::from_iter([
            ("source", "dramatiqworker.1"),
            (
                "dyno",
                "heroku.145151706.54c51996-a1c6-4491-8f76-b39b19374517",
            ),
            ("sample#memory_total", "110.70MB"),
            ("sample#memory_rss", "89.61MB"),
            ("sample#memory_cache", "20.91MB"),
            ("sample#memory_swap", "0.18MB"),
            ("sample#memory_pgpgin", "3244pages"),
            ("sample#memory_pgpgout", "176pages"),
            ("sample#memory_quota", "512.00MB"),
        ]);
        let result: Vec<_> = generate_metrics(&pairs).collect();

        let wanted_tags = HashMap::from_iter([
            ("source", "dramatiqworker.1"),
            ("proc", "dramatiqworker"),
            (
                "dyno",
                "heroku.145151706.54c51996-a1c6-4491-8f76-b39b19374517",
            ),
        ]);

        assert_eq!(
            result,
            vec![
                SentryMetric {
                    name: "memory_cache",
                    value: MetricValue::Gauge(20.91),
                    unit: MetricUnit::Information(InformationUnit::MebiByte),
                    tags: wanted_tags.clone(),
                },
                SentryMetric {
                    name: "memory_pgpgin",
                    value: MetricValue::Gauge(3244.0),
                    unit: PAGES,
                    tags: wanted_tags.clone(),
                },
                SentryMetric {
                    name: "memory_pgpgout",
                    value: MetricValue::Gauge(176.0),
                    unit: PAGES,
                    tags: wanted_tags.clone(),
                },
                SentryMetric {
                    name: "memory_quota",
                    value: MetricValue::Gauge(512.0),
                    unit: MetricUnit::Information(InformationUnit::MebiByte),
                    tags: wanted_tags.clone(),
                },
                SentryMetric {
                    name: "memory_rss",
                    value: MetricValue::Gauge(89.61),
                    unit: MetricUnit::Information(InformationUnit::MebiByte),
                    tags: wanted_tags.clone(),
                },
                SentryMetric {
                    name: "memory_swap",
                    value: MetricValue::Gauge(0.18),
                    unit: MetricUnit::Information(InformationUnit::MebiByte),
                    tags: wanted_tags.clone(),
                },
                SentryMetric {
                    name: "memory_total",
                    value: MetricValue::Gauge(110.7),
                    unit: MetricUnit::Information(InformationUnit::MebiByte),
                    tags: wanted_tags.clone(),
                },
            ]
        )
    }
}
