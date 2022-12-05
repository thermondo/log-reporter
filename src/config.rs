use once_cell::sync::OnceCell;
use std::env;

pub(crate) struct Config {
    pub port: u16,
    pub sentry_dsn: Option<String>,
}

impl Config {
    pub(crate) fn get() -> &'static Config {
        static CONFIG: OnceCell<Config> = OnceCell::new();
        CONFIG.get_or_init(|| Config {
            port: env::var("PORT")
                .map(|p| p.parse().expect("could not parse PORT"))
                .unwrap_or(3000),
            sentry_dsn: env::var("SENTRY_DSN").ok(),
        })
    }
}
