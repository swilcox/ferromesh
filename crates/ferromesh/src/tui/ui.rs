//! Drawing the app: a header, the current view, a footer, and overlays.

use std::ops::Range;

use ferromesh_model::{Event, Kind, SendStatus};
use jiff::Timestamp;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};

use super::app::{App, Connection, Conversation, DmLine, Prompt, View};
use super::{lists, overlay};
use crate::render::name_hash;

/// Names are coloured by hash, as in `tail`.
const PALETTE: [Color; 10] = [
    Color::Cyan,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::LightCyan,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
];

/// What scrolling position each list keeps between frames.
#[derive(Debug, Default)]
pub struct Screen {
    pub channels: usize,
    pub correspondents: usize,
    pub messages: usize,
    pub packets: usize,
    pub rf: usize,
    pub nodes: usize,
    pub watches: usize,
    pub alerts: usize,
}

pub fn draw(frame: &mut Frame, app: &App, screen: &mut Screen) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)])
            .areas(frame.area());
    draw_header(frame, header, app);
    match app.view {
        View::Messages => draw_messages(frame, body, app, screen),
        View::Dms => draw_dms(frame, body, app, screen),
        View::Packets | View::Rf => lists::draw_events(frame, body, app, screen),
        View::Nodes => lists::draw_nodes(frame, body, app, screen),
        View::Alerts => lists::draw_alerts(frame, body, app, screen),
        View::Health => lists::draw_health(frame, body, app),
    }
    draw_footer(frame, footer, app);
    if let Some(inspector) = &app.inspector {
        overlay::draw_inspector(frame, body, app, inspector);
    }
    if app.help {
        overlay::draw_help(frame, body);
    }
}

/// A buffer's rows as plain text, for snapshots and tests. Cells a wide
/// character covers are skipped, so rows keep their on-screen width.
pub fn text_lines(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area.height)
        .map(|y| {
            let mut line = String::new();
            let mut covered = 0;
            for x in 0..buffer.area.width {
                let symbol = buffer[(x, y)].symbol();
                if covered > 0 {
                    covered -= 1;
                    continue;
                }
                covered = Span::raw(symbol).width().saturating_sub(1);
                line.push_str(symbol);
            }
            line.trim_end().to_owned()
        })
        .collect()
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let mut tabs =
        vec![Span::styled(" ferromesh ", Style::new().bold().black().on_cyan()), " ".into()];
    for (index, view) in View::ALL.into_iter().enumerate() {
        let label = format!(" {} {} ", index + 1, view.title());
        tabs.push(if view == app.view {
            Span::styled(label, Style::new().bold().reversed())
        } else {
            Span::raw(label)
        });
        if view == View::Alerts && app.unseen_alerts > 0 {
            let count = format!(" {} ", app.unseen_alerts);
            tabs.push(Span::styled(count, Style::new().bold().black().on_yellow()));
        }
    }

    // The tabs are navigation, so they keep their room: on a narrow screen
    // the status drops its labels and the server's address instead.
    let tabs = Line::from(tabs);
    let mut status = feeds_status(app, true);
    if tabs.width() + status.width() > usize::from(area.width) {
        status = feeds_status(app, false);
    }

    let [left, right] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(status.width() as u16)])
            .areas(area);
    frame.render_widget(tabs, left);
    frame.render_widget(status, right);
}

