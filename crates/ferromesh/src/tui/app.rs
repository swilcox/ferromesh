//! The TUI's state and input handling. Nothing here touches the terminal or
//! the network, so it can be driven with keys and updates in tests.

use std::collections::{BTreeMap, HashMap, HashSet};

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ferromesh_model::{
    ChannelInfo, DirectMessageInfo, Event, Filter, FilterError, Kind, NodeInfo, ObserverHealth,
    PacketDetail, RadioContact, SendStatus, SentMessageInfo,
};
use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::config::WatchConfig;

/// Events kept per feed; live events push the oldest out beyond this.
pub const FEED_CAP: usize = 20_000;
/// Events asked for per page of older history.
pub const OLDER_BATCH: usize = 200;
/// Alerts kept, oldest dropped first.
const ALERT_CAP: usize = 500;
/// Rows a page key moves.
const PAGE: isize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum View {
    Messages,
    Dms,
    Packets,
    Rf,
    Nodes,
    Alerts,
    Health,
    /// The companion radio's own contact list.
    Contacts,
}

impl View {
    pub const ALL: [Self; 8] = [
        Self::Messages,
        Self::Dms,
        Self::Packets,
        Self::Rf,
        Self::Nodes,
        Self::Alerts,
        Self::Health,
        Self::Contacts,
    ];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Messages => "Messages",
            Self::Dms => "DMs",
            Self::Packets => "Packets",
            Self::Rf => "RF",
            Self::Nodes => "Nodes",
            Self::Alerts => "Alerts",
            Self::Health => "Health",
            Self::Contacts => "Contacts",
        }
    }

    /// The kind of event the view lists, if it lists events.
    pub const fn kind(self) -> Option<Kind> {
        match self {
            Self::Messages => Some(Kind::Messages),
            Self::Packets => Some(Kind::Packets),
            Self::Rf => Some(Kind::Observations),
            Self::Dms | Self::Nodes | Self::Alerts | Self::Health | Self::Contacts => None,
        }
    }
}

