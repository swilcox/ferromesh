//! The filter language.
//!
//! A filter is space-separated terms that must all match:
//!
//! ```text
//! chan:#bna-bot,#bna-wx from:BNABot storm        messages
//! type:advert,grp_txt node:4d1727 -chan:#test    packets
//! observer:Tanyard snr>-5 hops<3                 observations
//! ```
//!
//! Commas list alternatives, a leading `-` negates a term, double quotes allow
//! spaces (`from:"BNA Bot"`), and a bare word means `text:`. In a shell, quote
//! filters that use `>` or `<`.
//!
//! Matching ignores ASCII case only, exactly like SQLite's `lower()`, so the
//! store's SQL for a filter selects the same rows as [`Filter::matches`].

use std::str::FromStr;

use meshcore_proto::PayloadType;

use crate::event::{Event, Kind};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FilterError {
    #[error("unterminated quote")]
    UnterminatedQuote,

    #[error("unknown filter {0:?}")]
    UnknownKey(String),

    #[error("{0}: needs a value")]
    MissingValue(String),

    #[error("{key} doesn't take {op:?}")]
    BadOperator { key: String, op: char },

    #[error("{key}: {value:?} is not a number")]
    BadNumber { key: String, value: String },

    #[error("unknown packet type {0:?}")]
    UnknownType(String),

    #[error("node: {0:?} is not hex")]
    NotHex(String),

    #[error("{key}: doesn't apply to {kind}")]
    NotApplicable { key: &'static str, kind: Kind },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Filter {
    terms: Vec<Term>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Term {
    pub negated: bool,
    pub condition: Condition,
}

/// One test. Text values are stored ASCII-lowercased.
#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    /// `chan:` any of these channel names.
    Channel(Vec<String>),
    /// `from:` any of these sender patterns; `*` matches any run of characters.
    Sender(Vec<String>),
    /// `text:` or a bare word: a substring of the message body.
    Text(String),
    /// `type:` any of these packet types.
    Type(Vec<PayloadType>),
    /// `observer:` any of these observer names.
    Observer(Vec<String>),
    /// `node:` any of these advert public-key prefixes, as hex.
    Node(Vec<String>),
    /// `snr`, `rssi` or `hops` compared with `>` or `<`. A missing value never
    /// satisfies a comparison.
    Compare(Measure, Comparison, f64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measure {
    Snr,
    Rssi,
    Hops,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    Above,
    Below,
}

impl Filter {
    pub fn terms(&self) -> &[Term] {
        &self.terms
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Checks that every term makes sense for `kind`.
    pub fn validate(&self, kind: Kind) -> Result<(), FilterError> {
        match self.terms.iter().find(|term| !term.condition.applies_to(kind)) {
            Some(term) => Err(FilterError::NotApplicable { key: term.condition.key(), kind }),
            None => Ok(()),
        }
    }

    pub fn matches(&self, event: &Event) -> bool {
        self.terms.iter().all(|term| term.condition.matches(event) != term.negated)
    }
}

impl FromStr for Filter {
    type Err = FilterError;

    fn from_str(input: &str) -> Result<Self, FilterError> {
        let terms =
            tokens(input)?.iter().map(|token| parse_term(token)).collect::<Result<_, _>>()?;
        Ok(Self { terms })
    }
}

impl Condition {
    pub const fn key(&self) -> &'static str {
        match self {
            Self::Channel(_) => "chan",
            Self::Sender(_) => "from",
            Self::Text(_) => "text",
            Self::Type(_) => "type",
            Self::Observer(_) => "observer",
            Self::Node(_) => "node",
            Self::Compare(Measure::Snr, ..) => "snr",
            Self::Compare(Measure::Rssi, ..) => "rssi",
            Self::Compare(Measure::Hops, ..) => "hops",
        }
    }

    fn applies_to(&self, kind: Kind) -> bool {
        match self {
            Self::Channel(_) => true,
            Self::Sender(_) | Self::Text(_) => kind == Kind::Messages,
            Self::Type(_) | Self::Node(_) => kind != Kind::Messages,
            Self::Observer(_) | Self::Compare(..) => kind == Kind::Observations,
        }
    }

    fn matches(&self, event: &Event) -> bool {
        match (self, event) {
            (Self::Channel(names), _) => {
                channel_of(event).is_some_and(|channel| names.contains(&lower(channel)))
            }
            (Self::Sender(patterns), Event::Message(message)) => {
                let sender = lower(message.sender.as_deref().unwrap_or_default());
                patterns.iter().any(|pattern| glob_match(pattern, &sender))
            }
            (Self::Text(needle), Event::Message(message)) => {
                lower(&message.body).contains(needle.as_str())
            }
            (Self::Type(types), Event::Packet(packet)) => type_in(types, &packet.payload_type),
            (Self::Type(types), Event::Observation(observation)) => {
                type_in(types, &observation.payload_type)
            }
            (Self::Node(prefixes), Event::Packet(packet)) => {
                has_prefix(packet.advert.as_ref().map(|advert| advert.pubkey.as_str()), prefixes)
            }
            (Self::Node(prefixes), Event::Observation(observation)) => {
                has_prefix(observation.advert_pubkey.as_deref(), prefixes)
            }
            (Self::Observer(names), Event::Observation(observation)) => {
                names.contains(&lower(&observation.observer))
            }
            (Self::Compare(measure, comparison, bound), Event::Observation(observation)) => {
                let value = match measure {
                    Measure::Snr => observation.snr,
                    Measure::Rssi => observation.rssi.map(|rssi| rssi as f64),
                    Measure::Hops => Some(observation.hops.len() as f64),
                };
                value.is_some_and(|value| match comparison {
                    Comparison::Above => value > *bound,
                    Comparison::Below => value < *bound,
                })
            }
            // validate() rejects terms that don't apply to a kind.
            _ => false,
        }
    }
}

fn channel_of(event: &Event) -> Option<&str> {
    match event {
        Event::Message(message) => Some(&message.channel),
        Event::Packet(packet) => packet.channel.as_deref(),
        Event::Observation(observation) => observation.channel.as_deref(),
    }
}

fn type_in(types: &[PayloadType], name: &str) -> bool {
    PayloadType::from_name(name).is_some_and(|kind| types.contains(&kind))
}

fn has_prefix(pubkey: Option<&str>, prefixes: &[String]) -> bool {
    pubkey.is_some_and(|pubkey| {
        let pubkey = lower(pubkey);
        prefixes.iter().any(|prefix| pubkey.starts_with(prefix.as_str()))
    })
}

fn lower(text: &str) -> String {
    text.to_ascii_lowercase()
}

/// `*` matches any run of characters; every other character matches itself.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let (pattern, text): (Vec<char>, Vec<char>) =
        (pattern.chars().collect(), text.chars().collect());
    let (mut p, mut t) = (0, 0);
    let mut backtrack = None;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == '*' {
            backtrack = Some((p, t));
            p += 1;
        } else if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some((star, resume)) = backtrack {
            // Let the last `*` swallow one more character and retry.
            backtrack = Some((star, resume + 1));
            p = star + 1;
            t = resume + 1;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

/// Splits on whitespace outside double quotes, dropping the quotes.
fn tokens(input: &str) -> Result<Vec<String>, FilterError> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let (mut quoted, mut in_token) = (false, false);
    for c in input.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                in_token = true;
            }
            c if c.is_whitespace() && !quoted => {
                if in_token {
                    tokens.push(std::mem::take(&mut current));
                    in_token = false;
                }
            }
            c => {
                current.push(c);
                in_token = true;
            }
        }
    }
    if quoted {
        return Err(FilterError::UnterminatedQuote);
    }
    if in_token {
        tokens.push(current);
    }
    Ok(tokens)
}

