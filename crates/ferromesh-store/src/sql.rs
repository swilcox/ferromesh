//! Filters as SQL.
//!
//! Each condition mirrors `Condition::matches` in `ferromesh_model::filter`,
//! using `lower()` for its ASCII-only case folding and wrapping every term in
//! `coalesce(.., 0)` so NULLs count as "no match", as they do in memory.
//! `tests/query.rs` checks the two evaluations agree.

use ferromesh_model::filter::{Comparison, Condition, Measure};
use ferromesh_model::{Filter, Kind};
use rusqlite::types::Value;

/// `AND`-joined SQL fragments and their positional parameters.
#[derive(Debug, Default)]
pub(crate) struct Conditions {
    parts: Vec<String>,
    params: Vec<Value>,
}

impl Conditions {
    pub(crate) fn push(&mut self, sql: String, params: impl IntoIterator<Item = Value>) {
        self.parts.push(sql);
        self.params.extend(params);
    }

    pub(crate) fn sql(&self) -> String {
        if self.parts.is_empty() { "1".to_owned() } else { self.parts.join(" AND ") }
    }

    pub(crate) fn params(&self) -> &[Value] {
        &self.params
    }
}

/// Column expressions per kind, using the aliases in `read.rs`. A term that
/// doesn't apply to a kind is rejected by `Filter::validate` before it gets here.
struct Columns {
    channel: &'static str,
    sender: &'static str,
    body: &'static str,
    payload_type: &'static str,
    node: &'static str,
    observer: &'static str,
    snr: &'static str,
    rssi: &'static str,
    hops: &'static str,
}

const fn columns(kind: Kind) -> Columns {
    match kind {
        Kind::Messages => Columns {
            channel: "c.name",
            sender: "m.sender",
            body: "m.body",
            payload_type: "NULL",
            node: "NULL",
            observer: "NULL",
            snr: "NULL",
            rssi: "NULL",
            hops: "NULL",
        },
        Kind::Packets => Columns {
            channel: "c.name",
            sender: "NULL",
            body: "NULL",
            payload_type: "p.payload_type",
            node: "a.pubkey",
            observer: "NULL",
            snr: "NULL",
            rssi: "NULL",
            hops: "NULL",
        },
        Kind::Observations => Columns {
            channel: "c.name",
            sender: "NULL",
            body: "NULL",
            payload_type: "p.payload_type",
            node: "a.pubkey",
            observer: "coalesce(ob.name, hex(ob.pubkey))",
            snr: "o.snr",
            rssi: "o.rssi",
            hops: "(o.path_len & 63)",
        },
    }
}

pub(crate) fn conditions(kind: Kind, filter: &Filter) -> Conditions {
    let columns = columns(kind);
    let mut conditions = Conditions::default();
    for term in filter.terms() {
        let (sql, params) = condition(&columns, &term.condition);
        let sql = if term.negated {
            format!("NOT coalesce(({sql}), 0)")
        } else {
            format!("coalesce(({sql}), 0)")
        };
        conditions.push(sql, params);
    }
    conditions
}

fn condition(columns: &Columns, condition: &Condition) -> (String, Vec<Value>) {
    match condition {
        Condition::Channel(names) => (
            format!("lower(coalesce({}, '')) IN ({})", columns.channel, placeholders(names.len())),
            text_values(names),
        ),
        Condition::Sender(patterns) => (
            any_of(patterns.len(), &format!("lower(coalesce({}, '')) GLOB ?", columns.sender)),
            patterns.iter().map(|pattern| Value::Text(glob_pattern(pattern))).collect(),
        ),
        Condition::Text(needle) => (
            format!("instr(lower(coalesce({}, '')), ?) > 0", columns.body),
            vec![Value::Text(needle.clone())],
        ),
        Condition::Type(types) => (
            format!("{} IN ({})", columns.payload_type, placeholders(types.len())),
            types.iter().map(|kind| Value::Integer(kind.nibble().into())).collect(),
        ),
        Condition::Observer(names) => (
            format!("lower({}) IN ({})", columns.observer, placeholders(names.len())),
            text_values(names),
        ),
        Condition::Node(prefixes) => (
            any_of(prefixes.len(), &format!("instr(lower(hex({})), ?) = 1", columns.node)),
            text_values(prefixes),
        ),
        Condition::Compare(measure, comparison, bound) => {
            let column = match measure {
                Measure::Snr => columns.snr,
                Measure::Rssi => columns.rssi,
                Measure::Hops => columns.hops,
            };
            let op = match comparison {
                Comparison::Above => ">",
                Comparison::Below => "<",
            };
            (format!("{column} {op} ?"), vec![Value::Real(*bound)])
        }
    }
}

fn placeholders(count: usize) -> String {
    vec!["?"; count].join(", ")
}

fn any_of(count: usize, part: &str) -> String {
    format!("({})", vec![part; count].join(" OR "))
}

fn text_values(values: &[String]) -> Vec<Value> {
    values.iter().cloned().map(Value::Text).collect()
}

/// The filter's patterns only treat `*` as special; escape GLOB's `?` and `[`.
fn glob_pattern(pattern: &str) -> String {
    let mut escaped = String::with_capacity(pattern.len());
    for c in pattern.chars() {
        match c {
            '?' => escaped.push_str("[?]"),
            '[' => escaped.push_str("[[]"),
            c => escaped.push(c),
        }
    }
    escaped
}
