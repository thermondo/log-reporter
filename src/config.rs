use anyhow::{bail, Context as _, Result};
use sentry::transports::DefaultTransportFactory;
use std::{borrow::Cow, collections::HashMap, env, sync::Arc};
use tracing::{debug, info, instrument};

#[cfg(test)]
use std::future::Future;

// FIXME: Config shouldn't be `Clone`, but I currently need it for the test helpers
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub port: u16,
    pub sentry_dsn: Option<String>,
    pub sentry_debug: bool,
    pub sentry_clients: HashMap<String, Arc<sentry::Client>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 3000,
            sentry_dsn: None,
            sentry_debug: false,
            sentry_clients: HashMap::new(),
        }
    }
}

impl Config {
    #[instrument]
    pub(crate) fn init_from_env() -> Result<Config> {
        debug!("loading config");
        let mut config = Config {
            port: env::var("PORT")
                .unwrap_or_else(|_| "3000".into())
                .parse()
                .context("could not parse PORT")?,
            sentry_dsn: env::var("SENTRY_DSN").ok(),
            sentry_debug: !(env::var("SENTRY_DEBUG")
                .unwrap_or_else(|_| "".into())
                .is_empty()),
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
                    config
                        .sentry_clients
                        .insert(logplex_token.to_owned(), Arc::new(client));

                    info!(
                        ?logplex_token,
                        ?sentry_environment,
                        ?sentry_dsn,
                        "loaded logplex sentry mapping"
                    );
                } else {
                    bail!(
                        "sentry client is not enabled for {} {} {}",
                        logplex_token,
                        sentry_environment,
                        sentry_dsn
                    );
                }
            }
        }

        Ok(config)
    }

    #[cfg(test)]
    pub(crate) async fn with_captured_sentry_events_async<F>(
        mut self,
        logplex_token: &str,
        f: impl FnOnce(Arc<sentry::Client>, Arc<Config>) -> F,
    ) -> Vec<sentry::protocol::Event<'static>>
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
        self.sentry_clients
            .insert(logplex_token.to_owned(), client.clone());

        f(client, Arc::new(self.clone())).await;

        let result = test_transport
            .fetch_and_clear_envelopes()
            .iter()
            .filter_map(|envelope| envelope.event().cloned())
            .collect();

        self.sentry_clients.remove(&logplex_token.to_owned());
        result
    }

    #[cfg(test)]
    pub(crate) fn with_captured_sentry_events_sync(
        self,
        logplex_token: &str,
        f: impl FnOnce(Arc<sentry::Client>, Arc<Config>),
    ) -> Vec<sentry::protocol::Event<'static>> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("can't build runtime");

        runtime.block_on(async move {
            self.with_captured_sentry_events_async(
                logplex_token,
                |cl, cfg| async move { f(cl, cfg) },
            )
            .await
        })
    }
}
