use axum::headers::{Error, Header, HeaderName, HeaderValue};

static LOGPLEX_DRAIN_TOKEN: HeaderName = HeaderName::from_static("logplex-drain-token");

#[derive(Debug)]
pub(crate) struct LogplexDrainToken(String);

impl Header for LogplexDrainToken {
    fn name() -> &'static HeaderName {
        &LOGPLEX_DRAIN_TOKEN
    }

    fn decode<'i, I>(values: &mut I) -> Result<Self, Error>
    where
        I: Iterator<Item = &'i HeaderValue>,
    {
        let value = values.next().ok_or_else(Error::invalid)?;
        Ok(LogplexDrainToken(
            value.to_str().map_err(|_| Error::invalid())?.to_owned(),
        ))
    }

    fn encode<E>(&self, values: &mut E)
    where
        E: Extend<HeaderValue>,
    {
        let value =
            HeaderValue::from_str(&self.0).expect("invalid header value for logplex-drain-token");

        values.extend(std::iter::once(value));
    }
}
