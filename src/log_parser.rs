use chrono::{DateTime, FixedOffset};
use nom::{
    branch::alt,
    bytes::complete::{tag, take_till1, take_while1, take_while_m_n},
    character::complete::{char, digit1, multispace0, multispace1, space0, space1, u16},
    combinator::{all_consuming, map, map_res, opt, recognize, rest, value, verify},
    multi::many1,
    sequence::{delimited, preceded, tuple},
    IResult,
};
use std::collections::BTreeMap;
use tracing::instrument;

#[derive(Debug, PartialEq, Clone)]
pub(crate) enum Kind {
    Heroku,
    App,
}

#[derive(Debug, PartialEq)]
pub(crate) struct LogLine<'a> {
    pub timestamp: DateTime<FixedOffset>,
    pub source: &'a str,
    pub kind: Kind,
    pub text: &'a str,
}

pub(crate) type LogMap<'a> = BTreeMap<&'a str, &'a str>;

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
            source,
            kind,
            text,
        },
    )(input)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScalingEvent<'a> {
    pub(crate) proc: &'a str,
    pub(crate) count: u16,
    pub(crate) size: &'a str,
}

impl<'a> From<&'a OwnedScalingEvent> for ScalingEvent<'a> {
    fn from(value: &'a OwnedScalingEvent) -> Self {
        Self {
            proc: &value.proc,
            count: value.count,
            size: &value.size,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct OwnedScalingEvent {
    pub(crate) proc: String,
    pub(crate) count: u16,
    pub(crate) size: String,
}

impl<'a> From<&ScalingEvent<'a>> for OwnedScalingEvent {
    fn from(value: &ScalingEvent<'a>) -> Self {
        Self {
            proc: value.proc.into(),
            count: value.count,
            size: value.size.into(),
        }
    }
}

/// parses heroku scaling events
/// format like:
///     Scaled to web@4:Standard-1X worker@3:Standard-2X by user heroku.hirefire.api@thermondo.de
pub(crate) fn parse_scaling_event(input: &str) -> IResult<&str, (Vec<ScalingEvent>, &str)> {
    map(
        tuple((
            preceded(multispace0, tag("Scaled to")),
            many1(preceded(multispace1, parse_single_scaling_event)),
            preceded(multispace1, tag("by user")),
            preceded(multispace1, rest),
        )),
        |(_, events, _, user)| (events, user),
    )(input)
}

/// parses single scaling element
/// format like:
///     web@4:Standard-1X
fn parse_single_scaling_event(input: &str) -> IResult<&str, ScalingEvent> {
    map(
        tuple((
            take_till1(|c: char| c == '@'),
            tag("@"),
            u16,
            tag(":"),
            take_till1(|c: char| c.is_whitespace()),
        )),
        |(proc, _, count, _, size)| ScalingEvent { proc, count, size },
    )(input)
}

/// parses dyno log messages
/// format like:
///     Error R10 (Boot timeout) -> Web process failed to bind to $PORT within 60 seconds of launch
///
/// see https://devcenter.heroku.com/articles/error-codes#r10-boot-timeout
pub(crate) fn parse_dyno_error_code(input: &str) -> IResult<&str, (&str, &str)> {
    map(
        tuple((
            preceded(multispace0, tag("Error")),
            preceded(space1, take_till1(|c: char| c.is_whitespace())),
            preceded(
                space1,
                delimited(char('('), take_till1(|c: char| c == ')'), char(')')),
            ),
            opt(tuple((space1, tag("->"), rest))),
        )),
        |(_tag, code, name, _arrow)| (code, name),
    )(input)
}

pub(crate) fn parse_key_value_pairs(input: &str) -> IResult<&str, LogMap> {
    map(
        many1(map(
            delimited(
                space0,
                tuple((
                    take_while1(|c: char| c.is_alphanumeric() || c == '-' || c == '_' || c == '#'),
                    tag("="),
                    alt((
                        delimited(tag("\""), take_till1(|c: char| c == '"'), tag("\"")),
                        take_till1(|c: char| c.is_whitespace()),
                    )),
                )),
                space0,
            ),
            |(key, _, value): (&str, &str, &str)| (key, value),
        )),
        |pairs| pairs.into_iter().collect(),
    )(input)
}

pub(crate) fn parse_sfid(input: &str) -> IResult<&str, &str> {
    verify(
        alt((
            all_consuming(take_while_m_n(18, 18, |ch: char| {
                ch.is_ascii_alphanumeric()
            })),
            all_consuming(take_while_m_n(15, 15, |ch: char| {
                ch.is_ascii_alphanumeric()
            })),
        )),
        |sfid: &str| {
            // when the is is all lowercase or all uppercase, it's not an SFID
            // FIXME: the better solution is to _really_ parse the SFID following
            // the salesforce definition.
            !(sfid.chars().all(|ch| ch.is_ascii_lowercase())
                || sfid.chars().all(|ch| ch.is_ascii_uppercase()))
        },
    )(input)
}

/// parse a thermondo project reference
pub(crate) fn parse_project_reference(input: &str) -> IResult<&str, &str> {
    recognize(all_consuming(tuple((
        // the prefix.
        take_while_m_n(2, 2, |ch: char| ch.is_ascii_uppercase()),
        // the year
        take_while_m_n(2, 2, |ch: char| ch.is_ascii_digit()),
        // the counter, base32
        take_while_m_n(4, 4, |ch: char| {
            ch.is_ascii_uppercase() || ch.is_ascii_digit()
        }),
    ))))(input)
}

pub(crate) fn parse_partial_offer_number(input: &str) -> IResult<&str, &str> {
    recognize(tuple((
        take_while1(|ch: char| ch.is_ascii_digit()),
        tag("-"),
        take_while1(|ch: char| ch.is_ascii_digit()),
    )))(input)
}

pub(crate) fn parse_offer_number(input: &str) -> IResult<&str, &str> {
    recognize(all_consuming(parse_partial_offer_number))(input)
}

pub(crate) fn parse_offer_extension_number(input: &str) -> IResult<&str, &str> {
    recognize(all_consuming(tuple((
        parse_partial_offer_number,
        tag("-"),
        take_while1(|ch: char| ch.is_ascii_uppercase()),
    ))))(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("0608656-04")]
    fn test_parse_offer_id(input: &str) {
        let (remainder, result) = parse_offer_number(input).expect("parse error");
        assert!(remainder.is_empty(), "{}", remainder);
        assert_eq!(result, input);
    }

    #[test_case(""; "empty string")]
    #[test_case("0608656-04A"; "letter in offer number")]
    #[test_case("0608656-04-A"; "extension id")]
    #[test_case("060A656-04"; "letter in customer number")]
    #[test_case("-04"; "missing customer number")]
    #[test_case("123456-"; "missing offer number")]
    fn test_parse_offer_id_invalid(input: &str) {
        let result = parse_offer_number(input);
        assert!(result.is_err(), "{:?}", result);
    }

    #[test_case("0608656-04-A")]
    #[test_case("0608656-04-AB")]
    #[test_case("0608656123123123123-04123123123123-ABASLFKAJSLKJDAS")]
    fn test_parse_offer_extension_id(input: &str) {
        let result = parse_offer_extension_number(input);
        assert!(result.is_ok(), "{:?}", result);
        let (remainder, result) = result.expect("parse error");
        assert!(remainder.is_empty(), "{}", remainder);
        assert_eq!(result, input);
    }

    #[test_case(""; "empty string")]
    #[test_case("0608656-04"; "offer id")]
    #[test_case("0608656-04-1"; "number in extension counter")]
    fn test_parse_offer_extension_id_invalid(input: &str) {
        let result = parse_offer_extension_number(input);
        assert!(result.is_err(), "{:?}", result);
    }

    #[test_case("0WO1i000003COEnGAO"; "18 digit id")]
    #[test_case("0WO1i000003COEn"; "15 digit id")]
    // the following are some more real-world examples from the timeout logs
    #[test_case("0WO1i0000029e8EGAQ")]
    #[test_case("0WO1i000003CROHGA4")]
    #[test_case("0WO1i000003CPOKGA4")]
    #[test_case("0WO1i000003CP8qGAG")]
    #[test_case("0WO1i000003COEnGAO")]
    #[test_case("0WO1i000003CNKuGAO")]
    #[test_case("0WO1i000003CNxhGAG")]
    #[test_case("0WO1i000003BtjeGAC")]
    fn test_parse_sfid(input: &str) {
        let (remainder, result) = parse_sfid(input).expect("parse error");
        assert!(remainder.is_empty(), "{}", remainder);
        assert_eq!(result, input);
    }

    #[test_case("0WO1i000003COEnGA"; "length 17")]
    #[test_case("0WO1i000003COEnGABA"; "length 19")]
    #[test_case("0WO1i000003COE"; "length 14")]
    #[test_case("0WO1i000;03COEn"; "non alphanum char")]
    #[test_case("acceptanceprotocol"; "18 digit normal lower case word")]
    #[test_case("predefinedoffer"; "15 digit normal lower case word")]
    #[test_case("ACCEPTANCEPROTOCOL"; "18 digit normal upper case word")]
    #[test_case("PREDEFINEDOFFER"; "15 digit normal upper case word")]
    #[test_case(""; "empty string")]
    fn test_parse_sfid_invalid(input: &str) {
        let result = parse_sfid(input);
        assert!(result.is_err(), "{:?}", result);
    }

    #[test_case("WO220VLD")]
    #[test_case("BV221C02")]
    fn test_project_reference(input: &str) {
        let (remainder, result) = parse_project_reference(input).expect("parse error");
        assert!(remainder.is_empty(), "{}", remainder);
        assert_eq!(result, input);
    }

    #[test_case(""; "empty string")]
    #[test_case("BV221C0"; "too short")]
    #[test_case("BV2X1C00"; "letter in year")]
    #[test_case("1V221C00"; "number in prefix")]
    #[test_case("BV221c02"; "lower case letter in counter")]
    fn test_parse_project_reference_invalid(input: &str) {
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
                source: "router",
                text: "at=info method=GET path=\"/api/disposition/service/?hub=33\" host=thermondo-backend.herokuapp.com request_id=60fbbe6e-0ea5-4013-ab6a-9d6851fe1c95 fwd=\"80.187.107.115,167.82.231.29\" dyno=web.10 connect=2ms service=864ms status=200 bytes=15055 protocol=https"
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
                source: "web.15",
                text: "[r9673 d8512f2b] INFO     [292844f1-49fe-445b-87b3-af87088b7df8] log_request_id.middleware: method=GET path=/api/disposition/foundation/ status=200 user=875",
            });
    }

    #[test]
    fn test_full_boot_timeout_line_info() {
        let input = "
            152 <134>1 2023-04-29T23:11:12.604871+00:00 host heroku web.1 - \
            Error R10 (Boot timeout) -> \
            Web process failed to bind to $PORT within 60 seconds of launch\
            ";
        let (remainder, result) = parse_log_line(input).expect("parse error");
        assert!(remainder.is_empty());
        assert_eq!(
            result,
            LogLine {
                timestamp: DateTime::parse_from_rfc3339("2023-04-29T23:11:12.604871+00:00").unwrap(),
                kind: Kind::Heroku,
                source: "web.1",
                text: "Error R10 (Boot timeout) -> Web process failed to bind to $PORT within 60 seconds of launch",
            });
    }

    #[test]
    fn test_scaling_event_full_line() {
        let input = "
            124 <133>1 2024-05-29T07:07:25.193493+00:00 host app api - \
            Scaled to web@4:Standard-1X by user heroku.hirefire.api@thermondo.de";
        let (remainder, result) = parse_log_line(input).expect("parse error");
        assert!(remainder.is_empty());
        assert_eq!(
            result,
            LogLine {
                timestamp: DateTime::parse_from_rfc3339("2024-05-29T07:07:25.193493+00:00")
                    .unwrap(),
                kind: Kind::App,
                source: "api",
                text: "Scaled to web@4:Standard-1X by user heroku.hirefire.api@thermondo.de",
            }
        );
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
                source: "dramatiqworker.2",
                text: "",
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
            LogMap::from_iter([
                ("at", "info"),
                ("method", "GET",),
                ("path", "/api/disposition/service/?hub=33",),
                ("host", "thermondo-backend.herokuapp.com",),
                ("request_id", "60fbbe6e-0ea5-4013-ab6a-9d6851fe1c95",),
                ("fwd", "80.187.107.115,167.82.231.29",),
                ("dyno", "web.10",),
                ("connect", "2ms",),
                ("service", "864ms",),
                ("status", "200",),
                ("bytes", "15055",),
                ("protocol", "https",),
            ])
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
            LogMap::from_iter([
                ("at", "error"),
                ("code", "H12"),
                ("desc", "Request timeout"),
                ("method", "GET",),
                ("path", "/",),
                ("host", "myapp.herokuapp.com",),
                ("request_id", "8601b555-6a83-4c12-8269-97c8e32cdb22",),
                ("fwd", "204.204.204.204"),
                ("dyno", "web.1",),
                ("connect", "0ms",),
                ("service", "30000ms",),
                ("status", "503",),
                ("bytes", "0",),
                ("protocol", "https",),
            ])
        );
    }

    #[test]
    fn test_pure_text_log_as_key_value_errors() {
        let input: &str = "just some text";
        assert!(parse_key_value_pairs(input).is_err())
    }

    #[test]
    fn test_some_key_value_and_some_remainder() {
        let input: &str = "key=value and some text";

        let (remainder, result) = parse_key_value_pairs(input).expect("parse error");
        assert_eq!(result, LogMap::from_iter([("key", "value")]));
        assert_eq!(remainder, "and some text");
    }

    #[test]
    fn test_key_value_with_dashes_and_some_remainder() {
        let input: &str = "sample#some-key=some-value and some text";

        let (remainder, result) = parse_key_value_pairs(input).expect("parse error");
        assert_eq!(
            result,
            LogMap::from_iter([("sample#some-key", "some-value")])
        );
        assert_eq!(remainder, "and some text");
    }

    #[test]
    fn test_parse_metric_pairs() {
        let input: &str = "source=web.1 dyno=heroku.145151706.12daf639-fefc-4fba-9c12-d0f27c0a4604 sample#memory_total=184.68MB sample#memory_rss=158.27MB";

        let (remainder, result) = parse_key_value_pairs(input).expect("parse error");
        assert!(remainder.is_empty(), "rest: {}", remainder);
        assert_eq!(
            result,
            LogMap::from_iter([
                ("source", "web.1"),
                (
                    "dyno",
                    "heroku.145151706.12daf639-fefc-4fba-9c12-d0f27c0a4604"
                ),
                ("sample#memory_total", "184.68MB"),
                ("sample#memory_rss", "158.27MB")
            ])
        );
    }

    #[test_case("R10", "Boot timeout", "Error R10 (Boot timeout) -> Web process failed to bind to $PORT within 60 seconds of launch")]
    #[test_case(
        "R12",
        "Exit timeout",
        "Error R12 (Exit timeout) -> Process failed to exit within 30 seconds of SIGTERM"
    )]
    #[test_case(
        "R13",
        "Attach error",
        "Error R13 (Attach error) -> Failed to attach to process"
    )]
    #[test_case("R14", "Memory quota exceeded", "Error R14 (Memory quota exceeded)")]
    #[test_case(
        "R15",
        "Memory quota vastly exceeded",
        "Error R15 (Memory quota vastly exceeded)"
    )]
    #[test_case("R16", "Detached", "Error R16 (Detached) -> An attached process is not responding to SIGHUP after its external connection was closed.")]
    #[test_case("R17", "Checksum error", "Error R17 (Checksum error) -> Checksum does match expected value. Expected: SHA256:ed5718e83475c780145609cbb2e4f77ec8076f6f59ebc8a916fb790fbdb1ae64 Actual: SHA256:9ca15af16e06625dfd123ebc3472afb0c5091645512b31ac3dd355f0d8cc42c1")]
    fn test_extract_dyno_error(expected_code: &str, expected_name: &str, line: &str) {
        let (remainder, (code, name)) = parse_dyno_error_code(line).expect("parse error");
        assert!(remainder.is_empty(), "rest: {}", remainder);
        assert_eq!(code, expected_code);
        assert_eq!(name, expected_name);
    }

    #[test_case(
        vec![ScalingEvent {proc: "web", count: 4, size: "Standard-1X"}],
        "heroku.hirefire.api@thermondo.de",
        "Scaled to web@4:Standard-1X by user heroku.hirefire.api@thermondo.de"
    )]
    #[test_case(
        vec![
            ScalingEvent {proc: "celerybeat", count: 1, size: "Standard-1X"},
            ScalingEvent {proc: "celeryworkerhighmemory", count: 1, size: "Performance-M"},
            ScalingEvent {proc: "celeryworkerhighprio", count: 3, size: "Standard-2X"},
            ScalingEvent {proc: "celeryworkerlowprio", count: 1, size: "Performance-M"},
            ScalingEvent {proc: "celeryworkeroffergenerator", count: 1, size: "Performance-L"},
            ScalingEvent {proc: "release", count: 0, size: "Standard-2X"},
            ScalingEvent {proc: "web", count: 5, size: "Performance-M"},
        ],
        "heroku.hirefire.api@thermondo.de",
        "Scaled to \
            celerybeat@1:Standard-1X \
            celeryworkerhighmemory@1:Performance-M \
            celeryworkerhighprio@3:Standard-2X \
            celeryworkerlowprio@1:Performance-M \
            celeryworkeroffergenerator@1:Performance-L \
            release@0:Standard-2X \
            web@5:Performance-M \
            by user heroku.hirefire.api@thermondo.de"
    )]
    fn test_extract_scaling_events(
        expected_events: Vec<ScalingEvent>,
        expected_user: &str,
        line: &str,
    ) {
        let (remainder, (events, user)) = parse_scaling_event(line).expect("parse error");
        assert!(remainder.is_empty(), "rest: {}", remainder);
        assert_eq!(user, expected_user);
        assert_eq!(events, expected_events);
    }
}
