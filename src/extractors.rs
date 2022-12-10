use axum::headers::{Error, Header, HeaderName, HeaderValue};

pub static LOGPLEX_DRAIN_TOKEN: HeaderName = HeaderName::from_static("logplex-drain-token");

#[derive(Debug, Hash, PartialEq, Eq)]
pub(crate) struct LogplexDrainToken(String);

impl<'a> LogplexDrainToken {
    pub(crate) fn as_str(&'a self) -> &'a str {
        &self.0
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    use axum::headers::HeaderMapExt;
    use axum::http::HeaderMap;

    #[test]
    fn test_encode_logplex_drain_token() {
        let mut map = HeaderMap::new();
        map.typed_insert(LogplexDrainToken("token".into()));
        assert_eq!(map["logplex-drain-token"], "token");
    }

    #[test]
    fn test_decode_logplex_drain_token() {
        let mut map = HeaderMap::new();
        map.append(LogplexDrainToken::name(), "token".parse().unwrap());
        assert_eq!(
            map.typed_get::<LogplexDrainToken>().unwrap(),
            LogplexDrainToken("token".into())
        );
    }
}
