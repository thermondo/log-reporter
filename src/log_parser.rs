use chrono::{DateTime, FixedOffset};
use nom::{
    branch::alt,
    bytes::complete::{tag, take_till1, take_while1, take_while_m_n},
    character::complete::{digit1, multispace0, space0, space1},
    combinator::{all_consuming, map, map_res, rest, value},
    multi::many1,
    sequence::{delimited, preceded, tuple},
    IResult,
};
use tracing::instrument;

#[derive(Debug, PartialEq, Clone)]
pub(crate) enum Kind {
    Heroku,
    App,
}

#[derive(Debug, PartialEq)]
pub(crate) struct LogLine {
    pub timestamp: DateTime<FixedOffset>,
    pub source: String,
    pub kind: Kind,
    pub text: String,
}

#[instrument]
pub(crate) fn parse_log_line(input: &str) -> IResult<&str, LogLine> {
    map(
        tuple((
            preceded(multispace0, digit1),
            preceded(space1, delimited(tag("<"), digit1, tag(">"))),
            preceded(
                tuple((digit1, space1)),
                map_res(take_till1(|c: char| c.is_whitespace()), |input: &str| {
                    DateTime::parse_from_rfc3339(input)
                }),
            ),
            preceded(space1, tag("host")),
            preceded(
                space1,
                alt((
                    value(Kind::Heroku, tag("heroku")),
                    value(Kind::App, tag("app")),
                )),
            ),
            preceded(space1, take_till1(|c: char| c.is_whitespace())),
            preceded(tuple((space1, tag("-"), space0)), rest),
        )),
        |(_, _, timestamp, _, kind, source, text)| LogLine {
            timestamp,
            source: source.to_owned(),
            kind,
            text: text.to_owned(),
        },
    )(input)
}

pub(crate) fn parse_key_value_pairs(input: &str) -> IResult<&str, Vec<(String, String)>> {
    many1(map(
        delimited(
            space0,
            tuple((
                take_while1(|c: char| c.is_alphanumeric() || c == '_'),
                tag("="),
                alt((
                    delimited(tag("\""), take_till1(|c: char| c == '"'), tag("\"")),
                    take_till1(|c: char| c.is_whitespace()),
                )),
            )),
            space0,
        ),
        |(key, _, value): (&str, &str, &str)| (key.to_owned(), value.to_owned()),
    ))(input)
}