const fn slot(kind: Kind) -> usize {
    match kind {
        Kind::Messages => 0,
        Kind::Packets => 1,
        Kind::Observations => 2,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Connection {
    #[default]
    Connecting,
    Live,
    Lost(String),
}

/// One kind's events, in id order.
#[derive(Debug, Default)]
pub struct Feed {
    pub events: BTreeMap<i64, Event>,
    /// History has arrived, so later events are live.
    pub live: bool,
    pub connection: Connection,
    /// Per server-side filter (`None` for everything), how far back the feed
    /// holds every match.
    reach: HashMap<Option<String>, Reach>,
    pub loading_older: bool,
}

#[derive(Debug, Clone, Copy)]
struct Reach {
    /// Every match with an id at or above this is held.
    from: i64,
    /// The server has nothing older.
    exhausted: bool,
}

impl Feed {
    /// Where to ask for older matches of `filter`, or `None` when there are
    /// none to ask for.
    fn older_than(&self, filter: &Option<String>) -> Option<i64> {
        let all = self.reach.get(&None)?;
        let own = self.reach.get(filter).filter(|_| filter.is_some());
        if all.exhausted || own.is_some_and(|reach| reach.exhausted) {
            return None;
        }
        Some(own.map_or(all.from, |reach| reach.from.min(all.from)))
    }

    fn trim(&mut self) {
        let mut trimmed = false;
        while self.events.len() > FEED_CAP {
            self.events.pop_first();
            trimmed = true;
        }
        if let (true, Some(&first)) = (trimmed, self.events.keys().next()) {
            for reach in self.reach.values_mut() {
                *reach = Reach { from: reach.from.max(first), exhausted: false };
            }
        }
    }
}

pub struct Watch {
    pub config: WatchConfig,
    filter: Filter,
}

#[derive(Debug, Clone)]
pub struct Alert {
    pub watch: String,
    pub event: Event,
}

/// One direct message in a conversation, sent or received.
#[derive(Debug, Clone, PartialEq)]
pub struct DmLine {
    pub at: Timestamp,
    /// Sent by us, rather than received.
    pub outgoing: bool,
    pub body: String,
    /// Hops it travelled, for a received message.
    pub hops: Option<u8>,
    pub snr: Option<f64>,
    /// How a sent message is getting on.
    pub status: Option<SendStatus>,
    pub round_trip_ms: Option<u32>,
    pub error: Option<String>,
}

/// Everything exchanged with one node, oldest first.
#[derive(Debug, Clone, PartialEq)]
pub struct Conversation {
    /// The node's name, or its key prefix when nothing has named it. This is
    /// also what a reply is addressed to.
    pub who: String,
    pub messages: Vec<DmLine>,
}

impl Conversation {
    pub fn last_at(&self) -> Timestamp {
        self.messages.last().map_or(Timestamp::UNIX_EPOCH, |message| message.at)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    Filter,
    Watch,
    /// A message to the selected channel.
    Compose,
    /// Advertising the radio: `l` for neighbours, `f` for the whole mesh.
    Advert,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub prompt: Prompt,
    pub text: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Inspector {
    pub hash: String,
    /// `None` while loading.
    pub detail: Option<Result<PacketDetail, String>>,
    pub scroll: u16,
}

/// Work for the network side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Fetch a packet's detail for the inspector.
    Inspect(String),
    /// Fetch matching history older than `before`.
    Older {
        kind: Kind,
        before: i64,
        filter: Option<String>,
    },
    SaveWatches(Vec<WatchConfig>),
    /// Send through the server's companion radio.
    Send {
        to: String,
        text: String,
    },
    /// Advertise the server's companion radio.
    Advert {
        flood: bool,
    },
    /// Keep a node on the radio's contact list, or let it go.
    Pin {
        to: String,
        pinned: bool,
    },
}

/// News from the network side.
#[derive(Debug, Clone)]
pub enum Update {
    Event(Event),
    CaughtUp(Kind),
    Lost(Kind, String),
    Channels(Vec<ChannelInfo>),
    Nodes(Vec<NodeInfo>),
    /// Observers' health, or why it couldn't be fetched.
    Health(Result<Vec<ObserverHealth>, String>),
    /// Direct messages received, or why they couldn't be fetched.
    Dms(Result<Vec<DirectMessageInfo>, String>),
    /// Direct messages sent, newest first.
    SentDms(Vec<SentMessageInfo>),
    /// The radio's contacts, or why they couldn't be fetched.
    Contacts(Result<Vec<RadioContact>, String>),
    Detail(String, Result<PacketDetail, String>),
    Older {
        kind: Kind,
        filter: Option<String>,
        result: Result<Vec<Event>, String>,
    },
    Status(String),
}

struct ViewFilter {
    text: String,
    /// `None` for the nodes view, whose filter is plain text.
    filter: Option<Filter>,
}

pub struct App {
    pub server: String,
    /// For showing times; UTC in tests.
    pub zone: TimeZone,
    /// When the screen is drawn, for relative times.
    pub now: Timestamp,
    pub view: View,
    feeds: [Feed; 3],
    pub channels: Vec<ChannelInfo>,
    pub nodes: Vec<NodeInfo>,
    pub health: Result<Vec<ObserverHealth>, String>,
    /// Direct messages received, newest first, or why they couldn't be
    /// fetched.
    pub dms: Result<Vec<DirectMessageInfo>, String>,
    /// Direct messages sent, newest first.
    pub sent_dms: Vec<SentMessageInfo>,
    /// The DM view's conversation; `None` selects the newest.
    pub correspondent: Option<String>,
    /// Lines the conversation is scrolled back from its newest message.
    pub dm_scroll: usize,
    /// The radio's contacts, or why they couldn't be fetched.
    pub contacts: Result<Vec<RadioContact>, String>,
    pub contact_selected: usize,
    /// Node names by lowercase public-key prefix of 1–3 bytes, `None` where
    /// the prefix is ambiguous or the node unnamed.
    hop_names: HashMap<String, Option<String>>,
    /// The messages view's channel; `None` shows all of them.
    pub channel: Option<String>,
    pub sidebar_focus: bool,
    filters: HashMap<View, ViewFilter>,
    /// Per feed, the selected event's [`order`]; `None` follows the newest.
    cursors: [Option<(Timestamp, i64)>; 3],
    pub node_selected: usize,
    pub watches: Vec<Watch>,
    /// Oldest first.
    pub alerts: Vec<Alert>,
    /// Counted from the newest alert.
    pub alert_selected: usize,
    pub watch_focus: bool,
    pub watch_selected: usize,
    pub unseen_alerts: usize,
    watched: HashSet<(Kind, i64)>,
    /// Receptions seen per packet hash, to keep heard counts current.
    heard: HashMap<String, i64>,
    /// Per channel, the newest message id counted as read.
    last_read: HashMap<String, i64>,
    pub input: Option<Input>,
    pub inspector: Option<Inspector>,
    pub help: bool,
    pub bell_enabled: bool,
    bell: bool,
    pub status: Option<String>,
    pub quit: bool,
}

impl App {
    pub fn new(server: String, watches: Vec<WatchConfig>) -> Self {
        let mut app = Self {
            server,
            zone: TimeZone::system(),
            now: Timestamp::now(),
            view: View::Messages,
            feeds: Default::default(),
            channels: Vec::new(),
            nodes: Vec::new(),
            health: Ok(Vec::new()),
            dms: Ok(Vec::new()),
            sent_dms: Vec::new(),
            correspondent: None,
            dm_scroll: 0,
            contacts: Ok(Vec::new()),
            contact_selected: 0,
            hop_names: HashMap::new(),
            channel: None,
            sidebar_focus: false,
            filters: HashMap::new(),
            cursors: [None; 3],
            node_selected: 0,
            watches: Vec::new(),
            alerts: Vec::new(),
            alert_selected: 0,
            watch_focus: false,
            watch_selected: 0,
            unseen_alerts: 0,
            watched: HashSet::new(),
            heard: HashMap::new(),
            last_read: HashMap::new(),
            input: None,
            inspector: None,
            help: false,
            bell_enabled: true,
            bell: false,
            status: None,
            quit: false,
        };
        let mut skipped = Vec::new();
        for config in watches {
            match parse_for(&config.filter, config.kind) {
                Ok(filter) => app.watches.push(Watch { config, filter }),
                Err(error) => skipped.push(format!("{}: {error}", config.name)),
            }
        }
        if !skipped.is_empty() {
            app.status = Some(format!("skipped watches: {}", skipped.join("; ")));
        }
        app
    }

    pub fn feed(&self, kind: Kind) -> &Feed {
        &self.feeds[slot(kind)]
    }

    fn feed_mut(&mut self, kind: Kind) -> &mut Feed {
        &mut self.feeds[slot(kind)]
    }

    pub fn filter_text(&self, view: View) -> Option<&str> {
        self.filters.get(&view).map(|filter| filter.text.as_str())
    }

    pub fn is_watched(&self, event: &Event) -> bool {
        self.watched.contains(&(event.kind(), event.id()))
    }

    /// Copies heard, counting receptions that arrived after the event did.
    pub fn heard(&self, event: &Event) -> i64 {
        let (hash, stored) = match event {
            Event::Message(message) => (&message.packet_hash, message.heard),
            Event::Packet(packet) => (&packet.hash, packet.heard),
            Event::Observation(_) => return 1,
        };
        stored.max(self.heard.get(hash).copied().unwrap_or(0))
    }

    /// Messages not yet shown, per channel.
    pub fn unread(&self) -> HashMap<&str, usize> {
        let mut counts = HashMap::new();
        for event in self.feeds[slot(Kind::Messages)].events.values() {
            if let Event::Message(message) = event
                && message.id > self.last_read.get(&message.channel).copied().unwrap_or(0)
            {
                *counts.entry(message.channel.as_str()).or_default() += 1;
            }
        }
        counts
    }

    /// The name of the one node whose key starts with `hop` (hex).
    pub fn hop_name(&self, hop: &str) -> Option<&str> {
        self.hop_names.get(&hop.to_ascii_lowercase())?.as_deref()
    }

    /// The first stream that isn't connected, and why.
    pub fn feed_problem(&self) -> Option<String> {
        Kind::ALL.into_iter().find_map(|kind| match &self.feed(kind).connection {
            Connection::Lost(reason) => Some(format!("{kind} stream: {reason}")),
            _ => None,
        })
    }

    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.bell)
    }

    /// Nothing is loading: every stream has delivered history or failed, and
    /// no fetch is outstanding.
    pub fn idle(&self) -> bool {
        self.feeds.iter().all(|feed| {
            (feed.live || matches!(feed.connection, Connection::Lost(_))) && !feed.loading_older
        }) && self.inspector.as_ref().is_none_or(|inspector| inspector.detail.is_some())
    }

    /// The events a list view shows, in [`order`]: a message decrypted late,
    /// when its channel was added, still shows when it was received.
    pub fn visible(&self, view: View) -> Vec<&Event> {
        let Some(kind) = view.kind() else {
            return Vec::new();
        };
        let filter = self.filters.get(&view).and_then(|f| f.filter.as_ref());
        let mut visible: Vec<&Event> = self.feeds[slot(kind)]
            .events
            .values()
            .filter(|event| {
                let in_channel = match (view, &self.channel, event) {
                    (View::Messages, Some(channel), Event::Message(message)) => {
                        message.channel == *channel
                    }
                    _ => true,
                };
                in_channel && filter.is_none_or(|filter| filter.matches(event))
            })
            .collect();
        // Nearly sorted already, which the stable sort handles quickly.
        visible.sort_by_key(|event| order(event));
        visible
    }

    /// The selected row among `visible`: the cursor's event, or the newest
    /// when following.
    pub fn selected(&self, view: View, visible: &[&Event]) -> Option<usize> {
        let last = visible.len().checked_sub(1)?;
        let kind = view.kind()?;
        Some(match self.cursors[slot(kind)] {
            None => last,
            Some(cursor) => visible.partition_point(|event| order(event) < cursor).min(last),
        })
    }

    pub fn following(&self, view: View) -> bool {
        view.kind().is_none_or(|kind| self.cursors[slot(kind)].is_none())
    }

    pub fn visible_nodes(&self) -> Vec<&NodeInfo> {
        let needle = self.filters.get(&View::Nodes).map(|filter| filter.text.to_lowercase());
        self.nodes
            .iter()
            .filter(|node| {
                needle.as_ref().is_none_or(|needle| {
                    [
                        node.name.as_deref().unwrap_or_default(),
                        node.role.as_deref().unwrap_or_default(),
                        &node.pubkey,
                    ]
                    .iter()
                    .any(|field| field.to_lowercase().contains(needle.as_str()))
                })
            })
            .collect()
    }

    /// Everything exchanged with each node, newest conversation first and
    /// each conversation oldest message last. Messages received are grouped
    /// by the sender's name, or by its key prefix while no advert has named
    /// it; messages sent are grouped by who they were addressed to, which is
    /// the same name.
    pub fn conversations(&self) -> Vec<Conversation> {
        let mut threads: BTreeMap<String, Vec<DmLine>> = BTreeMap::new();
        for dm in self.dms.as_deref().unwrap_or_default() {
            let who = dm.sender.clone().unwrap_or_else(|| dm.sender_prefix.clone());
            threads.entry(who).or_default().push(DmLine {
                at: dm.received_at,
                outgoing: false,
                body: dm.body.clone(),
                hops: dm.hops,
                snr: dm.snr,
                status: None,
                round_trip_ms: None,
                error: None,
            });
        }
        for sent in self.sent_dms.iter().filter(|sent| sent.direct) {
            threads.entry(sent.to.clone()).or_default().push(DmLine {
                at: sent.sent_at,
                outgoing: true,
                body: sent.body.clone(),
                hops: None,
                snr: None,
                status: Some(sent.status),
                round_trip_ms: sent.round_trip_ms,
                error: sent.error.clone(),
            });
        }
        let mut conversations: Vec<Conversation> = threads
            .into_iter()
            .map(|(who, mut messages)| {
                messages.sort_by_key(|message| message.at);
                Conversation { who, messages }
            })
            .collect();
        conversations.sort_by(|a, b| b.last_at().cmp(&a.last_at()).then_with(|| a.who.cmp(&b.who)));
        conversations
    }

    /// The conversation the DM view is showing, and where it sits in the
    /// list.
    pub fn selected_conversation<'a>(
        &self,
        conversations: &'a [Conversation],
    ) -> Option<(usize, &'a Conversation)> {
        let index = match &self.correspondent {
            Some(who) => conversations.iter().position(|thread| &thread.who == who)?,
            None => 0,
        };
        conversations.get(index).map(|thread| (index, thread))
    }

    /// Who `c` writes to: the selected conversation in the DM view, or the
    /// selected channel in the messages view.
    pub fn compose_target(&self) -> Option<String> {
        match self.view {
            View::Contacts => self.selected_contact().map(|contact| contact.name.clone()),
            View::Dms => {
                let conversations = self.conversations();
                self.selected_conversation(&conversations).map(|(_, thread)| thread.who.clone())
            }
            _ => self.channel.clone(),
        }
    }

    /// The radio's contacts, favourites first and then the most recently
    /// heard, so the last non-favourite is the one the radio replaces next.
    pub fn visible_contacts(&self) -> Vec<&RadioContact> {
        let needle = self.filters.get(&View::Contacts).map(|filter| filter.text.to_lowercase());
        let mut contacts: Vec<&RadioContact> = self
            .contacts
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|contact| {
                needle.as_ref().is_none_or(|needle| {
                    [contact.name.as_str(), contact.kind.as_str(), contact.pubkey.as_str()]
                        .iter()
                        .any(|field| field.to_lowercase().contains(needle.as_str()))
                })
            })
            .collect();
        contacts.sort_by(|a, b| {
            b.favourite
                .cmp(&a.favourite)
                .then_with(|| b.last_advert.cmp(&a.last_advert))
                .then_with(|| a.name.cmp(&b.name))
        });
        contacts
    }

    /// The contact the radio would replace to make room: the one it heard
    /// from least recently that isn't a favourite.
    pub fn next_replaced(&self) -> Option<&str> {
        self.contacts
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|contact| !contact.favourite)
            .min_by_key(|contact| contact.last_advert)
            .map(|contact| contact.pubkey.as_str())
    }

    fn selected_contact(&self) -> Option<&RadioContact> {
        let contacts = self.visible_contacts();
        contacts.get(self.contact_selected.min(contacts.len().saturating_sub(1))).copied()
    }

    /// Pins or unpins what's selected: a contact in the contacts view, or a
    /// node in the nodes view, which adds it to the radio.
    fn pin_selected(&mut self) -> Vec<Command> {
        let (to, pinned, what) = match self.view {
            View::Contacts => match self.selected_contact() {
                Some(contact) => (contact.name.clone(), !contact.favourite, contact.name.clone()),
                None => return Vec::new(),
            },
            View::Nodes => {
                let nodes = self.visible_nodes();
                let Some(node) = nodes.get(self.node_selected) else {
                    return Vec::new();
                };
                let to = node.name.clone().unwrap_or_else(|| node.pubkey[..12].to_owned());
                (to.clone(), true, to)
            }
            _ => return Vec::new(),
        };
        self.status = Some(if pinned {
            format!("keeping {what} on the radio…")
        } else {
            format!("letting the radio replace {what} when it needs room…")
        });
        vec![Command::Pin { to, pinned }]
    }

    pub fn apply(&mut self, update: Update) {
        match update {
            Update::Event(event) => self.receive(event, true),
            Update::CaughtUp(kind) => {
                let feed = self.feed_mut(kind);
                let first = !feed.live;
                feed.live = true;
                feed.connection = Connection::Live;
                if first {
                    // The stream's history was the newest matches, so the
                    // feed holds everything from its oldest event on.
                    let reach = match feed.events.keys().next() {
                        Some(&from) => Reach { from, exhausted: false },
                        None => Reach { from: i64::MAX, exhausted: true },
                    };
                    feed.reach.insert(None, reach);
                    if kind == Kind::Messages {
                        self.mark_read(None);
                    }
                }
            }
            Update::Lost(kind, reason) => self.feed_mut(kind).connection = Connection::Lost(reason),
            Update::Channels(channels) => self.channels = channels,
            Update::Health(health) => self.health = health,
            Update::Dms(dms) => self.dms = dms,
            Update::SentDms(sent) => self.sent_dms = sent,
            Update::Contacts(contacts) => self.contacts = contacts,
            Update::Nodes(nodes) => {
                self.hop_names.clear();
                for node in &nodes {
                    for bytes in 1..=3 {
                        let Some(prefix) = node.pubkey.get(..bytes * 2) else { continue };
                        self.hop_names
                            .entry(prefix.to_ascii_lowercase())
                            .and_modify(|name| *name = None)
                            .or_insert_with(|| node.name.clone());
                    }
                }
                self.nodes = nodes;
                self.node_selected = self.node_selected.min(self.nodes.len().saturating_sub(1));
            }
            Update::Detail(hash, detail) => {
                if let Some(inspector) = self.inspector.as_mut().filter(|i| i.hash == hash) {
                    inspector.detail = Some(detail);
                }
            }
            Update::Older { kind, filter, result } => {
                let feed = self.feed_mut(kind);
                feed.loading_older = false;
                let events = match result {
                    Ok(events) => events,
                    Err(error) => {
                        self.status = Some(format!("couldn't load older {kind}: {error}"));
                        return;
                    }
                };
                let Some(before) = feed.older_than(&filter) else {
                    return;
                };
                let from = events.iter().map(Event::id).min().unwrap_or(before).min(before);
                let reach = Reach { from, exhausted: events.len() < OLDER_BATCH };
                feed.reach.insert(filter, reach);
                for event in events {
                    self.receive(event, false);
                }
            }
            Update::Status(message) => self.status = Some(message),
        }
    }

    /// `live` is false for history fetched on request, which can't alert.
    fn receive(&mut self, event: Event, live: bool) {
        let kind = event.kind();
        let feed = &self.feeds[slot(kind)];
        let alerting = live && feed.live;
        let key = (kind, event.id());
        if feed.events.contains_key(&key.1) {
            return;
        }
        if let Event::Observation(observation) = &event {
            *self.heard.entry(observation.hash.clone()).or_default() += 1;
        }

        let matched: Vec<String> = self
            .watches
            .iter()
            .filter(|watch| watch.config.kind == kind && watch.filter.matches(&event))
            .map(|watch| watch.config.name.clone())
            .collect();
        if !matched.is_empty() {
            self.watched.insert(key);
            if alerting {
                for watch in matched {
                    self.alerts.push(Alert { watch, event: event.clone() });
                }
                let excess = self.alerts.len().saturating_sub(ALERT_CAP);
                self.alerts.drain(..excess);
                if self.view != View::Alerts {
                    self.unseen_alerts += 1;
                }
                self.bell = self.bell_enabled;
            }
        }

        if let Event::Message(message) = &event {
            let shown = self.view == View::Messages
                && self.cursors[slot(Kind::Messages)].is_none()
                && self.channel.as_ref().is_none_or(|channel| *channel == message.channel);
            if alerting && shown {
                self.last_read.insert(message.channel.clone(), message.id);
            }
        }
        let feed = self.feed_mut(kind);
        feed.events.insert(key.1, event);
        if live {
            feed.trim();
        }
    }

    /// Counts every stored message on `channel` (or every channel) as read.
    fn mark_read(&mut self, channel: Option<&str>) {
        for event in self.feeds[slot(Kind::Messages)].events.values() {
            if let Event::Message(message) = event
                && channel.is_none_or(|channel| channel == message.channel)
            {
                let read = self.last_read.entry(message.channel.clone()).or_default();
                *read = (*read).max(message.id);
            }
        }
    }

    pub fn select_channel(&mut self, channel: Option<String>) {
        self.channel = channel;
        self.cursors[slot(Kind::Messages)] = None;
        let channel = self.channel.clone();
        self.mark_read(channel.as_deref());
    }

    pub fn handle(&mut self, event: TermEvent) -> Vec<Command> {
        match event {
            TermEvent::Key(key) if key.kind != KeyEventKind::Release => self.key(key),
            _ => Vec::new(),
        }
    }

    fn key(&mut self, key: KeyEvent) -> Vec<Command> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return Vec::new();
        }
        if self.input.is_some() {
            return self.input_key(key);
        }
        self.status = None;
        if self.help {
            self.help = false;
            return Vec::new();
        }
        if let Some(inspector) = &mut self.inspector {
            match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => self.inspector = None,
                KeyCode::Down | KeyCode::Char('j') => {
                    inspector.scroll = inspector.scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    inspector.scroll = inspector.scroll.saturating_sub(1)
                }
                KeyCode::PageDown => inspector.scroll = inspector.scroll.saturating_add(10),
                KeyCode::PageUp => inspector.scroll = inspector.scroll.saturating_sub(10),
                _ => {}
            }
            return Vec::new();
        }

        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char(digit @ '1'..='8') => {
                self.show(View::ALL[usize::from(digit as u8 - b'1')])
            }
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('/') => self.prompt(Prompt::Filter),
            KeyCode::Char('w') => self.prompt(Prompt::Watch),
            KeyCode::Char('b') => {
                self.bell_enabled = !self.bell_enabled;
                self.status =
                    Some(format!("bell {}", if self.bell_enabled { "on" } else { "off" }));
            }
            KeyCode::Tab | KeyCode::BackTab => match self.view {
                View::Messages | View::Dms => self.sidebar_focus = !self.sidebar_focus,
                View::Alerts => self.watch_focus = !self.watch_focus,
                _ => {}
            },
            KeyCode::Esc => {
                if self.sidebar_focus {
                    self.sidebar_focus = false;
                } else if self.filters.remove(&self.view).is_some() {
                    self.follow();
                }
            }
            KeyCode::Down | KeyCode::Char('j') => return self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => return self.move_by(-1),
            KeyCode::PageDown => return self.move_by(PAGE),
            KeyCode::PageUp => return self.move_by(-PAGE),
            KeyCode::Home | KeyCode::Char('g') => return self.move_by(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.follow(),
            KeyCode::Enter => return self.inspect_selected(),
            KeyCode::Char('d') if self.view == View::Alerts && self.watch_focus => {
                return self.delete_watch();
            }
            KeyCode::Char('c') if self.view == View::Alerts => {
                self.alerts.clear();
                self.alert_selected = 0;
            }
            KeyCode::Char('c')
                if matches!(self.view, View::Messages | View::Dms | View::Contacts) =>
            {
                self.prompt(Prompt::Compose);
            }
            KeyCode::Char('a') => self.prompt(Prompt::Advert),
            KeyCode::Char('p') if matches!(self.view, View::Contacts | View::Nodes) => {
                return self.pin_selected();
            }
            _ => {}
        }
        Vec::new()
    }

    fn show(&mut self, view: View) {
        self.view = view;
        self.sidebar_focus = false;
        if view == View::Alerts {
            self.unseen_alerts = 0;
        }
    }

    fn follow(&mut self) {
        match self.view.kind() {
            Some(kind) => self.cursors[slot(kind)] = None,
            // A plain list has no newest to follow, so End goes to its end.
            None => match self.view {
                View::Contacts => {
                    self.contact_selected = self.visible_contacts().len().saturating_sub(1);
                }
                View::Nodes => self.node_selected = self.visible_nodes().len().saturating_sub(1),
                _ => {
                    self.node_selected = 0;
                    self.alert_selected = 0;
                }
            },
        }
        if self.view == View::Messages {
            let channel = self.channel.clone();
            self.mark_read(channel.as_deref());
        }
    }

    fn move_by(&mut self, delta: isize) -> Vec<Command> {
        match self.view {
            View::Messages if self.sidebar_focus => {
                self.move_sidebar(delta);
                Vec::new()
            }
            View::Dms if self.sidebar_focus => {
                let conversations = self.conversations();
                let current = self.selected_conversation(&conversations).map_or(0, |(at, _)| at);
                let last = conversations.len().saturating_sub(1);
                self.correspondent =
                    conversations.get(step(current, delta, last)).map(|thread| thread.who.clone());
                self.dm_scroll = 0;
                Vec::new()
            }
            View::Dms => {
                // Scrolling back from the newest message, so a conversation
                // that grows while you read keeps its place.
                let back = -delta;
                self.dm_scroll = self.dm_scroll.saturating_add_signed(back);
                Vec::new()
            }
            View::Contacts => {
                let last = self.visible_contacts().len().saturating_sub(1);
                self.contact_selected = step(self.contact_selected, delta, last);
                Vec::new()
            }
            View::Nodes => {
                let last = self.visible_nodes().len().saturating_sub(1);
                self.node_selected = step(self.node_selected, delta, last);
                Vec::new()
            }
            View::Alerts if self.watch_focus => {
                self.watch_selected =
                    step(self.watch_selected, delta, self.watches.len().saturating_sub(1));
                Vec::new()
            }
            View::Alerts => {
                self.alert_selected =
                    step(self.alert_selected, delta, self.alerts.len().saturating_sub(1));
                Vec::new()
            }
            view => self.move_list(view, delta),
        }
    }

    fn move_list(&mut self, view: View, delta: isize) -> Vec<Command> {
        let Some(kind) = view.kind() else {
            return Vec::new();
        };
        let keys: Vec<(Timestamp, i64)> = self.visible(view).into_iter().map(order).collect();
        let Some(last) = keys.len().checked_sub(1) else {
            return self.older(view, kind);
        };
        let current = match self.cursors[slot(kind)] {
            None => last,
            Some(cursor) => keys.partition_point(|&key| key < cursor).min(last),
        };
        let target = current as isize + delta;
        if target < 0 {
            self.cursors[slot(kind)] = Some(keys[0]);
            return self.older(view, kind);
        }
        let target = (target as usize).min(last);
        self.cursors[slot(kind)] =
            if target == last && delta > 0 { None } else { Some(keys[target]) };
        if self.cursors[slot(kind)].is_none() && view == View::Messages {
            let channel = self.channel.clone();
            self.mark_read(channel.as_deref());
        }
        Vec::new()
    }

    fn move_sidebar(&mut self, delta: isize) {
        let names: Vec<Option<String>> = std::iter::once(None)
            .chain(self.channels.iter().map(|channel| Some(channel.name.clone())))
            .collect();
        let current = names.iter().position(|name| *name == self.channel).unwrap_or(0);
        let target = step(current, delta, names.len() - 1);
        self.select_channel(names[target].clone());
    }

    /// Asks for history older than the feed holds, matching the view.
    fn older(&mut self, view: View, kind: Kind) -> Vec<Command> {
        let filter = self.server_filter(view);
        let feed = self.feed_mut(kind);
        if feed.loading_older {
            return Vec::new();
        }
        let Some(before) = feed.older_than(&filter) else {
            return Vec::new();
        };
        feed.loading_older = true;
        vec![Command::Older { kind, before, filter }]
    }

    /// The view's channel and filter, as one server-side filter.
    fn server_filter(&self, view: View) -> Option<String> {
        let mut terms = Vec::new();
        if let (View::Messages, Some(channel)) = (view, &self.channel) {
            terms.push(format!("chan:\"{channel}\""));
        }
        if let Some(filter) = self.filters.get(&view) {
            terms.push(filter.text.clone());
        }
        (!terms.is_empty()).then(|| terms.join(" "))
    }

    fn inspect_selected(&mut self) -> Vec<Command> {
        let hash = match self.view {
            View::Nodes => None,
            View::Alerts => {
                self.alerts.iter().rev().nth(self.alert_selected).map(|a| hash_of(&a.event))
            }
            view => {
                let visible = self.visible(view);
                self.selected(view, &visible).map(|index| hash_of(visible[index]))
            }
        };
        let Some(hash) = hash else {
            return Vec::new();
        };
        self.inspector = Some(Inspector { hash: hash.clone(), detail: None, scroll: 0 });
        vec![Command::Inspect(hash)]
    }

    fn prompt(&mut self, prompt: Prompt) {
        if prompt == Prompt::Watch
            && matches!(self.view, View::Dms | View::Nodes | View::Health | View::Contacts)
        {
            self.status = Some("watches apply to messages, packets and RF".into());
            return;
        }
        if prompt == Prompt::Filter && matches!(self.view, View::Dms | View::Health) {
            self.status = Some("there's nothing to filter here".into());
            return;
        }
        if prompt == Prompt::Advert {
            self.input = Some(Input { prompt, text: String::new(), error: None });
            return;
        }
        if prompt == Prompt::Compose {
            match self.compose_target() {
                Some(_) => self.input = Some(Input { prompt, text: String::new(), error: None }),
                None if self.view == View::Dms => {
                    self.status = Some("no conversation to reply to yet".into());
                }
                None => {
                    self.status = Some("pick a channel to send to first (Tab, then j/k)".into());
                }
            }
            return;
        }
        let text = match (prompt, self.filter_text(self.view), &self.channel) {
            (_, Some(text), _) => text.to_owned(),
            (Prompt::Watch, None, Some(channel)) if self.view == View::Messages => {
                format!("chan:\"{channel}\"")
            }
            _ => String::new(),
        };
        self.input = Some(Input { prompt, text, error: None });
    }

    fn input_key(&mut self, key: KeyEvent) -> Vec<Command> {
        if self.input.as_ref().is_some_and(|input| input.prompt == Prompt::Advert) {
            return self.advert_key(key);
        }
        let Some(input) = &mut self.input else {
            return Vec::new();
        };
        match key.code {
            KeyCode::Esc => self.input = None,
            KeyCode::Enter => return self.submit(),
            KeyCode::Backspace => {
                input.text.pop();
                input.error = None;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.text.push(c);
                input.error = None;
            }
            _ => {}
        }
        Vec::new()
    }

    /// `l` advertises to the radios that hear it directly, `f` across the
    /// whole mesh; anything else waits, and Esc gives up.
    fn advert_key(&mut self, key: KeyEvent) -> Vec<Command> {
        let flood = match key.code {
            KeyCode::Char('l' | 'L') => false,
            KeyCode::Char('f' | 'F') => true,
            KeyCode::Esc => {
                self.input = None;
                return Vec::new();
            }
            _ => return Vec::new(),
        };
        self.input = None;
        self.status = Some(if flood {
            "advertising across the mesh…".into()
        } else {
            "advertising to the neighbours…".to_owned()
        });
        vec![Command::Advert { flood }]
    }

    fn submit(&mut self) -> Vec<Command> {
        let Some(input) = self.input.take() else {
            return Vec::new();
        };
        let text = input.text.trim().to_owned();
        let kind = self.view.kind();
        match input.prompt {
            // Answered by a single key in advert_key, never submitted.
            Prompt::Advert => Vec::new(),
            Prompt::Compose => {
                let Some(to) = self.compose_target() else {
                    return Vec::new();
                };
                if text.is_empty() {
                    return Vec::new();
                }
                self.status = Some(format!("sending to {to}…"));
                vec![Command::Send { to, text }]
            }
            Prompt::Filter => {
                if text.is_empty() {
                    self.filters.remove(&self.view);
                } else if let Some(kind) = kind {
                    match parse_for(&text, kind) {
                        Ok(filter) => {
                            self.filters
                                .insert(self.view, ViewFilter { text, filter: Some(filter) });
                        }
                        Err(error) => {
                            self.input = Some(Input { error: Some(error.to_string()), ..input });
                            return Vec::new();
                        }
                    }
                } else {
                    self.filters.insert(self.view, ViewFilter { text, filter: None });
                }
                self.follow();
                Vec::new()
            }
            Prompt::Watch => {
                let kind = kind.unwrap_or(Kind::Messages);
                let parsed = if text.is_empty() {
                    Err("a watch needs a filter".to_owned())
                } else {
                    parse_for(&text, kind).map_err(|error| error.to_string())
                };
                match parsed {
                    Err(error) => {
                        self.input = Some(Input { error: Some(error), ..input });
                        Vec::new()
                    }
                    Ok(filter) => {
                        for event in self.feeds[slot(kind)].events.values() {
                            if filter.matches(event) {
                                self.watched.insert((kind, event.id()));
                            }
                        }
                        let config = WatchConfig {
                            name: text.chars().take(32).collect(),
                            kind,
                            filter: text,
                        };
                        self.status = Some(format!("watching {kind} for {}", config.filter));
                        self.watches.push(Watch { config, filter });
                        vec![self.save_watches()]
                    }
                }
            }
        }
    }

    fn delete_watch(&mut self) -> Vec<Command> {
        if self.watch_selected >= self.watches.len() {
            return Vec::new();
        }
        let removed = self.watches.remove(self.watch_selected);
        self.watch_selected = self.watch_selected.min(self.watches.len().saturating_sub(1));
        self.watched.clear();
        for watch in &self.watches {
            let kind = watch.config.kind;
            for event in self.feeds[slot(kind)].events.values() {
                if watch.filter.matches(event) {
                    self.watched.insert((kind, event.id()));
                }
            }
        }
        self.status = Some(format!("stopped watching {}", removed.config.filter));
        vec![self.save_watches()]
    }

    fn save_watches(&self) -> Command {
        Command::SaveWatches(self.watches.iter().map(|watch| watch.config.clone()).collect())
    }
}

