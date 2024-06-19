use crate::{
    config::Config,
    log_parser::ScalingEvent,
    metrics::{generate_scaling_metrics, report_metrics},
};
use std::{sync::Arc, time::Duration};
use tokio::time::sleep;
use tracing::debug;

/// when sending scaling events to sentry as gauge,
/// we have an issue where sentry would report the dyno count as
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

            if let Some(events) = &*last_scaling_events {
                let events: Vec<ScalingEvent<'_>> = events.iter().map(Into::into).collect();

                debug!("resending scaling metrics");

                report_metrics(
                    destination,
                    generate_scaling_metrics(&events, "thermondo_log_reporter"),
                );
            }
        }
    }
}
