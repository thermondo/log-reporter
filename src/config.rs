use crate::log_parser::OwnedScalingEvent;
use anyhow::Result;
use crossbeam_utils::sync::WaitGroup;
use sentry::transports::DefaultTransportFactory;
use std::{
    borrow::Cow,
    collections::HashMap,
    env,
    sync::{Arc, Mutex, RwLock},
};
use tracing::{debug, error, info, instrument};

#[cfg(test)]
use std::future::Future;

#[derive(Debug)]
pub(crate) struct Destination {
    pub(crate) sentry_client: Arc<sentry::Client>,

    /// store the last seen scaling events so we can re-send them,
    /// assuming that the dyno counts don't change between scaling events.
    pub(crate) last_scaling_events: Mutex<Option<Vec<OwnedScalingEvent>>>,
}

impl Destination {
    pub(crate) fn new(sentry_client: Arc<sentry::Client>) -> Self {
        Self {
            sentry_client,
            last_scaling_events: Mutex::new(None),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub port: u16,
    pub sentry_dsn: Option<String>,
    pub sentry_debug: bool,
    pub sentry_report_metrics: bool,
    pub sentry_traces_sample_rate: f32,
    pub destinations: HashMap<String, Arc<Destination>>,
    /// clone this waitgroup for anything that the app needs to wait
    /// for when shutting down.
    /// See also [`WaitGroup`](crossbeam_utils::sync::WaitGroup).
    waitgroup: Arc<RwLock<Option<WaitGroup>>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 3000,
            sentry_dsn: None,
            sentry_debug: false,
            sentry_report_metrics: false,
            destinations: HashMap::new(),
            waitgroup: Arc::new(RwLock::new(Some(WaitGroup::new()))),
            sentry_traces_sample_rate: 0.0,
        }
    }
}

impl Config {
    /// do the shutdown work for the config or server.
    ///
    /// will
    /// - wait for all running waitgroup tickets
    /// - shut down sentry clients
    pub(crate) fn shutdown(&self) {
        info!(?self.waitgroup, "waiting for pending tasks");

        if let Some(waitgroup) = self.waitgroup.write().unwrap().take() {
            waitgroup.wait();
        }

        info!("flushing sentry events");
        for destination in self.destinations.values() {
            destination.sentry_client.close(None);
        }
    }

    /// Create a new "waitgroup ticket"
    ///
    /// Each background job / task that want to finish on shutdown
    /// should keep one.
    pub(crate) fn new_waitgroup_ticket(&self) -> Option<WaitGroup> {
        self.waitgroup.read().unwrap().clone()
    }

    #[instrument]
    pub(crate) fn init_from_env() -> Result<Config> {
        debug!("loading config");
        let mut config = Config {
            port: env::var("PORT")
                .unwrap_or("".into())
                .parse::<u16>()
                .unwrap_or(3000),
            sentry_dsn: env::var("SENTRY_DSN").ok(),
            sentry_traces_sample_rate: env::var("SENTRY_TRACES_SAMPLE_RATE")
                .unwrap_or("".into())
                .parse::<f32>()
                .unwrap_or(0.0),
            sentry_debug: env::var("SENTRY_DEBUG")
                .map(|var| !var.is_empty())
                .unwrap_or(false),
            sentry_report_metrics: env::var("SENTRY_REPORT_METRICS")
                .map(|var| !var.is_empty())
                .unwrap_or(false),
            ..Default::default()
        };

        for (name, value) in env::vars() {
            if !name.starts_with("SENTRY_MAPPING_") {
                continue;
            }

            let pieces: Vec<_> = value.trim().split('|').collect();
            if let [logplex_token, sentry_environment, sentry_dsn, ..] = pieces[..] {
                let client = sentry::Client::from((
                    sentry_dsn.to_owned(),
                    sentry::ClientOptions {
                        environment: Some(Cow::Owned(sentry_environment.to_owned())),
                        transport: Some(Arc::new(DefaultTransportFactory)),
                        debug: config.sentry_debug,
                        ..Default::default()
                    },
                ));
                if client.is_enabled() {
                    config.destinations.insert(
                        logplex_token.to_owned(),
                        Arc::new(Destination::new(Arc::new(client))),
                    );

                    info!(
                        ?logplex_token,
                        ?sentry_environment,
                        ?sentry_dsn,
                        "loaded logplex sentry mapping"
                    );
                } else {
                    error!(
                        ?logplex_token,
                        ?sentry_environment,
                        ?sentry_dsn,
                        "sentry client is not enabled",
                    );
                }
            } else {
                error!(name, value, "wrong sentry mapping line format.")
            }
        }

        Ok(config)
    }

    #[cfg(test)]
    pub(crate) async fn with_captured_sentry_events_async<F>(
        self,
        logplex_token: &str,
        f: impl FnOnce(Arc<Destination>, Arc<Config>) -> F,
    ) -> Vec<sentry::protocol::Event<'static>>
    where
        F: Future<Output = ()>,
    {
        let test_transport = self
            .with_captured_sentry_transport_async(logplex_token, f)
            .await;
        test_transport
            .fetch_and_clear_envelopes()
            .iter()
            .filter_map(|envelope| envelope.event().cloned())
            .collect()
    }

    #[cfg(test)]
    pub(crate) async fn with_captured_sentry_transport_async<F>(
        mut self,
        logplex_token: &str,
        f: impl FnOnce(Arc<Destination>, Arc<Config>) -> F,
    ) -> Arc<Arc<sentry::test::TestTransport>>
    where
        F: Future<Output = ()>,
    {
        let test_transport = Arc::new(sentry::test::TestTransport::new());
        let client = Arc::new(sentry::Client::from((
            "https://public@example.com/1".to_owned(),
            sentry::ClientOptions {
                transport: Some(test_transport.clone()),
                ..Default::default()
            },
        )));
        let dest = Arc::new(Destination::new(client.clone()));
        self.destinations
            .insert(logplex_token.to_owned(), dest.clone());

        f(dest, Arc::new(self.clone())).await;

        self.destinations.remove(logplex_token);
        test_transport
    }

    #[cfg(test)]
    pub(crate) fn with_captured_sentry_events_sync(
        self,
        logplex_token: &str,
        f: impl FnOnce(Arc<Destination>, Arc<Config>),
    ) -> Vec<sentry::protocol::Event<'static>> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("can't build runtime");

        runtime.block_on(async move {
            self.with_captured_sentry_events_async(logplex_token, |dest, cfg| async move {
                f(dest, cfg)
            })
            .await
        })
    }
}