/// Each stream's connection, with labels and the server's address when
/// there's room for them.
fn feeds_status(app: &App, verbose: bool) -> Line<'static> {
    let mut status = Vec::new();
    for (kind, label) in
        [(Kind::Messages, "msgs"), (Kind::Packets, "pkts"), (Kind::Observations, "rf")]
    {
        let color = match app.feed(kind).connection {
            Connection::Live => Color::Green,
            Connection::Connecting => Color::Yellow,
            Connection::Lost(_) => Color::Red,
        };
        status.push(Span::styled("●", color));
        status.push(Span::raw(if verbose { format!(" {label}  ") } else { " ".into() }).dim());
    }
    if verbose {
        status.push(Span::raw(format!("{} ", app.server)).dim());
    }
    Line::from(status)
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    if let Some(input) = &app.input {
        let label = match input.prompt {
            Prompt::Filter => format!("filter {}: ", app.view.title().to_lowercase()),
            Prompt::Watch => "watch: ".to_owned(),
            Prompt::Compose => format!("message {}: ", app.channel.as_deref().unwrap_or_default()),
            Prompt::Advert => {
                "advertise the radio:  l  to the neighbours   f  across the mesh   Esc  cancel"
                    .to_owned()
            }
        };
        let mut spans =
            vec![Span::styled(label, Style::new().bold().cyan()), Span::raw(&input.text)];
        let cursor = Line::from(spans.clone()).width() as u16;
        match &input.error {
            Some(error) => spans.push(Span::styled(format!("   {error}"), Color::Red)),
            None => spans.push(Span::raw("   Enter to apply, Esc to cancel").dim()),
        }
        frame.render_widget(Line::from(spans), area);
        frame.set_cursor_position((area.x + cursor.min(area.width.saturating_sub(1)), area.y));
        return;
    }

    let mut right = Vec::new();
    if let Some(filter) = app.filter_text(app.view) {
        right.push(Span::styled(format!(" filter: {filter} "), Style::new().black().on_cyan()));
        right.push(" ".into());
    }
    if !app.following(app.view) {
        right.push(Span::styled(" paused · End follows ", Style::new().black().on_yellow()));
    }
    let right = Line::from(right);
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(right.width() as u16)])
            .areas(area);

    let left = match (&app.status, app.feed_problem()) {
        (Some(status), _) => Line::from(Span::styled(status.as_str(), Color::Yellow)),
        (None, Some(problem)) => Line::from(Span::styled(problem, Color::Red)),
        (None, None) => {
            let hints = match app.view {
                View::Messages => {
                    "Tab channels  c compose  / filter  w watch  Enter inspect  ? help"
                }
                View::Packets | View::Rf => {
                    "/ filter  w watch  Enter inspect  g/G top/end  ? help  q quit"
                }
                View::Dms => "Tab people  c reply  j/k scroll  ? help  q quit",
                View::Nodes => "/ search  ? help  q quit",
                View::Health => {
                    "from each observer's status reports; refreshed every minute  ? help"
                }
                View::Alerts => {
                    "Tab watches  d delete watch  c clear  Enter inspect  b bell  ? help"
                }
            };
            Line::from(hints).dim()
        }
    };
    frame.render_widget(left, left_area);
    frame.render_widget(right, right_area);
}

fn draw_messages(frame: &mut Frame, area: Rect, app: &App, screen: &mut Screen) {
    let [sidebar, feed] =
        Layout::horizontal([Constraint::Length(26), Constraint::Fill(1)]).areas(area);
    draw_channels(frame, sidebar, app, screen);

    let title = app.channel.as_deref().unwrap_or("All channels");
    let block = pane(Line::from(title), !app.sidebar_focus);
    let inner = block.inner(feed);
    frame.render_widget(block, feed);

    let visible = app.visible(View::Messages);
    if visible.is_empty() {
        let note = if app.feed(Kind::Messages).live { "no messages" } else { "loading…" };
        frame.render_widget(Paragraph::new(note).dim(), inner);
        return;
    }
    let selected = app.selected(View::Messages, &visible);
    let width = usize::from(inner.width);
    // The top row always carries its date; that doesn't change heights.
    let lines = |index: usize, top: bool| {
        let previous =
            if top { None } else { index.checked_sub(1).map(|index| at(visible[index])) };
        message_lines(app, visible[index], previous, width)
    };
    let rows =
        window(visible.len(), selected, usize::from(inner.height), &mut screen.messages, |index| {
            lines(index, false).len()
        });
    let items: Vec<ListItem> =
        rows.clone().map(|index| ListItem::new(lines(index, index == rows.start))).collect();
    let mut state = ListState::default().with_selected(selected.map(|index| index - rows.start));
    let list = List::new(items).highlight_style(if app.sidebar_focus {
        Style::new()
    } else {
        highlight()
    });
    frame.render_stateful_widget(list, inner, &mut state);
}

