use anyhow::{anyhow, Context as _, Result};
use std::collections::HashMap;

use axum::http::uri::Uri;
use sentry::{Client, Hub, Level, Scope};
use std::sync::Arc;
use tracing::{debug, instrument};

use crate::log_parser::LogLine;

#[instrument]
pub(crate) fn report_to_sentry(
    client: Arc<Client>,
    _logline: &LogLine,
    items: &HashMap<String, String>,
) -> Result<()> {
    let mut scope = Scope::default();
    scope.set_level(Some(Level::Error));

    let path = items
        .get("path")
        .ok_or_else(|| anyhow!("missing path in logline"))?
        .as_str();

    let full_url = Uri::builder()
        .scheme("https")
        .authority(
            items
                .get("host")
                .ok_or_else(|| anyhow!("missing host in logline"))?
                .as_str(),
        )
        .path_and_query(path)
        .build()
        .context("failed building full URL")?;

    scope.set_tag("transaction", full_url.path());
    scope.set_tag("url", &full_url);

    debug!(?path, ?full_url, "reporting timeout to sentry");

    let hub = Hub::new(Some(client), Arc::new(scope));
    hub.capture_message(&format!("request timeout on {}", path), Level::Error);
    hub.client().expect("no client").flush(None);
    Ok(())
}
