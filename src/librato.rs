use anyhow::{bail, Result};
use chrono::{DateTime, FixedOffset};
use serde_json::json;
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};
use tracing::error;

const MAX_MEASURE_MEASUREMENTS_PER_REQUEST: usize = 300; // max as per documentation
const FLUSH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub(crate) enum Kind {
    Counter,
    Gauge,
}

#[derive(Debug, Clone)]
pub(crate) struct Measurement {
    pub(crate) kind: Kind,
    pub(crate) measure_time: DateTime<FixedOffset>,
    pub(crate) value: f64,
    pub(crate) name: String,
    pub(crate) source: String,
}

#[derive(Debug)]
struct State {
    queue: Vec<Measurement>,
    last_flush: Instant,
}

#[derive(Debug)]
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

    /// add measurement to the local queue of measurements to be sent.
    /// Will regularly flush the queue and send the measurements to librato
    /// in the background.
    pub(crate) fn add_measurement(&self, measurement: Measurement) {
        let mut state = self.inner.lock().unwrap();
        state.queue.push(measurement);

        if state.queue.len() > MAX_MEASURE_MEASUREMENTS_PER_REQUEST
            || state.last_flush.elapsed() > FLUSH_INTERVAL
        {
            self.trigger_background_flush(&mut state);
        }
    }

    fn trigger_background_flush(&self, state: &mut State) -> tokio::task::JoinHandle<()> {
        let handle = tokio::spawn({
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

        handle
    }

    /// shut down the librato client, sending all pending events to librato.
    pub(crate) async fn shutdown(&self) {
        let mut state = self.inner.lock().unwrap();
        let handle = self.trigger_background_flush(&mut state);
        handle.await.unwrap();
    }

    /// uses old API http://api-docs-archive.librato.com/#create-a-metric
    async fn send(username: String, token: String, measurements: Vec<Measurement>) -> Result<()> {
        let response = reqwest::Client::new()
            .post("https://metrics-api.librato.com/v1/metrics")
            .basic_auth(username, Some(token))
            .json(&json!({
               "gauges": measurements.iter().filter(|m| matches!(m.kind, Kind::Gauge)).map(|m| {
                    json!({
                        "measure_time": m.measure_time.timestamp(),
                        "name": m.name,
                        "value": m.value,
                        "source": m.source,
                    })
                }).collect::<Vec<_>>(),
               "counters": measurements.iter().filter(|m| matches!(m.kind, Kind::Counter)).map(|m| {
                    json!({
                        "measure_time": m.measure_time.timestamp(),
                        "name": m.name,
                        "value": m.value,
                        "source": m.source,
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

impl Drop for LibratoClient {
    /// make sure to flush all pending events to librato before dropping the client.
    /// Can panic if there no available tokio runtime.
    /// If there are no queued events, we'll return immediately to prevent the panic
    /// without runtime when we wouldn't need it anyways.
    fn drop(&mut self) {
        {
            let state = self.inner.lock().unwrap();
            if state.queue.is_empty() {
                return;
            }
        }
        tokio::runtime::Handle::current().block_on(self.shutdown());
    }
}
