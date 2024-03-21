use anyhow::{anyhow, bail, Context, Result};
use sentry::{
    metrics::{DurationUnit, InformationUnit, Metric, MetricUnit, MetricValue},
    Client,
};
use std::{borrow::Cow, collections::HashMap};
use tracing::{debug, warn};

#[derive(Debug)]
struct SentryMetric {
    name: String,
    value: MetricValue,
    unit: MetricUnit,
}

impl PartialEq for SentryMetric {
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

fn parse_metric_unit(unit: &str) -> MetricUnit {
    // TODO: convert to `nom::tag_no_case` to safe allocations,
    // TODO: make a normal parser so it can be combined in spilt_value_and_unit
    match unit.to_ascii_lowercase().as_ref() {
        "ms" => MetricUnit::Duration(DurationUnit::MilliSecond),
        "s" => MetricUnit::Duration(DurationUnit::Second),
        "mb" => MetricUnit::Information(InformationUnit::MebiByte),
        "kb" => MetricUnit::Information(InformationUnit::KibiByte),
        "bytes" => MetricUnit::Information(InformationUnit::Byte),
        "pages" => MetricUnit::Custom("pages".into()),
        _ if unit.is_empty() => MetricUnit::None,
        _ => {
            warn!(unit, "got custom metric unit");
            MetricUnit::Custom(Cow::Owned(unit.to_owned()))
        }
    }
}

fn split_metric_value_and_unit(value: &str) -> Result<(f64, MetricUnit)> {
    // TODO: convert to nom
    let (value, unit) = {
        if let Some(pos) = value.find(|c: char| !c.is_ascii_digit() && c != '.') {
            value.split_at(pos)
        } else {
            (value, "")
        }
    };
    let value: f64 = value.parse().context("can't parse metric value")?;
    Ok((value, parse_metric_unit(unit)))
}

/// parses a key-value pair into a metric
/// example source:
///     key => sample#memory_total
///     value => 196.79MB
/// is already prevously split into key and value,
/// here we're combining both again into a SentryMetric with name, value, unit.
fn metric_from_kv<'a>(key: &'a str, value: &'a str) -> Result<SentryMetric> {
    // TODO: convert to nom
    let (kind, name) = key
        .split_once('#')
        .ok_or_else(|| anyhow!("missing separator in metric name"))?;

    let (value, unit) = split_metric_value_and_unit(value)?;

    Ok(SentryMetric {
        name: name.to_owned(),
        value: match kind {
            "sample" => MetricValue::Gauge(value),
            "count" => MetricValue::Counter(value),
            "measure" => MetricValue::Distribution(value),
            _ => bail!("unknown metric kind: {}", kind),
        },
        unit,
    })
}

/// report router metrics to the sentry client.
/// These don't come in the metric format, but are just generated metrics based on the router log.
pub(crate) fn report_router_metrics<'a, I>(client: &Client, key_value_pairs: I) -> Result<()>
where
    I: Iterator<Item = (&'a str, &'a str)>,
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
                let (value, unit) = split_metric_value_and_unit(value)?;
                client.add_metric(
                    Metric::distribution("router.connect", value)
                        .with_unit(unit)
                        .finish(),
                );
            }
            "service" => {
                let (value, unit) = split_metric_value_and_unit(value)?;
                client.add_metric(
                    Metric::distribution("router.service", value)
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
            let metric = match metric_from_kv(key, value) {
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
        assert_eq!(
            metric_from_kv(key, value).unwrap(),
            SentryMetric {
                name: expected_name.into(),
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
        assert_eq!(parse_metric_unit(unit), expected_unit);
    }
}