fn parse_for(text: &str, kind: Kind) -> Result<Filter, FilterError> {
    let filter: Filter = text.parse()?;
    filter.validate(kind)?;
    Ok(filter)
}

/// When the event was received.
pub fn at(event: &Event) -> Timestamp {
    match event {
        Event::Message(message) => message.first_seen_at,
        Event::Packet(packet) => packet.first_seen_at,
        Event::Observation(observation) => observation.rx_at,
    }
}

/// How lists are ordered: by receive time, then by id.
fn order(event: &Event) -> (Timestamp, i64) {
    (at(event), event.id())
}

pub fn hash_of(event: &Event) -> String {
    match event {
        Event::Message(message) => message.packet_hash.clone(),
        Event::Packet(packet) => packet.hash.clone(),
        Event::Observation(observation) => observation.hash.clone(),
    }
}

fn step(current: usize, delta: isize, last: usize) -> usize {
    (current as isize).saturating_add(delta).clamp(0, last as isize) as usize
}

#[cfg(test)]
mod tests {
    use ferromesh_model::{MessageEvent, ObservationEvent};
    use jiff::Timestamp;

    use super::*;

    fn message(id: i64, channel: &str, body: &str) -> Event {
        Event::Message(MessageEvent {
            id,
            packet_hash: format!("{id:016X}"),
            first_seen_at: Timestamp::UNIX_EPOCH,
            channel: channel.into(),
            sender: Some("Bob".into()),
            body: body.into(),
            sender_timestamp: 0,
            txt_type: 0,
            attempt: 0,
            heard: 1,
        })
    }

