//! Talking to ferromeshd: history over HTTP, live traffic over a WebSocket.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use ferromesh_model::{
    DEFAULT_PORT, Event, Frame, HistoryQuery, Kind, MAX_HISTORY_LIMIT, StreamQuery,
};
use futures_util::StreamExt;
use jiff::Timestamp;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::{self, Message};

use crate::render::{self, Printer};

/// The server pings every 30s, so this much silence means a dead connection.
const SILENCE_LIMIT: Duration = Duration::from_secs(75);
const RETRY_MIN: Duration = Duration::from_secs(1);
/// Short, because the server is normally on the LAN and back within seconds.
const RETRY_MAX: Duration = Duration::from_secs(5);

pub struct Server {
    base: String,
}

impl Server {
    /// Accepts a URL (`http://host:port`) or `host[:port]`.
    pub fn new(address: &str) -> Self {
        let address = address.trim_end_matches('/');
        let has_port =
            address.rsplit_once(':').is_some_and(|(_, port)| port.parse::<u16>().is_ok());
        let base = if address.contains("://") {
            address.to_owned()
        } else if has_port {
            format!("http://{address}")
        } else {
            format!("http://{address}:{DEFAULT_PORT}")
        };
        Self { base }
    }

    fn url(&self, path: &str, query: &impl serde::Serialize) -> Result<String> {
        Ok(format!("{}{path}?{}", self.base, serde_urlencoded::to_string(query)?))
    }

    fn stream_url(&self, query: &StreamQuery) -> Result<String> {
        let url = self.url("/api/v1/stream", query)?;
        Ok(match url.split_once("://") {
            Some(("https", rest)) => format!("wss://{rest}"),
            Some((_, rest)) => format!("ws://{rest}"),
            None => url,
        })
    }
}

pub enum Start {
    Live,
    Last(usize),
    Since(Timestamp),
}

pub async fn query(
    server: &Server,
    kind: Kind,
    filter: Option<String>,
    limit: usize,
    (since, until): (Option<Timestamp>, Option<Timestamp>),
    mut printer: Printer,
) -> Result<()> {
    let query =
        HistoryQuery { filter, limit: Some(limit), since, until, ..HistoryQuery::default() };
    let url = server.url(&format!("/api/v1/{kind}"), &query)?;
    let response =
        reqwest::get(&url).await.with_context(|| format!("can't reach {}", server.base))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("{}", error_message(status.as_u16(), &body));
    }
    let mut events: Vec<Event> = response.json().await.context("unexpected response")?;
    events.reverse();
    for event in &events {
        printer.event(event)?;
    }
    if events.len() == limit.min(MAX_HISTORY_LIMIT) {
        render::status(format_args!(
            "showing the newest {}; raise --limit or narrow --since for more",
            events.len()
        ));
    }
    Ok(())
}

/// Follows a stream until interrupted, resuming after the last event seen
/// whenever the connection drops, so nothing is missed or repeated.
pub async fn tail(
    server: &Server,
    kind: Kind,
    filter: Option<String>,
    start: Start,
    mut printer: Printer,
) -> Result<()> {
    let mut resume = None;
    let mut reconnecting = false;
    let mut retry = RETRY_MIN;
    loop {
        let (last, since) = match (resume, &start) {
            (Some(_), _) | (None, Start::Live) => (None, None),
            (None, Start::Last(count)) => (Some(*count), None),
            (None, Start::Since(at)) => (None, Some(*at)),
        };
        let query = StreamQuery { kind, filter: filter.clone(), after: resume, since, last };
        match follow(&server.stream_url(&query)?, &mut printer, &mut resume, reconnecting).await? {
            Ended::Quit => return Ok(()),
            Ended::Lost { reason, was_live } => {
                if was_live {
                    retry = RETRY_MIN;
                }
                render::status(format_args!("{reason}; reconnecting in {}s", retry.as_secs()));
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => return Ok(()),
                    _ = tokio::time::sleep(retry) => {}
                }
                retry = (retry * 2).min(RETRY_MAX);
                reconnecting = true;
            }
        }
    }
}

enum Ended {
    Quit,
    Lost { reason: String, was_live: bool },
}

/// Reads one connection until it ends. Only problems a retry can't fix, such
/// as a rejected filter, come back as errors.
async fn follow(
    url: &str,
    printer: &mut Printer,
    resume: &mut Option<i64>,
    reconnecting: bool,
) -> Result<Ended> {
    let mut socket = match connect_async(url).await {
        Ok((socket, _)) => socket,
        Err(tungstenite::Error::Http(response)) => {
            let status = response.status();
            let body = response.body().as_deref().map(String::from_utf8_lossy).unwrap_or_default();
            let message = error_message(status.as_u16(), &body);
            if status.is_client_error() {
                bail!("{message}");
            }
            return Ok(lost(message, false));
        }
        Err(error) => return Ok(lost(format!("can't connect: {error}"), false)),
    };

    let mut live = false;
    loop {
        let next = tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                let _ = socket.close(None).await;
                return Ok(Ended::Quit);
            }
            next = tokio::time::timeout(SILENCE_LIMIT, socket.next()) => next,
        };
        let message = match next {
            Err(_) => return Ok(lost("the server went quiet", live)),
            Ok(None) => return Ok(lost("the server closed the connection", live)),
            Ok(Some(Err(error))) => return Ok(lost(format!("connection error: {error}"), live)),
            Ok(Some(Ok(message))) => message,
        };
        let Message::Text(text) = message else { continue };
        match serde_json::from_str::<Frame>(text.as_str()).context("unexpected message")? {
            Frame::Event { event } => {
                *resume = Some(event.id());
                printer.event(&event)?;
            }
            Frame::CaughtUp { last_id } => {
                *resume = Some(resume.map_or(last_id, |id| id.max(last_id)));
                if reconnecting {
                    render::status("reconnected");
                } else {
                    printer.live()?;
                }
                live = true;
            }
            Frame::Error { message } => return Ok(lost(format!("server error: {message}"), live)),
        }
    }
}

fn lost(reason: impl Into<String>, was_live: bool) -> Ended {
    Ended::Lost { reason: reason.into(), was_live }
}

fn error_message(status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|json| json.get("error")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| body.trim().to_owned());
    format!("server replied {status}: {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_addresses() {
        assert_eq!(Server::new("truffles.local").base, "http://truffles.local:7373");
        assert_eq!(Server::new("truffles.local:8000").base, "http://truffles.local:8000");
        assert_eq!(Server::new("http://truffles.local:7373/").base, "http://truffles.local:7373");
        assert_eq!(Server::new("[::1]").base, "http://[::1]:7373");
    }

    #[test]
    fn stream_urls() {
        let query = StreamQuery {
            kind: Kind::Messages,
            filter: Some("chan:#test snr>-5".into()),
            after: Some(42),
            since: None,
            last: None,
        };
        assert_eq!(
            Server::new("truffles.local").stream_url(&query).unwrap(),
            "ws://truffles.local:7373/api/v1/stream?kind=messages&filter=chan%3A%23test+snr%3E-5&after=42"
        );
        assert!(
            Server::new("https://mesh.example").stream_url(&query).unwrap().starts_with("wss://")
        );
    }

    #[test]
    fn error_messages() {
        assert_eq!(
            error_message(400, r#"{"error":"snr: doesn't apply to messages"}"#),
            "server replied 400: snr: doesn't apply to messages"
        );
        assert_eq!(error_message(502, "Bad Gateway\n"), "server replied 502: Bad Gateway");
    }
}
