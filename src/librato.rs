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
#[cfg(not(test))]
const DEFAULT_METRIC_ENDPOINT: &str = "https://metrics-api.librato.com/v1/metrics";

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Kind {
    #[allow(dead_code)]
    Counter,
    Gauge,
}

#[derive(Debug, Clone, PartialEq)]
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
pub(crate) struct Client {
    pub(crate) username: String,
    token: String,
    #[cfg(test)]
    endpoint: String,
    inner: Mutex<State>,
}

impl Client {
    pub(crate) fn new(
        username: impl Into<String>,
        token: impl Into<String>,
        waitgroup: Option<WaitGroup>,
        #[cfg(test)] endpoint: impl Into<String>,
    ) -> Client {
        Self {
            username: username.into(),
            token: token.into(),
            #[cfg(test)]
            endpoint: endpoint.into(),
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
                #[cfg(test)]
                let endpoint = self.endpoint.clone();
                let waitgroup = state.waitgroup.clone();
                async move {
                    if let Err(err) = Client::send(
                        &username,
                        &token,
                        #[cfg(test)]
                        &endpoint,
                        #[cfg(not(test))]
                        DEFAULT_METRIC_ENDPOINT,
                        &queue,
                    )
                    .await
                    {
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
        let queue = {
            let mut state = self.inner.lock().unwrap();
            state.waitgroup.take();
            let queue = state.queue.to_vec();
            state.reset();
            queue
        };
        if !queue.is_empty() {
            Client::send(
                &self.username,
                &self.token,
                #[cfg(test)]
                &self.endpoint,
                #[cfg(not(test))]
                DEFAULT_METRIC_ENDPOINT,
                &queue,
            )
            .await?;
        }
        Ok(())
    }

    /// Actually send the measurements to librato using their API.
    /// uses old source-based API, since that's what the Heroku addon instances use.
    /// See http://api-docs-archive.librato.com/#create-a-metric
    #[tracing::instrument(skip(token, measurements))]
    async fn send(
        username: impl AsRef<str> + std::fmt::Debug,
        token: impl AsRef<str> + std::fmt::Debug,
        endpoint: impl AsRef<str> + std::fmt::Debug,
        measurements: &[Measurement],
    ) -> Result<()> {
        debug!("making API call to librato");
        let response = reqwest::Client::new()
            .post(endpoint.as_ref())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_empty_shutdown() {
        let client = Client::new("username", "token", None, "invalid_endpoint");

        assert!(client.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn test_shutdown_fails_with_queued_measurements() {
        let client = Client::new("username", "token", None, "invalid_endpoint");
        client.add_measurement(Measurement {
            kind: Kind::Gauge,
            measure_time: chrono::Utc::now().into(),
            value: 1.0,
            name: "test".into(),
            source: "test".into(),
        });

        assert!(client.shutdown().await.is_err());
    }

    #[tokio::test]
    async fn test_full_send() -> Result<()> {
        let timestamp = chrono::Utc::now();

        let mut server = mockito::Server::new_async().await;

        let m = server
            .mock("POST", "/")
            .match_request(move |request| {
                let body: serde_json::Value =
                    serde_json::from_slice(request.body().unwrap()).unwrap();
                body == serde_json::json!({
                    "counters": [],
                    "gauges": [
                        {
                            "measure_time": timestamp.timestamp(),
                            "name": "testname",
                            "source": "testsource",
                            "value": 42.0
                        }
                    ]
                })
            })
            .create();

        let client = Client::new("username", "token", None, server.url());
        client.add_measurement(Measurement {
            kind: Kind::Gauge,
            measure_time: timestamp.into(),
            value: 42.0,
            name: "testname".into(),
            source: "testsource".into(),
        });

        client.shutdown().await?;

        m.assert_async().await;
        Ok(())
    }
}