    fn observation(id: i64, hash: &str) -> Event {
        Event::Observation(ObservationEvent {
            id,
            packet_id: 1,
            hash: hash.into(),
            payload_type: "GRP_TXT".into(),
            rx_at: Timestamp::UNIX_EPOCH,
            observer: "Tanyard".into(),
            route: "flood".into(),
            hops: Vec::new(),
            snr: Some(1.0),
            rssi: None,
            channel: None,
            advert_pubkey: None,
            advert_name: None,
            text: None,
        })
    }

    fn press(app: &mut App, code: KeyCode) -> Vec<Command> {
        app.handle(TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn ids(app: &App, view: View) -> Vec<i64> {
        app.visible(view).iter().map(|event| event.id()).collect()
    }

    fn with_history(events: Vec<Event>) -> App {
        let mut app = App::new("test".into(), Vec::new());
        for event in events {
            app.apply(Update::Event(event));
        }
        for kind in Kind::ALL {
            app.apply(Update::CaughtUp(kind));
        }
        app
    }

    fn older(kind: Kind, filter: Option<&str>, ids: std::ops::Range<i64>) -> Update {
        let result = Ok(ids.map(|id| message(id, "#a", "older")).collect());
        Update::Older { kind, filter: filter.map(str::to_owned), result }
    }

    #[test]
    fn follows_newest_and_pages_back() {
        let history = (1001..=1003).map(|id| message(id, "#a", "recent")).collect();
        let mut app = with_history(history);
        let visible = app.visible(View::Messages);
        assert_eq!(app.selected(View::Messages, &visible), Some(2));

        press(&mut app, KeyCode::Up);
        assert!(!app.following(View::Messages));
        let visible = app.visible(View::Messages);
        assert_eq!(app.selected(View::Messages, &visible), Some(1));

        // A new message doesn't move a selection that isn't following.
        app.apply(Update::Event(message(1004, "#a", "four")));
        let visible = app.visible(View::Messages);
        assert_eq!(visible[app.selected(View::Messages, &visible).unwrap()].id(), 1002);

        assert_eq!(press(&mut app, KeyCode::End), []);
        assert!(app.following(View::Messages));

        let first = press(&mut app, KeyCode::Home);
        assert_eq!(first, [Command::Older { kind: Kind::Messages, before: 1001, filter: None }]);
        assert_eq!(press(&mut app, KeyCode::Up), [], "already loading");
        app.apply(older(Kind::Messages, None, 801..1001));
        assert_eq!(ids(&app, View::Messages).len(), 204);
        assert_eq!(
            press(&mut app, KeyCode::Home),
            [Command::Older { kind: Kind::Messages, before: 801, filter: None }]
        );
        app.apply(older(Kind::Messages, None, 780..801));
        assert_eq!(press(&mut app, KeyCode::Home), [], "a short page was the last");
    }

    #[test]
    fn older_pages_are_tracked_per_filter() {
        let mut app = with_history(vec![message(500, "#a", "recent")]);
        app.select_channel(Some("#a".into()));
        let chan = Some("chan:\"#a\"".to_owned());
        let first = press(&mut app, KeyCode::Home);
        assert_eq!(
            first,
            [Command::Older { kind: Kind::Messages, before: 500, filter: chan.clone() }]
        );
        app.apply(older(Kind::Messages, chan.as_deref(), 100..300));

        // Everything, unfiltered, is only held from 500 on.
        app.select_channel(None);
        let all = press(&mut app, KeyCode::Home);
        assert_eq!(all, [Command::Older { kind: Kind::Messages, before: 500, filter: None }]);
    }

    #[test]
    fn watches_alert_on_live_matches_only() {
        let watch =
            WatchConfig { name: "storm".into(), kind: Kind::Messages, filter: "storm".into() };
        let mut app = App::new("test".into(), vec![watch]);
        app.apply(Update::Event(message(1, "#wx", "storm yesterday")));
        app.apply(Update::CaughtUp(Kind::Messages));
        assert!(app.is_watched(app.visible(View::Messages)[0]));
        assert!(app.alerts.is_empty());
        assert!(!app.take_bell());

        app.apply(Update::Event(message(2, "#wx", "calm")));
        app.apply(Update::Event(message(3, "#wx", "STORM now")));
        assert_eq!(app.alerts.len(), 1);
        assert_eq!(app.unseen_alerts, 1);
        assert!(app.take_bell());
        assert!(!app.take_bell());

        press(&mut app, KeyCode::Char('6'));
        assert_eq!(app.unseen_alerts, 0);
        assert_eq!(press(&mut app, KeyCode::Enter), [Command::Inspect(format!("{:016X}", 3))]);
    }

    #[test]
    fn receptions_raise_heard_counts() {
        let mut app = with_history(vec![message(1, "#a", "hi")]);
        let hash = format!("{:016X}", 1);
        app.apply(Update::Event(observation(10, &hash)));
        app.apply(Update::Event(observation(11, &hash)));
        app.apply(Update::Event(observation(11, &hash)));
        assert_eq!(app.heard(app.visible(View::Messages)[0]), 2);
    }

    #[test]
    fn unread_counts_follow_what_was_shown() {
        let mut app = with_history(vec![message(1, "#a", "old"), message(2, "#b", "old")]);
        app.channels = ["#a", "#b"]
            .iter()
            .map(|name| ChannelInfo {
                name: (*name).into(),
                kind: "hashtag".into(),
                hash: 0,
                enabled: true,
                added_at: Timestamp::UNIX_EPOCH,
                messages: 1,
                last_message_at: None,
            })
            .collect();
        let unread = |app: &App, channel| app.unread().get(channel).copied().unwrap_or(0);
        assert_eq!((unread(&app, "#a"), unread(&app, "#b")), (0, 0));

        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.channel.as_deref(), Some("#a"));
        app.apply(Update::Event(message(3, "#a", "shown")));
        app.apply(Update::Event(message(4, "#b", "elsewhere")));
        assert_eq!((unread(&app, "#a"), unread(&app, "#b")), (0, 1));
        assert_eq!(ids(&app, View::Messages), [1, 3]);

        press(&mut app, KeyCode::Down);
        assert_eq!((app.channel.as_deref(), unread(&app, "#b")), (Some("#b"), 0));
    }

    #[test]
    fn filters_are_checked_for_the_view() {
        let mut app = with_history(vec![message(1, "#a", "storm"), message(2, "#a", "calm")]);
        press(&mut app, KeyCode::Char('/'));
        type_text(&mut app, "snr>1");
        press(&mut app, KeyCode::Enter);
        let error = app.input.as_ref().and_then(|input| input.error.clone());
        assert_eq!(error.as_deref(), Some("snr: doesn't apply to messages"));

        for _ in 0..5 {
            press(&mut app, KeyCode::Backspace);
        }
        type_text(&mut app, "storm");
        press(&mut app, KeyCode::Enter);
        assert!(app.input.is_none());
        assert_eq!(ids(&app, View::Messages), [1]);
        assert_eq!(app.filter_text(View::Messages), Some("storm"));

        app.select_channel(Some("#a".into()));
        assert_eq!(
            press(&mut app, KeyCode::Home),
            [Command::Older {
                kind: Kind::Messages,
                before: 1,
                filter: Some("chan:\"#a\" storm".into())
            }]
        );

        press(&mut app, KeyCode::Esc);
        assert_eq!(app.filter_text(View::Messages), None);
    }

    #[test]
    fn inspector_takes_only_its_own_detail() {
        let mut app = with_history(vec![message(1, "#a", "hi")]);
        let hash = format!("{:016X}", 1);
        assert_eq!(press(&mut app, KeyCode::Enter), [Command::Inspect(hash.clone())]);
        assert!(!app.idle());
        app.apply(Update::Detail("OTHER".into(), Err("nope".into())));
        assert_eq!(app.inspector.as_ref().unwrap().detail, None);
        app.apply(Update::Detail(hash, Err("gone".into())));
        assert!(app.idle());
        press(&mut app, KeyCode::Down);
        assert_eq!(app.inspector.as_ref().unwrap().scroll, 1);
        press(&mut app, KeyCode::Esc);
        assert!(app.inspector.is_none());
    }

    #[test]
    fn watches_are_added_and_deleted() {
        let mut app = with_history(vec![message(1, "#a", "storm")]);
        press(&mut app, KeyCode::Char('w'));
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.input.as_ref().and_then(|i| i.error.as_deref()),
            Some("a watch needs a filter")
        );
        type_text(&mut app, "storm");
        let saved = press(&mut app, KeyCode::Enter);
        let expected =
            WatchConfig { name: "storm".into(), kind: Kind::Messages, filter: "storm".into() };
        assert_eq!(saved, [Command::SaveWatches(vec![expected])]);
        assert!(app.is_watched(app.visible(View::Messages)[0]));

        press(&mut app, KeyCode::Char('6'));
        press(&mut app, KeyCode::Tab);
        assert_eq!(press(&mut app, KeyCode::Char('d')), [Command::SaveWatches(Vec::new())]);
        assert!(!app.is_watched(app.visible(View::Messages)[0]));
    }

