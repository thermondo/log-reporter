use crate::server::build_app;
use anyhow::Result;
use sentry::integrations::{
    panic as sentry_panic, tower as sentry_tower, tracing as sentry_tracing,
};
use std::{
    borrow::Cow,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};
use tokio::{net::TcpListener, signal};
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::{info, instrument};
use tracing_subscriber::{EnvFilter, prelude::*};

mod background;
mod config;
mod extractors;
mod librato;
mod log_parser;
mod metrics;
mod reporter;
mod server;
#[cfg(test)]
mod test_utils;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let config = Arc::new(config::Config::init_from_env()?);
    info!(?config, "config loaded");

    let heroku_release = std::env::var("HEROKU_RELEASE_VERSION").ok();

    let mut tracing_subscriber_layer = tracing_subscriber::fmt::layer();

    if heroku_release.is_some() {
        // we don't want ansi colors on heroku since logentries doesnt understand them.
        tracing_subscriber_layer = tracing_subscriber_layer.with_ansi(false);
    }

    let tracing_registry = tracing_subscriber::registry()
        .with(tracing_subscriber_layer)
        .with(EnvFilter::from_default_env());

    let _sentry_guard = if let Some(sentry_dsn) = &config.sentry_dsn {
        tracing_registry.with(sentry_tracing::layer()).init();
        Some(sentry::init((
            sentry_dsn.clone(),
            sentry::ClientOptions {
                release: heroku_release.map(Cow::Owned),
                attach_stacktrace: true,
                debug: config.sentry_debug,
                traces_sample_rate: config.sentry_traces_sample_rate,
                ..Default::default()
            }
            .add_integration(sentry_panic::PanicIntegration::default()),
        )))
    } else {
        tracing_registry.init();
        None
    };

    info!("starting background task: resend scaling events");
    tokio::spawn(background::resend_scaling_events(config.clone()));

    let port = config.port;
    let app = build_app(config.clone()).layer(
        ServiceBuilder::new()
            .layer(TraceLayer::new_for_http())
            .layer(sentry_tower::NewSentryLayer::new_from_top())
            .layer(sentry_tower::SentryHttpLayer::new().enable_transaction()),
    );

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), port);
    info!(?addr, "starting server");

    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    config.shutdown().await;

    Ok(())
}

#[instrument]
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("signal received, starting graceful shutdown");
}
