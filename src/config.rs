use crate::{graphite, librato, log_parser::OwnedScalingEvent};
use anyhow::{Context as _, Result, bail};
use crossbeam_utils::sync::WaitGroup;
use sentry::transports::DefaultTransportFactory;
use serde::Deserialize;
use std::{
    borrow::Cow,
    collections::HashMap,
    env,
    sync::{Arc, Mutex, RwLock},
};
use tracing::{debug, error, info, instrument, warn};

#[cfg(test)]
use std::future::Future;

/// parseable settings for a destination.
/// Can be parsed via TOML, or old-style environment variable values.
///
/// TOML format is a multiline TOML table:
///     logplex_token="some-token"
///     sentry_environment="env"
/// the line breaks are important.
///
/// old-style format is:
///     logplex_token|sentry_environment|sentry_dsn|librato_username|librato_token
/// (doesn't support graphite)
#[derive(Deserialize)]
struct DestinationSettings {
    logplex_token: String,
    sentry_environment: String,
    sentry_dsn: String,
    librato_username: Option<String>,
    librato_password: Option<String>,
    graphite_api_key: Option<String>,
}

impl DestinationSettings {
    fn from_environment_line(line: &str) -> Result<Self> {
        let pieces: Vec<_> = line.trim().split('|').collect();
        if pieces.len() < 3 {
            bail!("wrong sentry mapping line format.");
        }

        Ok(Self {
            logplex_token: pieces[0].to_owned(),
            sentry_environment: pieces[1].to_owned(),
            sentry_dsn: pieces[2].to_owned(),
            librato_username: pieces.get(3).map(ToString::to_string),
            librato_password: pieces.get(4).map(ToString::to_string),
            graphite_api_key: None,
        })
    }

    fn from_toml(value: &str) -> Result<Self> {
        toml::from_str(value).context("failed to parse destination settings")
    }
}

#[derive(Debug)]
pub(crate) struct Destination {
    pub(crate) sentry_client: Arc<sentry::Client>,

    pub(crate) librato_client: Option<librato::Client>,
    pub(crate) graphite_client: Option<graphite::Client>,

    /// store the last seen scaling events so we can re-send them,
    /// assuming that the dyno counts don't change between scaling events.
    pub(crate) last_scaling_events: Mutex<Option<Vec<OwnedScalingEvent>>>,
}

