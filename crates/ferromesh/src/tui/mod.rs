//! `ferromesh tui`: a full-screen view of live traffic.
//!
//! Three WebSocket streams (messages, packets, observations) feed one [`App`];
//! channel and node lists are fetched once a minute, and packet details and
//! older history on request.

mod app;
mod emoji;
mod inspect;
mod lists;
mod overlay;
mod ui;

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
use ferromesh_model::{
    AdvertRequest, AdvertSent, ChannelInfo, DirectMessageInfo, Event, HistoryQuery, Kind,
    MAX_NODES, NodeInfo, ObserverHealth, PacketDetail, PinRequest, RadioContact, SendRequest,
    SentMessageInfo,
};
use futures_util::StreamExt;
use jiff::Timestamp;
use ratatui::backend::TestBackend;
use ratatui::{DefaultTerminal, Terminal};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinSet;

use self::app::{App, Command, OLDER_BATCH, Update};
use self::ui::Screen;
use crate::config;
use crate::server::{self, Server, Sink, Start};

/// The history each stream starts with.
const HISTORY: [(Kind, usize); 3] =
    [(Kind::Messages, 1000), (Kind::Packets, 500), (Kind::Observations, 2000)];
/// How often the channel and node lists are fetched.
const REFRESH: Duration = Duration::from_secs(60);
/// How often direct messages are fetched: often enough to hold a
/// conversation, since they don't come down the event streams.
const DM_REFRESH: Duration = Duration::from_secs(10);
/// Direct messages kept in the DM view, sent and received.
const DM_HISTORY: usize = 500;
/// How long a snapshot waits for the server.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(20);

/// Render one screen as text instead of running interactively.
pub struct Snapshot {
    pub width: u16,
    pub height: u16,
    /// Pressed, in order, once everything has loaded.
    pub keys: Vec<KeyEvent>,
}

/// `token` is the server's API token, which sending needs.
pub async fn run(server: Server, token: Option<String>, snapshot: Option<Snapshot>) -> Result<()> {
    let dir = config::dir();
    let watches = match &dir {
        Some(dir) => config::load_watches(dir)?,
        None => Vec::new(),
    };
    let app = App::new(server.address().to_owned(), watches);
    let server = Arc::new(server);
    let (updates, received) = mpsc::unbounded_channel();
    let mut streams = JoinSet::new();
    for (kind, last) in HISTORY {
        streams.spawn(stream(server.clone(), kind, last, updates.clone()));
    }
    let result = match snapshot {
        // A snapshot never writes the watch file.
        Some(snapshot) => {
            let network = Network { server, updates, dir: None, token };
            snap(app, &network, received, snapshot).await
        }
        None => interactive(app, &Network { server, updates, dir, token }, received).await,
    };
    streams.abort_all();
    result
}

async fn interactive(
    mut app: App,
    network: &Network,
    mut received: UnboundedReceiver<Update>,
) -> Result<()> {
    let mut terminal = ratatui::try_init().context("starting the terminal UI")?;
    let result = event_loop(&mut terminal, &mut app, network, &mut received).await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    network: &Network,
    received: &mut UnboundedReceiver<Update>,
) -> Result<()> {
    let mut screen = Screen::default();
    let mut keys = EventStream::new();
    let mut refresh = tokio::time::interval(REFRESH);
    let mut dms = tokio::time::interval(DM_REFRESH);
    let mut clock = tokio::time::interval(Duration::from_secs(1));
    while !app.quit {
        app.now = Timestamp::now();
        terminal.draw(|frame| ui::draw(frame, app, &mut screen))?;
        if app.take_bell() {
            let mut out = io::stdout();
            out.write_all(b"\x07")?;
            out.flush()?;
        }
        tokio::select! {
            key = keys.next() => match key {
                Some(key) => {
                    for command in app.handle(key?) {
                        network.run(command);
                    }
                }
                None => break,
            },
            Some(update) = received.recv() => {
                app.apply(update);
                while let Ok(update) = received.try_recv() {
                    app.apply(update);
                }
            }
            _ = refresh.tick() => network.refresh(),
            _ = dms.tick() => network.refresh_dms(),
            _ = clock.tick() => {}
        }
    }
    Ok(())
}

