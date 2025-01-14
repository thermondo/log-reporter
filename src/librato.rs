use anyhow::{bail, Result};
use chrono::{DateTime, FixedOffset};
use serde_json::json;
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};
use tracing::error;

const MAX_MEASURE_MEASUREMENTS_PER_REQUEST: usize = 300;
const FLUSH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub(crate) enum Kind {
    Counter,
    Gauge,
}

#[derive(Debug, Clone)]
pub(crate) struct Measurement {
    kind: Kind,
    measure_time: DateTime<FixedOffset>,
    value: f64,
    name: String,
    source: String,
}

struct State {
    queue: Vec<Measurement>,
    last_flush: Instant,
}

pub(crate) struct LibratoClient {
    username: String,
    token: String,
    inner: Mutex<State>,
}

impl LibratoClient {
    pub(crate) fn new(username: impl Into<String>, token: impl Into<String>) -> LibratoClient {
        Self {
            username: username.into(),
            token: token.into(),
            inner: Mutex::new(State {
                queue: Vec::new(),
                last_flush: Instant::now(),
            }),
        }
    }

    pub(crate) fn add_measurement(&self, measurement: Measurement) {
        let mut state = self.inner.lock().unwrap();
        state.queue.push(measurement);

        if state.queue.len() > MAX_MEASURE_MEASUREMENTS_PER_REQUEST
            || state.last_flush.elapsed() > FLUSH_INTERVAL
        {
            tokio::spawn({
                let queue = state.queue.clone();
                let username = self.username.clone();
                let token = self.token.clone();
                async move {
                    if let Err(err) = LibratoClient::send(username, token, queue).await {
                        error!(?err, "error sending metrics to librato");
                    }
                }
            });
            state.last_flush = Instant::now();
            state.queue.clear();
        }
    }

    /// uses old API http://api-docs-archive.librato.com/
    pub(crate) async fn send(
        username: String,
        token: String,
        measurements: Vec<Measurement>,
    ) -> Result<()> {
        let response = reqwest::Client::new()
            .post("https://metrics-api.librato.com/v1/metrics")
            .basic_auth(username, Some(token))
            .json(&json!({
               "gauges": measurements.iter().filter_map(|m| {
                    matches!(m.kind, Kind::Gauge).then(|| {
                        json!({
                            "measure_time": m.measure_time.timestamp(),
                            "name": m.name,
                            "value": m.value,
                            "source": m.source,
                        })
                    })
                }).collect::<Vec<_>>(),
               "counters": measurements.iter().filter_map(|m| {
                    matches!(m.kind, Kind::Counter).then(|| {
                        json!({
                            "measure_time": m.measure_time.timestamp(),
                            "name": m.name,
                            "value": m.value,
                            "source": m.source,
                        })
                    })
                }).collect::<Vec<_>>(),
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            bail!(
                "librato returned an error code {}: {}",
                response.status(),
                response.text().await?
            );
        }

        Ok(())
    }
}
