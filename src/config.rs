use anyhow::{bail, Context as _, Result};
use sentry::transports::DefaultTransportFactory;

use std::{borrow::Cow, collections::HashMap, env, sync::Arc};
use tracing::{debug, info, instrument};

#[derive(Debug)]
pub(crate) struct Config {
    pub port: u16,
    pub sentry_dsn: Option<String>,
    pub sentry_clients: HashMap<String, Arc<sentry::Client>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 3000,
            sentry_dsn: None,
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
    pub(crate) fn with_fake_sentry_client(mut self, logplex_token: &str) -> Config {
        self.sentry_clients.insert(
            logplex_token.into(),
            Arc::new(sentry::Client::from((
                "https://user:pwd@fake_dsn/project",
                sentry::ClientOptions::default(),
            ))),
        );
        self
    }
}