fn parse_term(token: &str) -> Result<Term, FilterError> {
    let (negated, body) = match token.strip_prefix('-') {
        Some(rest) if !rest.is_empty() => (true, rest),
        _ => (false, token),
    };
    let key_len = body.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(body.len());
    let (key, rest) = body.split_at(key_len);
    let mut chars = rest.chars();
    let Some(op @ (':' | '>' | '<')) = chars.next().filter(|_| key_len > 0) else {
        // A bare word searches message text.
        return Ok(Term { negated, condition: Condition::Text(lower(body)) });
    };
    let key = lower(key);
    let value = chars.as_str();
    let list = || -> Result<Vec<String>, FilterError> {
        let values: Vec<String> = value.split(',').filter(|v| !v.is_empty()).map(lower).collect();
        if values.is_empty() { Err(FilterError::MissingValue(key.clone())) } else { Ok(values) }
    };
    if value.is_empty() {
        return Err(FilterError::MissingValue(key));
    }

    let condition = match (key.as_str(), op) {
        ("chan" | "channel", ':') => Condition::Channel(list()?),
        ("from" | "sender", ':') => Condition::Sender(list()?),
        ("text", ':') => Condition::Text(lower(value)),
        ("observer", ':') => Condition::Observer(list()?),
        ("type", ':') => Condition::Type(
            list()?
                .iter()
                .map(|name| {
                    PayloadType::from_name(name)
                        .ok_or_else(|| FilterError::UnknownType(name.clone()))
                })
                .collect::<Result<_, _>>()?,
        ),
        ("node", ':') => {
            let prefixes = list()?;
            if let Some(bad) = prefixes.iter().find(|p| !p.chars().all(|c| c.is_ascii_hexdigit())) {
                return Err(FilterError::NotHex(bad.clone()));
            }
            Condition::Node(prefixes)
        }
        ("snr" | "rssi" | "hops", '>' | '<') => {
            let measure = match key.as_str() {
                "snr" => Measure::Snr,
                "rssi" => Measure::Rssi,
                _ => Measure::Hops,
            };
            let comparison = if op == '>' { Comparison::Above } else { Comparison::Below };
            let bound = value.parse::<f64>().ok().filter(|n| n.is_finite()).ok_or_else(|| {
                FilterError::BadNumber { key: key.clone(), value: value.to_owned() }
            })?;
            Condition::Compare(measure, comparison, bound)
        }
        (
            "chan" | "channel" | "from" | "sender" | "text" | "observer" | "type" | "node" | "snr"
            | "rssi" | "hops",
            op,
        ) => return Err(FilterError::BadOperator { key, op }),
        _ => return Err(FilterError::UnknownKey(key)),
    };
    Ok(Term { negated, condition })
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;

    use super::*;
    use crate::event::{MessageEvent, ObservationEvent};

    fn message(channel: &str, sender: Option<&str>, body: &str) -> Event {
        Event::Message(MessageEvent {
            id: 1,
            packet_hash: "00".into(),
            first_seen_at: Timestamp::UNIX_EPOCH,
            channel: channel.into(),
            sender: sender.map(Into::into),
            body: body.into(),
            sender_timestamp: 0,
            txt_type: 0,
            attempt: 0,
            heard: 1,
        })
    }

    fn observation(observer: &str, snr: Option<f64>, hops: usize) -> Event {
        Event::Observation(ObservationEvent {
            id: 1,
            packet_id: 1,
            hash: "00".into(),
            payload_type: "GRP_TXT".into(),
            rx_at: Timestamp::UNIX_EPOCH,
            observer: observer.into(),
            route: "flood".into(),
            hops: vec!["ab".into(); hops],
            snr,
            rssi: None,
            channel: Some("#test".into()),
            advert_pubkey: None,
            advert_name: None,
            text: None,
        })
    }

    fn matches(filter: &str, event: &Event) -> bool {
        filter.parse::<Filter>().unwrap().matches(event)
    }

    #[test]
    fn parses_terms() {
        let filter: Filter = r#"chan:#Test,#wx -from:"BNA Bot" storm"#.parse().unwrap();
        assert_eq!(
            filter.terms(),
            [
                Term {
                    negated: false,
                    condition: Condition::Channel(vec!["#test".into(), "#wx".into()])
                },
                Term { negated: true, condition: Condition::Sender(vec!["bna bot".into()]) },
                Term { negated: false, condition: Condition::Text("storm".into()) },
            ]
        );
    }

    #[test]
    fn parses_types_nodes_and_comparisons() {
        let filter: Filter = "type:advert,GRP_TXT node:4D17 snr>-5 hops<3".parse().unwrap();
        let conditions: Vec<_> = filter.terms().iter().map(|term| &term.condition).collect();
        assert_eq!(
            conditions,
            [
                &Condition::Type(vec![PayloadType::Advert, PayloadType::GrpTxt]),
                &Condition::Node(vec!["4d17".into()]),
                &Condition::Compare(Measure::Snr, Comparison::Above, -5.0),
                &Condition::Compare(Measure::Hops, Comparison::Below, 3.0),
            ]
        );
    }

    #[test]
    fn rejects_bad_input() {
        let error = |input: &str| input.parse::<Filter>().unwrap_err();
        assert_eq!(error("colour:red"), FilterError::UnknownKey("colour".into()));
        assert_eq!(error("chan:"), FilterError::MissingValue("chan".into()));
        assert_eq!(error("chan:,"), FilterError::MissingValue("chan".into()));
        assert_eq!(error("chan>x"), FilterError::BadOperator { key: "chan".into(), op: '>' });
        assert_eq!(error("snr:3"), FilterError::BadOperator { key: "snr".into(), op: ':' });
        assert_eq!(
            error("snr>loud"),
            FilterError::BadNumber { key: "snr".into(), value: "loud".into() }
        );
        assert_eq!(error("type:ping"), FilterError::UnknownType("ping".into()));
        assert_eq!(error("node:xyz"), FilterError::NotHex("xyz".into()));
        assert_eq!(error(r#"from:"BNA"#), FilterError::UnterminatedQuote);
    }

    #[test]
    fn validates_per_kind() {
        let filter: Filter = "chan:#test snr>0".parse().unwrap();
        assert_eq!(filter.validate(Kind::Observations), Ok(()));
        assert_eq!(
            filter.validate(Kind::Messages),
            Err(FilterError::NotApplicable { key: "snr", kind: Kind::Messages })
        );
    }

    #[test]
    fn matches_messages() {
        let bot = message("#test", Some("BNABot"), "Storm warning");
        let anonymous = message("public", None, "hello");
        assert!(matches("chan:#TEST from:bna* storm", &bot));
        assert!(!matches("-chan:#test", &bot));
        assert!(matches("", &anonymous));
        // A missing sender matches like an empty one, in SQL and here.
        assert!(matches("from:*", &anonymous));
        assert!(matches("-from:bnabot", &anonymous));
    }

    #[test]
    fn matches_observations() {
        let loud = observation("Tanyard", Some(-2.5), 3);
        let unknown = observation("Ridge", None, 0);
        assert!(matches("observer:tanyard snr>-5 hops>2 type:grp_txt chan:#test", &loud));
        assert!(!matches("type:advert", &loud));
        assert!(!matches("snr>-5", &unknown));
        assert!(!matches("snr<-5", &unknown));
        assert!(matches("-snr>-5", &unknown));
    }

    #[test]
    fn globs() {
        assert!(glob_match("bna*", "bnabot"));
        assert!(glob_match("*bot", "bnabot"));
        assert!(glob_match("b*b*t", "bnabot"));
        assert!(glob_match("*", ""));
        assert!(!glob_match("bna", "bnabot"));
        assert!(!glob_match("*x*", "bnabot"));
    }
}
