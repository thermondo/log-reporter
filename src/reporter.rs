use anyhow::{anyhow, Context as _, Result};
use std::collections::HashMap;

use axum::http::uri::Uri;
use sentry::{Client, Hub, Level, Scope};
use std::sync::Arc;
use tracing::{info, instrument};

use crate::log_parser::LogLine;

#[instrument(skip(client))]
pub(crate) fn report_to_sentry(
    client: Arc<Client>,
    logline: &LogLine,
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
    scope.set_tag("url", &full_url.to_string()[..200]);

    if let Some(request_id) = items.get("request_id") {
        scope.set_tag("request_id", request_id);
    }

    if let Some(dyno) = items.get("dyno") {
        scope.set_tag("server_name", dyno);
    }

    scope.set_fingerprint(Some(&["heroku-router-request-timeout", full_url.path()]));

    info!(?scope, "reporting timeout to sentry");

    let hub = Hub::new(Some(client), Arc::new(scope));
    let uuid = hub.capture_message(
        &format!("request timeout on {}\n{}", full_url.path(), logline.text),
        Level::Error,
    );
    info!(?uuid, last_event_id = ?hub.last_event_id(), "captured message");
    Ok(())
}
