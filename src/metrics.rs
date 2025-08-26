use chrono::{DateTime, FixedOffset};

use crate::{graphite, log_parser::ScalingEvent};

/// generate graphite metrics from scaling events
pub(crate) fn generate_graphite_scaling_metrics(
    timestamp: &DateTime<FixedOffset>,
    events: &[ScalingEvent<'_>],
) -> Vec<graphite::Measurement> {
    events
        .iter()
        .map(|event| {
            // we we only need the low level detailed scaling event.
            // If we don't care about the size, we would run a query like `web.dyno_count.*:sum`
            graphite::Measurement {
                measure_time: *timestamp,
                value: event.count as f64,
                name: format!("{}.dyno_count.{}", event.proc, event.size.to_lowercase()),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use self::graphite;

    use super::*;
    use chrono::Local;

    #[test]
    fn test_generate_graphite_scaling_metrics() {
        let ts = Local::now().fixed_offset();
        let result = generate_graphite_scaling_metrics(
            &ts,
            &[ScalingEvent {
                proc: "web",
                count: 99,
                size: "huuuuge-2X",
            }],
        );

        assert_eq!(
            result,
            vec![graphite::Measurement {
                measure_time: ts,
                name: "web.dyno_count.huuuuge-2x".into(),
                value: 99.0,
            },]
        );
    }
}
