use crate::{config::Destination, log_parser::ScalingEvent};
use anyhow::{bail, Context, Result};
use std::{io::Write, sync::Arc};
use tokio::net::UdpSocket;
use tracing::warn;

#[derive(Debug, PartialEq)]
pub(crate) struct Metric {
    pub(crate) name: String,
    pub(crate) value: f64,
}

/// generate metrics from scaling events
pub(crate) fn generate_scaling_metrics(events: &[ScalingEvent]) -> Vec<Metric> {
    let mut result = Vec::with_capacity(events.len());

    for event in events {
        result.push(Metric {
            name: format!("{}.{}.dyno_count", event.proc, event.size),
            value: event.count as f64,
        })
    }

    result
}

/// report metrics to graphite
/// will return directly and spawn a tokio task for the actual sending.
pub(crate) fn report_metrics<M>(destination: Arc<Destination>, metrics: M)
where
    M: IntoIterator<Item = Metric> + Send + 'static,
{
    tokio::spawn({
        let destination = destination.clone();
        async move {
            if let Err(err) = send_metrics(&destination, metrics).await {
                warn!(?err, "failed to send metrics");
            };
        }
    });
}

async fn send_metrics<'a, M>(destination: &Destination, metrics: M) -> Result<()>
where
    M: IntoIterator<Item = Metric>,
{
    if destination.graphite_api_key.is_none() {
        return Ok(());
    }
    let api_key = destination.graphite_api_key.as_ref().unwrap();

    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("Failed to bind socket")?;

    socket
        .connect("carbon.hostedgraphite.com:2003")
        .await
        .context("can't connect to UDP socket")?;

    let mut buf: Vec<u8> = Vec::with_capacity(8024);

    for metric in metrics {
        write!(buf, "{}.{} {} ", api_key, metric.name, metric.value)?;
        writeln!(buf)?;
    }

    if buf.len() > 65535 {
        bail!("Message exceeds max UDP packet size (65535 bytes)");
    }

    socket.send(&buf).await.context("Failed to send message")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_scaling_metrics() {
        let result = generate_scaling_metrics(&[ScalingEvent {
            proc: "web",
            count: 99,
            size: "huuuuge",
        }]);

        assert_eq!(
            result,
            vec![Metric {
                name: "web.huuuuge.dyno_count".into(),
                value: 99.0,
            },]
        );
    }
}
