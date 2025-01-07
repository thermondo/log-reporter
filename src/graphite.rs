use anyhow::{bail, Context, Result};
use chrono::{DateTime, FixedOffset};
use std::io::Write;
use tokio::net::UdpSocket;

pub(crate) struct Metric<'a> {
    pub(crate) name: &'a str,
    pub(crate) value: f64,
    pub(crate) timestamp: Option<DateTime<FixedOffset>>,
}

pub(crate) async fn send_metrics<'a, M>(apikey: &str, metrics: M) -> Result<()>
where
    M: IntoIterator<Item = Metric<'a>>,
{
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("Failed to bind socket")?;

    socket
        .connect("carbon.hostedgraphite.com:2003")
        .await
        .context("can't connect to UDP socket")?;

    let mut buf: Vec<u8> = Vec::with_capacity(8024);

    for metric in metrics {
        write!(buf, "{}.{} {} ", apikey, metric.name, metric.value)?;
        if let Some(timestamp) = metric.timestamp {
            write!(buf, " {}", timestamp.timestamp())?;
        }
        writeln!(buf)?;
    }

    if buf.len() > 65535 {
        bail!("Message exceeds max UDP packet size (65535 bytes)");
    }

    socket.send(&buf).await.context("Failed to send message")?;
    Ok(())
}
