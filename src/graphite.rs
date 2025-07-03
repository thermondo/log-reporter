use anyhow::{Result, anyhow};
use chrono::{DateTime, FixedOffset};
use crossbeam_utils::sync::WaitGroup;
use std::{
    fmt::Display,
    net::ToSocketAddrs,
    sync::Mutex,
    time::{Duration, Instant},
};
use tokio::net::UdpSocket;

use tracing::{debug, error};

const MAX_BYTES_PER_UDP_REQUEST: usize = 8192; // max UDP packet size
const FLUSH_INTERVAL: Duration = Duration::from_secs(60);

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
#[derive(Debug)]
pub(crate) struct Client {
    api_key: String,
    state: Mutex<State>,
}

impl Client {
    pub(crate) fn new(api_key: impl Into<String>, waitgroup: Option<WaitGroup>) -> Client {
        Self {
            api_key: api_key.into(),
            state: Mutex::new(State {
                waitgroup,
                queue: Vec::new(),
                last_flush: Instant::now(),
            }),
        }
    }

    /// add measurement to the local queue of measurements to be sent.
    /// Will regularly flush the queue and send the measurements to graphite
    /// in the background.
    pub(crate) fn add_measurement(&self, measurement: Measurement) {
        let mut state = self.state.lock().unwrap();
        state.queue.push(measurement);

        if state.last_flush.elapsed() <= FLUSH_INTERVAL {
            return;
        }

        debug!(?state.queue, "triggering background flushing to graphite");
        tokio::spawn({
            let queue = state.queue.clone();
            let api_key = self.api_key.clone();
            let waitgroup = state.waitgroup.clone();
            async move {
                if let Err(err) = Client::send(&api_key, &queue).await {
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
            Client::send(&self.api_key, &queue).await?;
        }
        Ok(())
    }

    /// Actually send the measurements to graphite using their API.
    #[tracing::instrument(skip(measurements))]
    async fn send(
        api_key: impl AsRef<str> + std::fmt::Debug,
        measurements: &[Measurement],
    ) -> Result<()> {
        debug!("sending metrics to graphite");

        let socket = UdpSocket::bind("0.0.0.0:0").await?;

        let target_addr = ("carbon.hostedgraphite.com", 2003)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow!("could not resolve carbon.hostedgraphite.com"))?;

        socket.connect(target_addr).await?;

        let mut buf: Vec<u8> = Vec::with_capacity(MAX_BYTES_PER_UDP_REQUEST);

        for m in measurements {
            let next = format!("{}.{}\n", api_key.as_ref(), m);

            if buf.len() + next.len() > MAX_BYTES_PER_UDP_REQUEST {
                socket
                    .send(&buf)
                    .await
                    .map_err(|e| anyhow!("failed to send measurement: {:?}", e))?;

                buf.clear();
            }

            buf.extend_from_slice(format!("{}.{}\n", api_key.as_ref(), m).as_bytes());
        }

        if !buf.is_empty() {
            socket
                .send(&buf)
                .await
                .map_err(|e| anyhow!("failed to send measurements: {:?}", e))?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // use super::*;

    // #[tokio::test]
    // async fn test_empty_shutdown() {
    //     let client = Client::new("username", "token", None, "invalid_endpoint");

    //     assert!(client.shutdown().await.is_ok());
    // }

    // #[tokio::test]
    // async fn test_shutdown_fails_with_queued_measurements() {
    //     let client = Client::new("username", "token", None, "invalid_endpoint");
    //     client.add_measurement(Measurement {
    //         kind: Kind::Gauge,
    //         measure_time: chrono::Utc::now().into(),
    //         value: 1.0,
    //         name: "test".into(),
    //         source: "test".into(),
    //     });

    //     assert!(client.shutdown().await.is_err());
    // }

    // #[tokio::test]
    // async fn test_full_send() -> Result<()> {
    //     let timestamp = chrono::Utc::now();

    //     let mut server = mockito::Server::new_async().await;

    //     let m = server
    //         .mock("POST", "/")
    //         .match_request(move |request| {
    //             let body: serde_json::Value =
    //                 serde_json::from_slice(request.body().unwrap()).unwrap();
    //             body == serde_json::json!({
    //                 "counters": [],
    //                 "gauges": [
    //                     {
    //                         "measure_time": timestamp.timestamp(),
    //                         "name": "testname",
    //                         "source": "testsource",
    //                         "value": 42.0
    //                     }
    //                 ]
    //             })
    //         })
    //         .create();

    //     let client = Client::new("username", "token", None, server.url());
    //     client.add_measurement(Measurement {
    //         kind: Kind::Gauge,
    //         measure_time: timestamp.into(),
    //         value: 42.0,
    //         name: "testname".into(),
    //         source: "testsource".into(),
    //     });

    //     client.shutdown().await?;

    //     m.assert_async().await;
    //     Ok(())
    // }
}
