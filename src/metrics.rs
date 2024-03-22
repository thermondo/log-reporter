use anyhow::{Context, Result};
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

static PAGES: MetricUnit = MetricUnit::Custom(Cow::Borrowed("pages"));

#[derive(Debug)]
struct SentryMetric<'a> {
    name: &'a str,
    value: MetricValue,
    unit: MetricUnit,
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
        map(all_consuming(tag_no_case("pages")), |_| PAGES.clone()),
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
fn parse_metric_from_kv<'a>(key: &'a str, value: &'a str) -> IResult<&'a str, SentryMetric<'a>> {
    let (_, (metric_value, unit)) = complete(parse_metric_value_and_unit)(value)?;

    map(
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
        },
    )(key)
}

/// report router metrics to the sentry client.
/// These don't come in the metric format, but are just generated metrics based on the router log.
pub(crate) fn report_router_metrics<'a, 'b, I>(client: &Client, key_value_pairs: I) -> Result<()>
where
    I: Iterator<Item = (&'a str, &'b str)>,
{
    for (key, value) in key_value_pairs {
        match key {
            "bytes" => {
                let value: u32 = value.parse().context("can't parse bytes")?;
                client.add_metric(
                    Metric::distribution("router.bytes", value as f64)
                        .with_unit(MetricUnit::Information(InformationUnit::Byte))
                        .finish(),
                );
            }
            "connect" => {
                let (metric_value, unit) = split_metric_value_and_unit(value)?;
                client.add_metric(
                    Metric::distribution("router.connect", metric_value)
                        .with_unit(unit)
                        .finish(),
                );
            }
            "service" => {
                let (metric_value, unit) = split_metric_value_and_unit(value)?;
                client.add_metric(
                    Metric::distribution("router.service", metric_value)
                        .with_unit(unit)
                        .finish(),
                );
            }
            "status" => {
                let status: u16 = value.parse().context("can't parse status code")?;
                client.add_metric(
                    Metric::count(format!(
                        "router.status.{}",
                        match status {
                            200..=299 => "2xx",
                            300..=399 => "3xx",
                            400..=499 => "4xx",
                            500..=599 => "5xx",
                            _ => "xxx",
                        }
                    ))
                    .finish(),
                );
            }
            _ => {}
        }
    }

    Ok(())
}

/// parse metrics from key-value pairs and report them to the sentry clients.
/// Should understand:
/// - native log-based metrics: https://devcenter.heroku.com/articles/librato#native-log-based-metrics
/// - custom log-based metrics: https://devcenter.heroku.com/articles/librato#custom-log-based-metrics
///
/// Metric everything that is not a metric in the line like the dyno or source, will be convert
/// into tags.
///
/// We don't support annotations yet.
pub(crate) fn report_metrics<'a, I>(client: &Client, key_value_pairs: I) -> Result<()>
where
    I: Iterator<Item = (&'a str, &'a str)>,
{
    let mut tags: HashMap<String, String> = HashMap::new();

    for (key, value) in key_value_pairs {
        if is_metric(key) {
            debug!(key, value, "got metric");
            let (_, metric) = match parse_metric_from_kv(key, value) {
                Ok(result) => result,
                Err(err) => {
                    warn!(key, value, ?err, "couldn't parse metric");
                    continue;
                }
            };

            let mut builder =
                Metric::build(metric.name.to_owned(), metric.value).with_unit(metric.unit);

            for (tk, tv) in tags.iter() {
                builder = builder.with_tag(tk.clone(), tv.clone());
            }

            let metric = builder.finish();

            debug!(?metric, "sending metric");
            client.add_metric(metric);
        } else {
            debug!(key, value, "got tag");
            tags.insert(key.to_owned(), value.to_owned());
        }
    }

    Ok(())
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
        let (remainder, result) = parse_metric_from_kv(key, value).unwrap();
        assert!(remainder.is_empty());
        assert_eq!(
            result,
            SentryMetric {
                name: expected_name,
                value: expected_value,
                unit: expected_unit
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
}
