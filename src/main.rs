use std::{
    borrow::Cow,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};
use tracing_subscriber::{prelude::*, EnvFilter};

use anyhow::Result;
use tokio::signal;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::{info, instrument};

use crate::server::get_app;

mod config;
mod extractors;
mod log_parser;
mod server;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let config = config::Config::get();

    let tracing_registry = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(EnvFilter::from_default_env());

    let _sentry_guard = if let Some(sentry_dsn) = &config.sentry_dsn {
        tracing_registry.with(sentry_tracing::layer()).init();
        Some(sentry::init((
            sentry_dsn.clone(),
            sentry::ClientOptions {
                release: std::env::var("HEROKU_RELEASE_VERSION").map(Cow::Owned).ok(),
                attach_stacktrace: true,
                ..Default::default()
            }
            .add_integration(sentry_panic::PanicIntegration::default()),
        )))
    } else {
        tracing_registry.init();
        None
    };

    let app = get_app().layer(
        ServiceBuilder::new()
            .layer(TraceLayer::new_for_http())
            .layer(sentry_tower::NewSentryLayer::new_from_top())
            .layer(sentry_tower::SentryHttpLayer::with_transaction()),
    );

    axum::Server::bind(&SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
        config.port,
    ))
    .serve(app.into_make_service())
    .with_graceful_shutdown(shutdown_signal())
    .await?;

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
