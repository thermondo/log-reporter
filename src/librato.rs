use anyhow::{bail, Result};
use chrono::{DateTime, FixedOffset};
use crossbeam_utils::sync::WaitGroup;
use serde_json::json;
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};
use tracing::{debug, error};

const MAX_MEASURE_MEASUREMENTS_PER_REQUEST: usize = 300; // max as per documentation
const FLUSH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub(crate) enum Kind {
    #[allow(dead_code)]
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
    waitgroup: Option<WaitGroup>,
}

impl State {
    fn reset(&mut self) {
        self.queue.clear();
        self.last_flush = Instant::now();
    }
}

#[derive(Debug)]
pub(crate) struct LibratoClient {
    pub(crate) username: String,
    token: String,
    inner: Mutex<State>,
}

impl LibratoClient {
    pub(crate) fn new(
        username: impl Into<String>,
        token: impl Into<String>,
        waitgroup: Option<WaitGroup>,
    ) -> LibratoClient {
        Self {
            username: username.into(),
            token: token.into(),
            inner: Mutex::new(State {
                waitgroup,
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
            debug!(?state.queue, "triggering background flushing to librato");
            tokio::spawn({
                let queue = state.queue.clone();
                let username = self.username.clone();
                let token = self.token.clone();
                let waitgroup = state.waitgroup.clone();
                async move {
                    if let Err(err) = LibratoClient::send(&username, &token, &queue).await {
                        error!(?err, username, ?queue, "error sending metrics to librato");
                    }
                    drop(waitgroup);
                }
            });
            state.reset();
        }
    }

    /// shut down the librato client, sending all pending events to librato.
    pub(crate) async fn shutdown(&self) -> Result<()> {
        debug!("triggering shutdown of librato client");
        let mut state = self.inner.lock().unwrap();
        if !state.queue.is_empty() {
            LibratoClient::send(&self.username, &self.token, &state.queue).await?;
            state.reset();
        }
        state.waitgroup.take();
        Ok(())
    }

    /// Actually send the measurements to librato using their API.
    /// uses old source-based API, since that's what the Heroku addon instances use.
    /// See http://api-docs-archive.librato.com/#create-a-metric
    #[tracing::instrument(skip(token, measurements))]
    async fn send(
        username: impl AsRef<str> + std::fmt::Debug,
        token: impl AsRef<str> + std::fmt::Debug,
        measurements: &[Measurement],
    ) -> Result<()> {
        debug!("making API call to librato");
        let response = reqwest::Client::new()
            .post("https://metrics-api.librato.com/v1/metrics")
            .basic_auth(username.as_ref(), Some(token.as_ref()))
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
    /// Can panic if
    /// - there no available tokio runtime.
    /// - there is an error sending the events to librato.
    /// If there are no queued events, we'll return immediately to prevent the panic
    /// without runtime when we wouldn't need it anyways.
    ///
    /// Generally it's better to call `shutdown` explicitly.
    fn drop(&mut self) {
        {
            let state = self.inner.lock().unwrap();
            if state.queue.is_empty() {
                return;
            }
        }
        tokio::runtime::Handle::current()
            .block_on(self.shutdown())
            .expect("error sending metrics");
    }
}
