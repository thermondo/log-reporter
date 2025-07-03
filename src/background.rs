use crate::{
    config::Config,
    log_parser::ScalingEvent,
    metrics::{generate_graphite_scaling_metrics, generate_librato_scaling_metrics},
};
use chrono::Local;
use std::{sync::Arc, time::Duration};
use tokio::time::sleep;
use tracing::debug;

/// when sending scaling events as gauge.
/// we have an issue where metrics would report the dyno count as
/// "not reported" or zero between scaling events.
///
/// So we just store the last reported values and then regularly
/// re-send them.
/// due to how tokio works this spawned task won't block the server shutdown.
pub(crate) async fn resend_scaling_events(config: Arc<Config>) {
    loop {
        sleep(Duration::from_secs(10)).await;

        for (_, destination) in config.destinations.iter() {
            let last_scaling_events = destination.last_scaling_events.lock().unwrap();

            let Some(events) = &*last_scaling_events else {
                continue;
            };

            let events: Vec<ScalingEvent<'_>> = events.iter().map(Into::into).collect();
            debug!("resending scaling metrics");

            if let Some(ref librato_client) = destination.librato_client {
                for measurement in
                    generate_librato_scaling_metrics(&Local::now().fixed_offset(), &events)
                {
                    librato_client.add_measurement(measurement);
                }
            }

            if let Some(ref graphite_client) = destination.graphite_client {
                for measurement in
                    generate_graphite_scaling_metrics(&Local::now().fixed_offset(), &events)
                {
                    graphite_client.add_measurement(measurement);
                }
            }
        }
    }
}
