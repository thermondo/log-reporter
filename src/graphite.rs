use anyhow::{Result, bail};
use chrono::{DateTime, FixedOffset};
use crossbeam_utils::sync::WaitGroup;
use std::{
    fmt::Display,
    sync::Mutex,
    time::{Duration, Instant},
};
use tracing::{debug, error};

const FLUSH_INTERVAL: Duration = Duration::from_secs(60);
const FLUSH_AFTER_QUEUE_LENGTH: usize = 100;
#[cfg(not(test))]
const DEFAULT_METRIC_ENDPOINT: &str = "https://www.hostedgraphite.com/api/v1/sink";

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Measurement {
    pub(crate) measure_time: DateTime<FixedOffset>,
    pub(crate) value: f64,
    pub(crate) name: String,
}

impl Display for Measurement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} {}",
            self.name,
            self.value,
            self.measure_time.timestamp()
        )
    }
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

/// graphite client to send measurements to hosted graphite
/// collects metrics in an internal queue and regularly send them to graphite
/// in the background.
/// Using the HTTP API:
/// https://docs.hostedgraphite.com/sending-metrics/supported-protocols#http-post
#[derive(Debug)]
pub(crate) struct Client {
    api_key: String,
    state: Mutex<State>,
    #[cfg(test)]
    endpoint: String,
}

impl Client {
    pub(crate) fn new(
        api_key: impl Into<String>,
        waitgroup: Option<WaitGroup>,
        #[cfg(test)] endpoint: impl Into<String>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            api_key: api_key.into(),
            state: Mutex::new(State {
                waitgroup,
                queue: Vec::with_capacity(FLUSH_AFTER_QUEUE_LENGTH + 1),
                last_flush: Instant::now(),
            }),
            #[cfg(test)]
            endpoint: endpoint.into(),
        })
    }

    /// add measurement to the local queue of measurements to be sent.
    /// Will regularly flush the queue and send the measurements to graphite
    /// in the background.
    pub(crate) fn add_measurement(&self, measurement: Measurement) {
        let mut state = self.state.lock().unwrap();
        state.queue.push(measurement);

        if !(state.last_flush.elapsed() > FLUSH_INTERVAL
            || state.queue.len() > FLUSH_AFTER_QUEUE_LENGTH)
        {
            return;
        }

        debug!(?state.queue, "triggering background flushing to graphite");
        tokio::spawn({
            let queue = state.queue.clone();
            let api_key = self.api_key.clone();
            let waitgroup = state.waitgroup.clone();
            #[cfg(test)]
            let endpoint = self.endpoint.clone();
            async move {
                if let Err(err) = Client::send(
                    &api_key,
                    #[cfg(test)]
                    &endpoint,
                    #[cfg(not(test))]
                    DEFAULT_METRIC_ENDPOINT,
                    &queue,
                )
                .await
                {
                    error!(?err, api_key, ?queue, "error sending metrics to graphite");
                }
                drop(waitgroup);
            }
        });
        state.reset();
    }

    /// shut down the graphite client, sending all pending events to graphite.
    pub(crate) async fn shutdown(&self) -> Result<()> {
        debug!("triggering shutdown of graphite client");
        let queue = {
            let mut state = self.state.lock().unwrap();

            state.waitgroup.take();

            let queue = state.queue.to_vec();
            state.reset();
            queue
        };
        if !queue.is_empty() {
            Client::send(
                &self.api_key,
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

    /// Actually send the measurements to graphite using their HTTP API
    #[tracing::instrument(skip(measurements))]
    async fn send(
        api_key: impl AsRef<str> + std::fmt::Debug,
        endpoint: impl AsRef<str> + std::fmt::Debug,
        measurements: &[Measurement],
    ) -> Result<()> {
        debug!("sending metrics to graphite");

        let mut payload: Vec<u8> = Vec::with_capacity(64 * measurements.len());

        for m in measurements {
            payload.extend_from_slice(m.to_string().as_bytes());
            payload.push(b'\n');
        }

        let response = reqwest::Client::new()
            .post(endpoint.as_ref())
            .basic_auth(api_key.as_ref(), None::<String>)
            .body(payload)
            .send()
            .await?;

        if !response.status().is_success() {
            bail!(
                "graphite returned an error code {}: {}",
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
    async fn test_empty_shutdown() -> anyhow::Result<()> {
        let client = Client::new("api-token", None, "invalid_endpoint")?;

        // shutdown would fail if the client would try to send stuff to graphite
        assert!(client.shutdown().await.is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn test_shutdown_fails_with_queued_measurements() -> Result<()> {
        let client = Client::new("api-token", None, "invalid_endpoint")?;

        client.add_measurement(Measurement {
            measure_time: chrono::Utc::now().into(),
            value: 1.23,
            name: "name".into(),
        });

        assert!(client.shutdown().await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_100_measures_trigger_flush() -> anyhow::Result<()> {
        let mut server = mockito::Server::new_async().await;

        let m = server
            .mock("POST", "/")
            .match_request(move |request| {
                let body = request.body().unwrap();
                let body = String::from_utf8_lossy(body);
                let lines: Vec<_> = body.lines().collect();

                assert_eq!(lines.len(), FLUSH_AFTER_QUEUE_LENGTH + 1);

                true
            })
            .create();

        let client = Client::new("api-token", None, server.url())?;

        // one more measure than FLUSH_AFTER_QUEUE_LENGTH
        for i in 0..(FLUSH_AFTER_QUEUE_LENGTH + 1) {
            client.add_measurement(Measurement {
                measure_time: chrono::Utc::now().into(),
                value: i as f64,
                name: format!("test-{i}"),
            });
        }

        // wait a bit so the background task can finish
        tokio::time::sleep(Duration::from_millis(200)).await;

        drop(client); // doesn't trigger graceful `.shutdown()`

        m.assert_async().await;

        Ok(())
    }

    #[tokio::test]
    async fn test_shutdown_sends_queued_measurements() -> anyhow::Result<()> {
        let timestamp = chrono::Utc::now();
        let mut server = mockito::Server::new_async().await;

        let m = server
            .mock("POST", "/")
            .match_request(move |request| {
                let body = request.body().unwrap();
                let body = String::from_utf8_lossy(body);
                let lines: Vec<_> = body.lines().collect();

                assert_eq!(
                    lines,
                    vec![
                        format!("test 1.23 {}", timestamp.timestamp()),
                        format!("another 3.21 {}", timestamp.timestamp())
                    ]
                );
                true
            })
            .create();

        let client = Client::new("api-token", None, server.url())?;

        client.add_measurement(Measurement {
            measure_time: timestamp.into(),
            value: 1.23,
            name: "test".into(),
        });
        client.add_measurement(Measurement {
            measure_time: timestamp.into(),
            value: 3.21,
            name: "another".into(),
        });

        client.shutdown().await?;
        m.assert_async().await;

        Ok(())
    }
}