fn message_lines(
    app: &App,
    event: &Event,
    previous: Option<Timestamp>,
    width: usize,
) -> Vec<Line<'static>> {
    let Event::Message(message) = event else {
        return Vec::new();
    };
    let mut prefix = vec![
        watch_marker(app, event),
        Span::raw(day_label(app, message.first_seen_at, previous)).dim(),
        Span::raw(format!("{} ", clock(app, message.first_seen_at))).dim(),
    ];
    if app.channel.is_none() {
        let channel = format!("{:<14} ", truncate(&message.channel, 14));
        prefix.push(Span::styled(channel, name_color(&message.channel)));
    }
    let mut content = Vec::new();
    if let Some(sender) = &message.sender {
        content.push(Span::styled(sender.clone(), Style::new().bold().fg(name_color(sender))));
        content.push(Span::raw(": "));
    }
    content.push(Span::raw(message.body.clone()));
    let heard = app.heard(event);
    if heard > 1 {
        content.push(Span::raw(format!("  ×{heard}")).dim());
    }
    wrap(prefix, content, width)
}

fn draw_channels(frame: &mut Frame, area: Rect, app: &App, screen: &mut Screen) {
    let block = pane(Line::from("Channels"), app.sidebar_focus);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let unread = app.unread();
    let names: Vec<Option<&str>> = std::iter::once(None)
        .chain(app.channels.iter().map(|channel| Some(channel.name.as_str())))
        .collect();
    let selected = names.iter().position(|name| *name == app.channel.as_deref());
    let rows =
        window(names.len(), selected, usize::from(inner.height), &mut screen.channels, |_| 1);
    let width = usize::from(inner.width);
    let items: Vec<ListItem> = names[rows.clone()]
        .iter()
        .map(|name| {
            let count = match name {
                Some(name) => unread.get(name).copied().unwrap_or(0),
                None => unread.values().sum(),
            };
            let badge = if count > 0 { format!(" {count}") } else { String::new() };
            let label = truncate(name.unwrap_or("All"), width.saturating_sub(badge.len() + 1));
            let pad = width.saturating_sub(Span::raw(label.as_str()).width() + badge.len() + 1);
            let color = name.map_or(Color::Reset, name_color);
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {label}"), color),
                Span::raw(" ".repeat(pad)),
                Span::styled(badge, Style::new().bold().yellow()),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(selected.map(|index| index - rows.start));
    let style = if app.sidebar_focus { highlight() } else { Style::new().bold() };
    frame.render_stateful_widget(List::new(items).highlight_style(style), inner, &mut state);
}

/// Direct messages: who you've exchanged them with, and one conversation.
fn draw_dms(frame: &mut Frame, area: Rect, app: &App, screen: &mut Screen) {
    let [sidebar, thread] =
        Layout::horizontal([Constraint::Length(20), Constraint::Fill(1)]).areas(area);
    let conversations = app.conversations();
    let selected = app.selected_conversation(&conversations);
    draw_correspondents(frame, sidebar, app, &conversations, selected.map(|(at, _)| at), screen);

    let title = selected.map_or("Direct messages", |(_, thread)| thread.who.as_str());
    let block = pane(Line::from(title), !app.sidebar_focus);
    let inner = block.inner(thread);
    frame.render_widget(block, thread);

    if let Err(problem) = &app.dms {
        frame.render_widget(Paragraph::new(problem.as_str()).red(), inner);
        return;
    }
    let Some((_, conversation)) = selected else {
        let note = "no direct messages yet\n\nOthers can write to your radio once they have it \
                    as a contact, which they get from hearing it advertise (a).";
        frame.render_widget(Paragraph::new(note).dim().wrap(Wrap { trim: false }), inner);
        return;
    };

    let width = usize::from(inner.width);
    let mut lines = Vec::new();
    let mut previous = None;
    for message in &conversation.messages {
        lines.extend(dm_lines(app, message, previous, width));
        previous = Some(message.at);
    }
    // The newest message sits at the bottom; j/k scrolls back from there.
    let height = usize::from(inner.height);
    let end = lines.len().saturating_sub(app.dm_scroll.min(lines.len().saturating_sub(1)));
    let start = end.saturating_sub(height);
    let shown: Vec<Line> = lines[start..end].to_vec();
    frame.render_widget(Paragraph::new(shown), inner);
}

fn dm_lines(
    app: &App,
    message: &DmLine,
    previous: Option<Timestamp>,
    width: usize,
) -> Vec<Line<'static>> {
    let who = if message.outgoing { "you" } else { "them" };
    let prefix = vec![
        Span::raw(day_label(app, message.at, previous)).dim(),
        Span::raw(format!("{} ", clock(app, message.at))).dim(),
    ];
    let mut content = vec![Span::styled(
        format!("{who}: "),
        if message.outgoing {
            Style::new().bold().cyan()
        } else {
            Style::new().bold().fg(name_color(who))
        },
    )];
    content.push(Span::raw(message.body.clone()));
    if let Some(note) = delivery(message) {
        content.push(Span::raw(format!("  {note}")).dim());
    }
    wrap(prefix, content, width)
}

/// How a message got on: the acknowledgement for one you sent, the path for
/// one you received.
fn delivery(message: &DmLine) -> Option<String> {
    if message.outgoing {
        return match (message.status?, message.round_trip_ms) {
            (SendStatus::Failed, _) => {
                Some(format!("✗ {}", message.error.as_deref().unwrap_or("failed")))
            }
            (SendStatus::Delivered, Some(ms)) => Some(format!("✓ {:.1} s", f64::from(ms) / 1000.0)),
            (SendStatus::Delivered, None) => Some("✓".to_owned()),
            (SendStatus::Unacknowledged, _) => Some("no acknowledgement".to_owned()),
            _ => Some("sending…".to_owned()),
        };
    }
    match (message.hops, message.snr) {
        (Some(hops), Some(snr)) => Some(format!("{hops} hops, SNR {snr:.1}")),
        (Some(hops), None) => Some(format!("{hops} hops")),
        (None, Some(snr)) => Some(format!("direct, SNR {snr:.1}")),
        (None, None) => None,
    }
}

fn draw_correspondents(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    conversations: &[Conversation],
    selected: Option<usize>,
    screen: &mut Screen,
) {
    let block = pane(Line::from("People"), app.sidebar_focus);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = window(
        conversations.len(),
        selected,
        usize::from(inner.height),
        &mut screen.correspondents,
        |_| 1,
    );
    let width = usize::from(inner.width);
    let items: Vec<ListItem> = conversations[rows.clone()]
        .iter()
        .map(|thread| {
            let when = ago(app, thread.last_at());
            let label = truncate(&thread.who, width.saturating_sub(when.len() + 2));
            let pad = width.saturating_sub(Span::raw(label.as_str()).width() + when.len() + 2);
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {label}"), name_color(&thread.who)),
                Span::raw(" ".repeat(pad)),
                Span::raw(when).dim(),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(selected.map(|index| index - rows.start));
    let style = if app.sidebar_focus { highlight() } else { Style::new().bold() };
    frame.render_stateful_widget(List::new(items).highlight_style(style), inner, &mut state);
}

/// A bordered pane, brighter when it has the keyboard.
pub(super) fn pane(title: Line<'_>, focused: bool) -> Block<'_> {
    let border = if focused { Style::new().cyan() } else { Style::new().dark_gray() };
    Block::bordered().border_style(border).title(title.bold())
}

pub(super) fn highlight() -> Style {
    Style::new().bg(Color::Indexed(237))
}

/// Marks events a watch matches.
pub(super) fn watch_marker(app: &App, event: &Event) -> Span<'static> {
    if app.is_watched(event) { Span::styled("▌", Color::Yellow) } else { Span::raw(" ") }
}

pub(super) use super::app::at;

pub(super) fn clock(app: &App, at: Timestamp) -> String {
    at.to_zoned(app.zone.clone()).strftime("%H:%M:%S").to_string()
}

/// `Sep 12 ` on the first row of each day, blank otherwise.
pub(super) fn day_label(app: &App, at: Timestamp, previous: Option<Timestamp>) -> String {
    let day = at.to_zoned(app.zone.clone()).date();
    if previous.is_some_and(|previous| previous.to_zoned(app.zone.clone()).date() == day) {
        " ".repeat(7)
    } else {
        format!("{} ", day.strftime("%b %d"))
    }
}

/// How long ago, briefly: `42s`, `5m`, `3h`, `12d`.
pub(super) fn ago(app: &App, at: Timestamp) -> String {
    let seconds = app.now.duration_since(at).as_secs().max(0);
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

pub(super) fn name_color(name: &str) -> Color {
    PALETTE[name_hash(name) as usize % PALETTE.len()]
}

pub(super) fn type_color(payload_type: &str) -> Color {
    match payload_type {
        "GRP_TXT" | "TXT_MSG" => Color::LightCyan,
        "ADVERT" => Color::LightGreen,
        "ACK" | "PATH" | "TRACE" => Color::Blue,
        "REQ" | "RESPONSE" | "ANON_REQ" => Color::Magenta,
        _ => Color::Reset,
    }
}

/// LoRa SNR runs roughly from -20 dB (barely decodable) to +12 dB.
pub(super) fn snr_color(snr: f64) -> Color {
    if snr >= 0.0 {
        Color::Green
    } else if snr >= -7.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

pub(super) fn truncate(text: &str, width: usize) -> String {
    if Span::raw(text).width() <= width {
        return text.to_owned();
    }
    let (head, _) = split_at_width(text, width.saturating_sub(1));
    format!("{head}…")
}

/// Which items to draw from a list of `count`, keeping `selected` on screen
/// and scrolling no more than needed. `offset` is the first item shown, kept
/// between frames.
pub(super) fn window(
    count: usize,
    selected: Option<usize>,
    height: usize,
    offset: &mut usize,
    item_height: impl Fn(usize) -> usize,
) -> Range<usize> {
    if count == 0 {
        *offset = 0;
        return 0..0;
    }
    *offset = (*offset).min(count - 1);
    if let Some(selected) = selected {
        if selected < *offset {
            *offset = selected;
        } else {
            let (mut first, mut used) = (selected, item_height(selected));
            while first > *offset && used + item_height(first - 1) <= height {
                first -= 1;
                used += item_height(first);
            }
            *offset = first.max(*offset);
        }
    }
    // Pull earlier items in when the list ends before the screen does.
    let mut used = 0;
    let mut end = *offset;
    while end < count && used < height {
        used += item_height(end);
        end += 1;
    }
    while *offset > 0 && used + item_height(*offset - 1) <= height {
        *offset -= 1;
        used += item_height(*offset);
    }
    *offset..end
}

/// Word-wraps `content` after `prefix`, indenting later lines to line up
/// with the first, or by two when that leaves too little room.
pub(super) fn wrap(
    prefix: Vec<Span<'static>>,
    content: Vec<Span<'static>>,
    width: usize,
) -> Vec<Line<'static>> {
    let prefix_width: usize = prefix.iter().map(Span::width).sum();
    let indent = if width.saturating_sub(prefix_width) < 20 { 2 } else { prefix_width };
    let mut lines = Vec::new();
    let mut spans = prefix;
    let mut used = prefix_width;
    let mut fresh = true;
    for span in content {
        for mut word in span.content.split_inclusive(' ') {
            while !word.is_empty() {
                if !fresh && used + Span::raw(word.trim_end()).width() > width {
                    lines.push(Line::from(std::mem::take(&mut spans)));
                    spans.push(Span::raw(" ".repeat(indent)));
                    used = indent;
                    fresh = true;
                    continue;
                }
                // A word wider than the line breaks where the line ends.
                let (head, tail) = split_at_width(word, width.saturating_sub(used).max(1));
                used += Span::raw(head).width();
                spans.push(Span::styled(head.to_owned(), span.style));
                word = tail;
                fresh = false;
            }
        }
    }
    lines.push(Line::from(spans));
    lines
}

/// Splits after as many characters as fit in `width`, but at least one.
fn split_at_width(text: &str, width: usize) -> (&str, &str) {
    let mut used = 0;
    for (index, c) in text.char_indices() {
        let size = Span::raw(&text[index..index + c.len_utf8()]).width();
        if index > 0 && used + size > width {
            return text.split_at(index);
        }
        used += size;
    }
    (text, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn wrapping() {
        let lines = wrap(vec!["12:00 ".into()], vec!["one two three four".into()], 16);
        assert_eq!(text(&lines), ["12:00 one two ", "  three four"]);

        let lines = wrap(vec![], vec!["abcdefghij".into()], 4);
        assert_eq!(text(&lines), ["abcd", "  ef", "  gh", "  ij"]);
    }

    #[test]
    fn windows_scroll_only_as_needed() {
        let mut offset = 0;
        assert_eq!(window(100, Some(99), 10, &mut offset, |_| 1), 90..100);
        assert_eq!(window(100, Some(95), 10, &mut offset, |_| 1), 90..100);
        assert_eq!(window(100, Some(80), 10, &mut offset, |_| 1), 80..90);
        assert_eq!(window(3, Some(2), 10, &mut offset, |_| 1), 0..3);
        assert_eq!(window(0, None, 10, &mut offset, |_| 1), 0..0);

        let mut offset = 0;
        assert_eq!(window(10, Some(9), 10, &mut offset, |_| 3), 7..10);
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("#chattanooga", 8), "#chatta…");
        assert_eq!(truncate("#wx", 8), "#wx");
    }

    mod screens {
        use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
        use ferromesh_model::{
            ChannelInfo, DecodeState, DirectMessageInfo, MessageEvent, PacketDetail, PacketEvent,
            PacketReception, SentMessageInfo,
        };
        use jiff::tz::TimeZone;
        use meshcore_proto::{ChannelKey, GroupText};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        use super::super::*;
        use crate::tui::app::{Command, Update};

        const HASH: &str = "0123456789ABCDEF";

        fn app() -> App {
            let at: Timestamp = "2026-09-13T14:03:22Z".parse().unwrap();
            let mut app = App::new("mesh:7373".into(), Vec::new());
            app.zone = TimeZone::UTC;
            app.now = at;
            app.apply(Update::Channels(vec![ChannelInfo {
                name: "#wx".into(),
                kind: "hashtag".into(),
                hash: 0x42,
                enabled: true,
                added_at: at,
                messages: 1,
                last_message_at: Some(at),
            }]));
            app.apply(Update::Event(Event::Message(MessageEvent {
                id: 1,
                packet_hash: HASH.into(),
                first_seen_at: at,
                channel: "#wx".into(),
                sender: Some("Bob".into()),
                body: "storm rolling in".into(),
                sender_timestamp: 0,
                txt_type: 0,
                attempt: 0,
                heard: 3,
            })));
            for kind in Kind::ALL {
                app.apply(Update::CaughtUp(kind));
            }
            app
        }

        fn press(app: &mut App, code: KeyCode) {
            app.handle(TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
        }

        fn render(app: &App, width: u16, height: u16) -> Vec<String> {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, app, &mut Screen::default())).unwrap();
            text_lines(terminal.backend().buffer())
        }

        #[test]
        fn messages() {
            // Wide enough for the header's full status; it drops the labels
            // and the address when the tabs need the room.
            let lines = render(&app(), 120, 6);
            assert!(lines[0].starts_with(" ferromesh "), "{lines:#?}");
            assert!(lines[0].ends_with("● rf  mesh:7373"), "{lines:#?}");
            assert!(render(&app(), 100, 6)[0].ends_with("● ● ●"), "narrow header keeps its tabs");
            assert!(
                lines[2].contains("│ Sep 13 14:03:22 #wx            Bob: storm rolling in  ×3"),
                "{lines:#?}"
            );
            assert!(lines[3].starts_with("│ #wx"), "{lines:#?}");
            assert!(lines[5].starts_with("Tab channels"), "{lines:#?}");
        }

        #[test]
        fn inspector() {
            let mut app = app();
            press(&mut app, KeyCode::Enter);
            let text = GroupText {
                sender_timestamp: 1,
                txt_type: 0,
                attempt: 0,
                text: b"Bob: storm rolling in".to_vec(),
            };
            let frame = [
                &[0x15, 0x42, 0xAB, 0xCD, 0x11, 0x22][..],
                &ChannelKey::from_hashtag("#wx").encrypt(&text.to_plaintext()),
            ]
            .concat();
            let at = app.now;
            let detail = PacketDetail {
                packet: PacketEvent {
                    id: 1,
                    hash: HASH.into(),
                    payload_type: "GRP_TXT".into(),
                    first_seen_at: at,
                    last_seen_at: at,
                    heard: 1,
                    decode_state: DecodeState::Decrypted,
                    size: frame.len() as i64,
                    channel: Some("#wx".into()),
                    channel_hash: Some(0x42),
                    advert: None,
                    text: Some("Bob: storm rolling in".into()),
                },
                receptions: vec![PacketReception {
                    observation_id: 1,
                    observer: "Tanyard".into(),
                    rx_at: at,
                    snr: Some(-2.5),
                    rssi: Some(-101),
                    frame: hex::encode_upper(&frame),
                }],
            };
            app.apply(Update::Detail(HASH.into(), Ok(detail)));
            let lines = render(&app, 100, 30).join("\n");
            for expected in [
                "Packet 0123456789ABCDEF",
                "Receptions (1)",
                "Tanyard",
                "SNR   -2.5",
                "path length  42",
                "AB CD 11 22",
            ] {
                assert!(lines.contains(expected), "{expected:?} missing from\n{lines}");
            }
        }

        #[test]
        fn direct_messages() {
            let mut app = app();
            let at = app.now;
            app.apply(Update::Dms(Ok(vec![DirectMessageInfo {
                id: 1,
                received_at: at,
                to: "scw".into(),
                sender: Some("KK4SW".into()),
                sender_prefix: "d2aa11bb22cc".into(),
                hops: Some(2),
                txt_type: 0,
                sender_timestamp: at,
                snr: Some(-3.0),
                body: "are you there?".into(),
            }])));
            app.apply(Update::SentDms(vec![SentMessageInfo {
                id: 2,
                sent_at: at,
                from: "scw".into(),
                to: "KK4SW".into(),
                direct: true,
                body: "here now".into(),
                sender_timestamp: at,
                status: SendStatus::Delivered,
                error: None,
                round_trip_ms: Some(600),
                heard: 0,
                heard_by: Vec::new(),
                packet_hash: None,
            }]));

            press(&mut app, KeyCode::Char('2'));
            let lines = render(&app, 100, 20).join("\n");
            for expected in
                ["People", "KK4SW", "them: are you there?", "2 hops", "you: here now", "✓ 0.6 s"]
            {
                assert!(lines.contains(expected), "{expected:?} missing from\n{lines}");
            }

            // c replies to whoever the conversation is with.
            press(&mut app, KeyCode::Char('c'));
            for key in "yes".chars() {
                press(&mut app, KeyCode::Char(key));
            }
            let sent =
                app.handle(TermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
            assert_eq!(sent, [Command::Send { to: "KK4SW".into(), text: "yes".into() }]);
        }

        /// Every view and overlay draws, even on the smallest screen.
        #[test]
        fn every_view_at_any_size() {
            for (width, height) in [(40, 10), (100, 30)] {
                let mut app = app();
                for key in ['1', '2', '3', '4', '5', '6', '7'] {
                    press(&mut app, KeyCode::Char(key));
                    render(&app, width, height);
                    press(&mut app, KeyCode::Char('?'));
                    render(&app, width, height);
                    press(&mut app, KeyCode::Esc);
                }
                press(&mut app, KeyCode::Char('1'));
                press(&mut app, KeyCode::Enter);
                render(&app, width, height);
            }
        }
    }
}