pub(crate) fn parse_sfid(input: &str) -> IResult<&str, &str> {
    alt((
        all_consuming(take_while_m_n(18, 18, |ch: char| {
            ch.is_ascii_alphanumeric()
        })),
        all_consuming(take_while_m_n(15, 15, |ch: char| {
            ch.is_ascii_alphanumeric()
        })),
    ))(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("0WO1i000003COEnGAO"; "18 digit id")]
    #[test_case("0WO1i000003COEn"; "15 digit id")]
    fn test_parse_sfid(input: &str) {
        let (remainder, result) = parse_sfid(input).expect("parse error");
        assert!(remainder.is_empty(), "{}", remainder);
        assert_eq!(result, input);
    }

    #[test_case("0WO1i000003COEnGA"; "length 17")]
    #[test_case("0WO1i000003COEnGABA"; "length 19")]
    #[test_case("0WO1i000003COE"; "length 14")]
    #[test_case("0WO1i000;03COEn"; "non alphanum char")]
    #[test_case(""; "empty string")]
    fn test_parse_sfid_invalid(input: &str) {
        let result = parse_sfid(input);
        assert!(result.is_err(), "{:?}", result);
    }

    #[test]
    fn test_full_router_line_info() {
        let input: &str = "\
            111 <158>1 2022-12-05T08:59:21.850424+00:00 host heroku router - \
            at=info method=GET path=\"/api/disposition/service/?hub=33\" \
            host=thermondo-backend.herokuapp.com \
            request_id=60fbbe6e-0ea5-4013-ab6a-9d6851fe1c95 \
            fwd=\"80.187.107.115,167.82.231.29\" dyno=web.10 \
            connect=2ms service=864ms status=200 bytes=15055 protocol=https\
            ";

        let (remainder, result) = parse_log_line(input).expect("parse error");
        assert!(remainder.is_empty());
        assert_eq!(
            result,
            LogLine {
                timestamp: DateTime::parse_from_rfc3339("2022-12-05T08:59:21.850424+00:00").unwrap(),
                kind: Kind::Heroku,
                source: "router".into(), 
                text: "at=info method=GET path=\"/api/disposition/service/?hub=33\" host=thermondo-backend.herokuapp.com request_id=60fbbe6e-0ea5-4013-ab6a-9d6851fe1c95 fwd=\"80.187.107.115,167.82.231.29\" dyno=web.10 connect=2ms service=864ms status=200 bytes=15055 protocol=https".into()
            });
    }

    #[test]
    fn test_full_web_line_info() {
        // 205 <134>1 2022-12-05T09:51:04.778759+00:00 host heroku web.1 - source=web.1 dyno=heroku.261104379.cd817c77-4f8e-4e68-b42a-3dea4e04d99c sample#load_avg_1m=0.00 sample#load_avg_5m=0.00 sample#load_avg_15m=0.01\n337 <134>1 2022-12-05T09:51:04.835127+00:00 host heroku web.1 - source=web.1 dyno=heroku.261104379.cd817c77-4f8e-4e68-b42a-3dea4e04d99c sample#memory_total=221.47MB sample#memory_rss=217.77MB sample#memory_cache=3.70MB sample#memory_swap=0.00MB sample#memory_pgpgin=149293pages sample#memory_pgpgout=123257pages sample#memory_quota=512.00MB\n
        let input: &str = "\
            111 <190>1 2022-12-05T08:59:21.66229+00:00 host app web.15 - \
            [r9673 d8512f2b] INFO     [292844f1-49fe-445b-87b3-af87088b7df8] \
            log_request_id.middleware: \
            method=GET path=/api/disposition/foundation/ status=200 user=875\
            ";

        let (remainder, result) = parse_log_line(input).expect("parse error");
        assert!(remainder.is_empty());
        assert_eq!(
            result,
            LogLine {
                timestamp: DateTime::parse_from_rfc3339("2022-12-05T08:59:21.66229+00:00").unwrap(),
                kind: Kind::App,
                source: "web.15".into(), 
                text: "[r9673 d8512f2b] INFO     [292844f1-49fe-445b-87b3-af87088b7df8] log_request_id.middleware: method=GET path=/api/disposition/foundation/ status=200 user=875".into(),
            });
    }

    #[test]
    fn test_parse_empty_line() {
        let input: &str = "69 <190>1 2022-12-05T20:26:20.860136+00:00 host app dramatiqworker.2 -";
        let (remainder, result) = parse_log_line(input).expect("parse error");
        assert!(remainder.is_empty());
        assert_eq!(
            result,
            LogLine {
                timestamp: DateTime::parse_from_rfc3339("2022-12-05T20:26:20.860136+00:00")
                    .unwrap(),
                kind: Kind::App,
                source: "dramatiqworker.2".into(),
                text: "".into(),
            }
        );
    }

    #[test]
    fn test_parse_router_log() {
        let input: &str = "\
            at=info method=GET path=\"/api/disposition/service/?hub=33\" \
            host=thermondo-backend.herokuapp.com \
            request_id=60fbbe6e-0ea5-4013-ab6a-9d6851fe1c95 \
            fwd=\"80.187.107.115,167.82.231.29\" dyno=web.10 \
            connect=2ms service=864ms status=200 bytes=15055 protocol=https\
            ";

        let (remainder, result) = parse_key_value_pairs(input).expect("parse error");
        assert!(remainder.is_empty());

        assert_eq!(
            result,
            vec![
                ("at".into(), "info".into()),
                ("method".into(), "GET".into(),),
                ("path".into(), "/api/disposition/service/?hub=33".into(),),
                ("host".into(), "thermondo-backend.herokuapp.com".into(),),
                (
                    "request_id".into(),
                    "60fbbe6e-0ea5-4013-ab6a-9d6851fe1c95".into(),
                ),
                ("fwd".into(), "80.187.107.115,167.82.231.29".into(),),
                ("dyno".into(), "web.10".into(),),
                ("connect".into(), "2ms".into(),),
                ("service".into(), "864ms".into(),),
                ("status".into(), "200".into(),),
                ("bytes".into(), "15055".into(),),
                ("protocol".into(), "https".into(),),
            ]
        );
    }

    #[test]
    fn test_parse_router_timeout_log() {
        let input: &str = "\
            at=error code=H12 desc=\"Request timeout\" method=GET \
            path=/ host=myapp.herokuapp.com \
            request_id=8601b555-6a83-4c12-8269-97c8e32cdb22 \
            fwd=\"204.204.204.204\" dyno=web.1 connect=0ms service=30000ms \
            status=503 bytes=0 protocol=https\
            ";

        let (remainder, result) = parse_key_value_pairs(input).expect("parse error");
        assert!(remainder.is_empty(), "rest: {}", remainder);

        assert_eq!(
            result,
            vec![
                ("at".into(), "error".into()),
                ("code".into(), "H12".into()),
                ("desc".into(), "Request timeout".into()),
                ("method".into(), "GET".into(),),
                ("path".into(), "/".into(),),
                ("host".into(), "myapp.herokuapp.com".into(),),
                (
                    "request_id".into(),
                    "8601b555-6a83-4c12-8269-97c8e32cdb22".into(),
                ),
                ("fwd".into(), "204.204.204.204".into()),
                ("dyno".into(), "web.1".into(),),
                ("connect".into(), "0ms".into(),),
                ("service".into(), "30000ms".into(),),
                ("status".into(), "503".into(),),
                ("bytes".into(), "0".into(),),
                ("protocol".into(), "https".into(),),
            ]
        );
    }
}