impl Destination {
    pub(crate) fn new(
        sentry_client: Arc<sentry::Client>,
        librato_client: Option<librato::Client>,
        graphite_client: Option<graphite::Client>,
    ) -> Self {
        Self {
            sentry_client,
            librato_client,
            graphite_client,
            last_scaling_events: Mutex::new(None),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub port: u16,
    pub sentry_dsn: Option<String>,
    pub sentry_debug: bool,
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
    /// - send pending librato metrics
    /// - send pending graphite metrics
    pub(crate) async fn shutdown(&self) {
        info!("flushing metrics");
        for destination in self.destinations.values() {
            // we have to do this before we wait for the waitgroups,
            // since we might have running background send-to-librato tasks.
            // the shutdown itself won't generate new tasks, so we're fine here.

            if let Some(graphite_client) = &destination.graphite_client {
                if let Err(err) = graphite_client.shutdown().await {
                    error!(?err, "error shutting down graphite client ");
                };
            }
            if let Some(librato_client) = &destination.librato_client {
                if let Err(err) = librato_client.shutdown().await {
                    error!(
                        ?err,
                        librato_client.username, "error shutting down librato client"
                    );
                };
            }
        }

        info!(?self.waitgroup, "waiting for pending background tasks");
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
            ..Default::default()
        };

        for (name, value) in env::vars() {
            if !name.starts_with("SENTRY_MAPPING_") {
                continue;
            }

            let settings = match DestinationSettings::from_toml(&value) {
                Ok(settings) => settings,
                Err(err) => {
                    warn!(?err, value, "couldn't parse destination settings as TOML");

                    DestinationSettings::from_environment_line(&value)
                        .context("couldn't parse destination settings with separators")?
                }
            };

            let client = sentry::Client::from((
                settings.sentry_dsn.to_owned(),
                sentry::ClientOptions {
                    environment: Some(Cow::Owned(settings.sentry_environment.to_owned())),
                    transport: Some(Arc::new(DefaultTransportFactory)),
                    debug: config.sentry_debug,
                    ..Default::default()
                },
            ));

            if !client.is_enabled() {
                error!(
                    ?settings.logplex_token,
                    ?settings.sentry_environment,
                    ?settings.sentry_dsn,
                    "sentry client is not enabled",
                );
                continue;
            }

            let librato_client = if let (Some(username), Some(password)) =
                (settings.librato_username, settings.librato_password)
            {
                info!(username, "configuring librato client");
                Some(librato::Client::new(
                    username.to_string(),
                    password.to_string(),
                    config.new_waitgroup_ticket(),
                    #[cfg(test)]
                    "invalid_endpoint",
                ))
            } else {
                None
            };

            let graphite_client = if let Some(api_key) = settings.graphite_api_key {
                info!("configuring graphite client");
                Some(graphite::Client::new(
                    api_key.to_string(),
                    config.new_waitgroup_ticket(),
                    #[cfg(test)]
                    "invalid_endpoint",
                )?)
            } else {
                None
            };

            config.destinations.insert(
                settings.logplex_token.to_owned(),
                Arc::new(Destination::new(
                    Arc::new(client),
                    librato_client,
                    graphite_client,
                )),
            );

            info!(
                ?settings.logplex_token,
                ?settings.sentry_environment,
                ?settings.sentry_dsn,
                "loaded logplex sentry mapping"
            );
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
        let dest = Arc::new(Destination::new(client.clone(), None, None));
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

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("")]
    #[test_case("1|2")]
    fn test_invalid_old_destination_setting_format(line: &str) {
        assert!(DestinationSettings::from_environment_line(line).is_err());
    }

    #[test]
    fn test_load_old_destination_setting_format_minimal() -> anyhow::Result<()> {
        let settings = DestinationSettings::from_environment_line(
            "logplex_token|sentry_environment|sentry_dsn",
        )?;

        assert_eq!(settings.logplex_token, "logplex_token");
        assert_eq!(settings.sentry_environment, "sentry_environment");
        assert_eq!(settings.sentry_dsn, "sentry_dsn");
        assert!(settings.librato_username.is_none());
        assert!(settings.librato_password.is_none());
        assert!(settings.graphite_api_key.is_none());

        Ok(())
    }

    #[test]
    fn test_load_old_destination_setting_format_max() -> anyhow::Result<()> {
        let settings = DestinationSettings::from_environment_line(
            "logplex_token|sentry_environment|sentry_dsn|librato_username|librato_password",
        )?;

        assert_eq!(settings.logplex_token, "logplex_token");
        assert_eq!(settings.sentry_environment, "sentry_environment");
        assert_eq!(settings.sentry_dsn, "sentry_dsn");
        assert_eq!(
            settings.librato_username.as_deref(),
            Some("librato_username")
        );
        assert_eq!(
            settings.librato_password.as_deref(),
            Some("librato_password")
        );
        assert!(settings.graphite_api_key.is_none());

        Ok(())
    }

    #[test]
    fn test_load_toml_destination_setting_format_minimal() -> anyhow::Result<()> {
        let settings = DestinationSettings::from_toml(
            "logplex_token = \"logplex_token\"
                   sentry_environment = \"sentry_environment\"
                   sentry_dsn = \"sentry_dsn\"",
        )?;

        assert_eq!(settings.logplex_token, "logplex_token");
        assert_eq!(settings.sentry_environment, "sentry_environment");
        assert_eq!(settings.sentry_dsn, "sentry_dsn");
        assert!(settings.librato_username.is_none());
        assert!(settings.librato_password.is_none());
        assert!(settings.graphite_api_key.is_none());

        Ok(())
    }

    #[test]
    fn test_load_toml_destination_setting_format_max() -> anyhow::Result<()> {
        let settings = DestinationSettings::from_toml(
            "logplex_token = \"logplex_token\"
                   sentry_environment = \"sentry_environment\"
                   sentry_dsn = \"sentry_dsn\"
                   librato_username = \"librato_username\"
                   librato_password = \"librato_password\"
                   graphite_api_key = \"graphite_api_key\"",
        )?;

        assert_eq!(settings.logplex_token, "logplex_token");
        assert_eq!(settings.sentry_environment, "sentry_environment");
        assert_eq!(settings.sentry_dsn, "sentry_dsn");
        assert_eq!(
            settings.librato_username.as_deref(),
            Some("librato_username")
        );
        assert_eq!(
            settings.librato_password.as_deref(),
            Some("librato_password")
        );
        assert_eq!(
            settings.graphite_api_key.as_deref(),
            Some("graphite_api_key")
        );

        Ok(())
    }

    #[test]
    fn test_load_toml_destination_setting_format_just_graphite() -> anyhow::Result<()> {
        let settings = DestinationSettings::from_toml(
            "logplex_token = \"logplex_token\"
                   sentry_environment = \"sentry_environment\"
                   sentry_dsn = \"sentry_dsn\"
                   graphite_api_key = \"graphite_api_key\"",
        )?;

        assert_eq!(settings.logplex_token, "logplex_token");
        assert_eq!(settings.sentry_environment, "sentry_environment");
        assert_eq!(settings.sentry_dsn, "sentry_dsn");
        assert!(settings.librato_username.is_none());
        assert!(settings.librato_password.is_none());
        assert_eq!(
            settings.graphite_api_key.as_deref(),
            Some("graphite_api_key")
        );

        Ok(())
    }
}