    #[test]
    fn lists_follow_receive_time() {
        let received = |id, second| {
            let Event::Message(mut message) = message(id, "#a", "hi") else { unreachable!() };
            message.first_seen_at = Timestamp::from_second(second).unwrap();
            Event::Message(message)
        };
        // Message 3 was decrypted after message 2 but received before it.
        let mut app = with_history(vec![received(1, 100), received(2, 300), received(3, 200)]);
        assert_eq!(ids(&app, View::Messages), [1, 3, 2]);
        press(&mut app, KeyCode::Up);
        let visible = app.visible(View::Messages);
        assert_eq!(visible[app.selected(View::Messages, &visible).unwrap()].id(), 3);
        press(&mut app, KeyCode::Up);
        let visible = app.visible(View::Messages);
        assert_eq!(visible[app.selected(View::Messages, &visible).unwrap()].id(), 1);
    }

    #[test]
    fn hops_resolve_to_unambiguous_names() {
        let node = |pubkey: &str, name: &str| NodeInfo {
            pubkey: pubkey.into(),
            name: Some(name.into()),
            role: None,
            lat: None,
            lon: None,
            first_seen_at: Timestamp::UNIX_EPOCH,
            last_seen_at: Timestamp::UNIX_EPOCH,
            adverts: 1,
        };
        let mut app = with_history(Vec::new());
        app.apply(Update::Nodes(vec![node("a1b2c3d4", "Hilltop"), node("a1ffee00", "Valley")]));
        assert_eq!(app.hop_name("A1"), None);
        assert_eq!(app.hop_name("A1B2"), Some("Hilltop"));
        assert_eq!(app.hop_name("a1ff"), Some("Valley"));
        assert_eq!(app.hop_name("77"), None);
    }

    #[test]
    fn composing_sends_to_the_selected_channel() {
        let mut app = with_history(Vec::new());
        press(&mut app, KeyCode::Char('c'));
        assert!(app.input.is_none(), "no channel selected");
        assert!(app.status.as_deref().unwrap().contains("pick a channel"));

        app.select_channel(Some("#test".into()));
        press(&mut app, KeyCode::Char('c'));
        type_text(&mut app, "hello mesh");
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            [Command::Send { to: "#test".into(), text: "hello mesh".into() }]
        );
        assert_eq!(app.status.as_deref(), Some("sending to #test…"));

        press(&mut app, KeyCode::Char('c'));
        assert_eq!(press(&mut app, KeyCode::Enter), [], "nothing to send");
    }

    #[test]
    fn quitting() {
        let mut app = with_history(Vec::new());
        app.handle(TermEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(app.quit);
    }
}