async fn snap(
    mut app: App,
    network: &Network,
    mut received: UnboundedReceiver<Update>,
    snapshot: Snapshot,
) -> Result<()> {
    let work = async {
        fetch_lists(&network.server, &network.updates).await;
        settle(&mut app, &mut received).await;
        for key in snapshot.keys {
            for command in app.handle(TermEvent::Key(key)) {
                network.run(command);
            }
            settle(&mut app, &mut received).await;
        }
    };
    tokio::time::timeout(SNAPSHOT_TIMEOUT, work)
        .await
        .map_err(|_| anyhow!("timed out waiting for {}", network.server.address()))?;

    app.now = Timestamp::now();
    let mut terminal = Terminal::new(TestBackend::new(snapshot.width, snapshot.height))?;
    terminal.draw(|frame| ui::draw(frame, &app, &mut Screen::default()))?;
    let mut out = io::stdout().lock();
    for line in ui::text_lines(terminal.backend().buffer()) {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// Applies updates until nothing is loading.
async fn settle(app: &mut App, received: &mut UnboundedReceiver<Update>) {
    loop {
        while let Ok(update) = received.try_recv() {
            app.apply(update);
        }
        if app.idle() {
            return;
        }
        match received.recv().await {
            Some(update) => app.apply(update),
            None => return,
        }
    }
}

async fn stream(server: Arc<Server>, kind: Kind, last: usize, updates: UnboundedSender<Update>) {
    let mut sink = Forward { kind, updates: updates.clone() };
    if let Err(error) = server::tail(&server, kind, None, Start::Last(last), &mut sink).await {
        let _ = updates.send(Update::Lost(kind, format!("{error:#}")));
    }
}

/// Passes one stream's traffic to the app.
struct Forward {
    kind: Kind,
    updates: UnboundedSender<Update>,
}

impl Forward {
    fn send(&self, update: Update) -> Result<()> {
        self.updates.send(update).map_err(|_| anyhow!("the terminal UI has closed"))
    }
}

impl Sink for Forward {
    fn event(&mut self, event: Event) -> Result<()> {
        self.send(Update::Event(event))
    }

    fn caught_up(&mut self, _reconnected: bool) -> Result<()> {
        self.send(Update::CaughtUp(self.kind))
    }

    fn lost(&mut self, reason: &str, retry: Duration) -> Result<()> {
        self.send(Update::Lost(self.kind, format!("{reason}; retrying in {}s", retry.as_secs())))
    }
}

struct Network {
    server: Arc<Server>,
    updates: UnboundedSender<Update>,
    /// Where watches are saved, if anywhere.
    dir: Option<PathBuf>,
    token: Option<String>,
}

impl Network {
    fn run(&self, command: Command) {
        let (server, updates) = (self.server.clone(), self.updates.clone());
        match command {
            Command::Inspect(hash) => {
                tokio::spawn(async move {
                    let detail = server
                        .get::<PacketDetail>(&format!("/api/v1/packets/{hash}"))
                        .await
                        .map_err(|error| format!("{error:#}"));
                    let _ = updates.send(Update::Detail(hash, detail));
                });
            }
            Command::Older { kind, before, filter } => {
                tokio::spawn(async move {
                    let query = HistoryQuery {
                        filter: filter.clone(),
                        limit: Some(OLDER_BATCH),
                        before: Some(before),
                        ..HistoryQuery::default()
                    };
                    let result = match server.path(&format!("/api/v1/{kind}"), &query) {
                        Ok(path) => server.get::<Vec<Event>>(&path).await,
                        Err(error) => Err(error),
                    };
                    let result = result.map_err(|error| format!("{error:#}"));
                    let _ = updates.send(Update::Older { kind, filter, result });
                });
            }
            Command::SaveWatches(watches) => {
                if let Some(dir) = &self.dir
                    && let Err(error) = config::save_watches(dir, &watches)
                {
                    let _ =
                        updates.send(Update::Status(format!("couldn't save watches: {error:#}")));
                }
            }
            Command::Pin { to, pinned } => {
                let token = self.token.clone();
                let updates_after = updates.clone();
                tokio::spawn(async move {
                    let request = PinRequest { to: to.clone(), pinned };
                    let result: Result<RadioContact> =
                        server.post("/api/v1/contacts", &request, token.as_deref()).await;
                    let status = match (result, pinned) {
                        (Ok(contact), true) => format!("the radio keeps {}", contact.name),
                        (Ok(contact), false) => {
                            format!("{} can be replaced when the radio needs room", contact.name)
                        }
                        (Err(error), _) => format!("couldn't change {to}: {error:#}"),
                    };
                    let _ = updates.send(Update::Status(status));
                    fetch_contacts(&server, &updates_after).await;
                });
            }
            Command::Advert { flood } => {
                let token = self.token.clone();
                tokio::spawn(async move {
                    let request = AdvertRequest { flood };
                    let result: Result<AdvertSent> =
                        server.post("/api/v1/advert", &request, token.as_deref()).await;
                    let status = match result {
                        Ok(sent) if sent.flood => {
                            format!("{} advertised across the mesh", sent.name)
                        }
                        Ok(sent) => format!("{} advertised to its neighbours", sent.name),
                        Err(error) => format!("couldn't advertise: {error:#}"),
                    };
                    let _ = updates.send(Update::Status(status));
                });
            }
            Command::Send { to, text } => {
                let token = self.token.clone();
                tokio::spawn(async move {
                    let request = SendRequest { to: to.clone(), text };
                    let result: Result<SentMessageInfo> =
                        server.post("/api/v1/send", &request, token.as_deref()).await;
                    let status = match &result {
                        Ok(sent) => format!("sent to {} as {}", sent.to, sent.from),
                        Err(error) => format!("couldn't send to {to}: {error:#}"),
                    };
                    // Keep the DM view on the conversation just written in,
                    // under the name the server resolved.
                    if let Ok(sent) = &result
                        && sent.direct
                    {
                        let _ = updates.send(Update::Wrote(sent.to.clone()));
                    }
                    let _ = updates.send(Update::Status(status));
                    fetch_dms(&server, &updates).await;
                });
            }
        }
    }

    fn refresh(&self) {
        let (server, updates) = (self.server.clone(), self.updates.clone());
        tokio::spawn(async move { fetch_lists(&server, &updates).await });
    }

    /// Direct messages don't come down the event streams, so the DM view is
    /// kept current by asking for them.
    fn refresh_dms(&self) {
        let (server, updates) = (self.server.clone(), self.updates.clone());
        tokio::spawn(async move { fetch_dms(&server, &updates).await });
    }
}

async fn fetch_lists(server: &Server, updates: &UnboundedSender<Update>) {
    let update = match server.get::<Vec<ChannelInfo>>("/api/v1/channels").await {
        Ok(channels) => Update::Channels(channels),
        Err(error) => Update::Status(format!("couldn't load channels: {error:#}")),
    };
    let _ = updates.send(update);
    let update =
        match server.get::<Vec<NodeInfo>>(&format!("/api/v1/nodes?limit={MAX_NODES}")).await {
            Ok(nodes) => Update::Nodes(nodes),
            Err(error) => Update::Status(format!("couldn't load nodes: {error:#}")),
        };
    let _ = updates.send(update);
    let health = server
        .get::<Vec<ObserverHealth>>("/api/v1/observers?hours=24")
        .await
        .map_err(|error| format!("{error:#}"));
    let _ = updates.send(Update::Health(health));
    fetch_dms(server, updates).await;
    fetch_contacts(server, updates).await;
}

async fn fetch_contacts(server: &Server, updates: &UnboundedSender<Update>) {
    let contacts = server
        .get::<Vec<RadioContact>>("/api/v1/contacts")
        .await
        .map_err(|error| format!("{error:#}"));
    let _ = updates.send(Update::Contacts(contacts));
}

async fn fetch_dms(server: &Server, updates: &UnboundedSender<Update>) {
    let dms = server
        .get::<Vec<DirectMessageInfo>>(&format!("/api/v1/direct?limit={DM_HISTORY}"))
        .await
        .map_err(|error| format!("{error:#}"));
    let _ = updates.send(Update::Dms(dms));
    if let Ok(sent) =
        server.get::<Vec<SentMessageInfo>>(&format!("/api/v1/outbox?limit={DM_HISTORY}")).await
    {
        let _ = updates.send(Update::SentDms(sent));
    }
}

/// Keys for `--keys`: characters as typed, plus `<enter>`, `<esc>`, `<tab>`,
/// `<up>`, `<down>`, `<pgup>`, `<pgdn>`, `<home>`, `<end>`, `<bs>` and `<lt>`.
pub fn parse_keys(text: &str) -> Result<Vec<KeyEvent>> {
    let mut keys = Vec::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let (code, used) = match rest.find('>') {
            Some(end) if c == '<' => {
                let code = match &rest[1..end] {
                    "enter" => KeyCode::Enter,
                    "esc" => KeyCode::Esc,
                    "tab" => KeyCode::Tab,
                    "up" => KeyCode::Up,
                    "down" => KeyCode::Down,
                    "pgup" => KeyCode::PageUp,
                    "pgdn" => KeyCode::PageDown,
                    "home" => KeyCode::Home,
                    "end" => KeyCode::End,
                    "bs" => KeyCode::Backspace,
                    "lt" => KeyCode::Char('<'),
                    name => bail!("unknown key <{name}>; type < as <lt>"),
                };
                (code, end + 1)
            }
            _ => (KeyCode::Char(c), c.len_utf8()),
        };
        keys.push(KeyEvent::new(code, KeyModifiers::NONE));
        rest = &rest[used..];
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_parse() {
        let codes: Vec<KeyCode> =
            parse_keys("3<enter>/a b<lt><bs>").unwrap().into_iter().map(|key| key.code).collect();
        assert_eq!(
            codes,
            [
                KeyCode::Char('3'),
                KeyCode::Enter,
                KeyCode::Char('/'),
                KeyCode::Char('a'),
                KeyCode::Char(' '),
                KeyCode::Char('b'),
                KeyCode::Char('<'),
                KeyCode::Backspace,
            ]
        );
        assert!(parse_keys("<nope>").is_err());
    }
}
