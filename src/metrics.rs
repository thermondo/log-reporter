use chrono::{DateTime, FixedOffset};

use crate::{librato, log_parser::ScalingEvent};

/// generate librato metrics from scaling events
pub(crate) fn generate_librato_scaling_metrics(
    timestamp: &DateTime<FixedOffset>,
    events: &[ScalingEvent<'_>],
) -> Vec<librato::Measurement> {
    let mut result = Vec::with_capacity(events.len() * 2);

    for event in events {
        result.push(librato::Measurement {
            measure_time: *timestamp,
            kind: librato::Kind::Gauge,
            value: event.count as f64,
            source: event.proc.to_string(),
            name: format!("dyno_count.{}", event.size.to_lowercase()),
        });
        result.push(librato::Measurement {
            measure_time: *timestamp,
            kind: librato::Kind::Gauge,
            value: event.count as f64,
            source: event.proc.to_string(),
            name: "dyno_count".to_string(),
        });
    }

    result
}

#[cfg(test)]
mod tests {
    use self::librato::{Kind, Measurement};

    use super::*;
    use chrono::Local;

    #[test]
    fn test_generate_librato_scaling_metrics() {
        let ts = Local::now().fixed_offset();
        let result = generate_librato_scaling_metrics(
            &ts,
            &[ScalingEvent {
                proc: "web",
                count: 99,
                size: "huuuuge-2X",
            }],
        );

        assert_eq!(
            result,
            vec![
                Measurement {
                    measure_time: ts,
                    kind: Kind::Gauge,
                    name: "dyno_count.huuuuge-2x".into(),
                    value: 99.0,
                    source: "web".into()
                },
                Measurement {
                    measure_time: ts,
                    kind: Kind::Gauge,
                    name: "dyno_count".into(),
                    value: 99.0,
                    source: "web".into()
                },
            ]
        );
    }
}
