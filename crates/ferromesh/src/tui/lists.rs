//! The views besides messages: traffic, nodes, alerts, health, and organizing
//! channels.

use ferromesh_model::{Event, Guess, Kind, ObservationEvent, ObserverState, PacketEvent};
use jiff::Timestamp;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap};

use super::app::{App, View};
use super::ui::{self, Screen, pane};
use crate::health;

/// SNR bar cells, spanning -20 dB to +12 dB.
const BAR: usize = 8;

pub fn draw_events(frame: &mut Frame, area: Rect, app: &App, screen: &mut Screen) {
    let view = app.view;
    let Some(kind) = view.kind() else {
        return;
    };
    let visible = app.visible(view);
    let title = format!("Traffic · {} ({})", view.title().to_lowercase(), visible.len());
    let block = pane(Line::from(title), true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if visible.is_empty() {
        let note = if app.feed(kind).live { "nothing yet" } else { "loading…" };
        frame.render_widget(Paragraph::new(note).dim(), inner);
        return;
    }

    let selected = app.selected(view, &visible);
    let offset = if view == View::Packets { &mut screen.packets } else { &mut screen.rf };
    let height = usize::from(inner.height.saturating_sub(1));
    let rows = ui::window(visible.len(), selected, height, offset, |_| 1);
    let body: Vec<Row> = rows
        .clone()
        .map(|index| {
            // The top row always carries its date.
            let previous = (index > rows.start).then(|| ui::at(visible[index - 1]));
            match visible[index] {
                Event::Packet(packet) => packet_row(app, visible[index], packet, previous),
                Event::Observation(observation) => {
                    observation_row(app, visible[index], observation, previous)
                }
                Event::Message(_) => Row::new(Vec::<Cell>::new()),
            }
        })
        .collect();

    let (header, widths) = if view == View::Packets {
        (
            header(&["", "", "time", "type", "heard", "size", "", "hash"]),
            vec![
                Constraint::Length(1),
                Constraint::Length(6),
                Constraint::Length(8),
                Constraint::Length(9),
                Constraint::Length(5),
                Constraint::Length(5),
                Constraint::Fill(1),
                Constraint::Length(16),
            ],
        )
    } else {
        (
            header(&[
                "", "", "time", "observer", "type", "route", "hops", "SNR", "RSSI", "", "path",
            ]),
            vec![
                Constraint::Length(1),
                Constraint::Length(6),
                Constraint::Length(8),
                Constraint::Length(10),
                Constraint::Length(9),
                Constraint::Length(8),
                Constraint::Length(4),
                Constraint::Length(BAR as u16 + 6),
                Constraint::Length(4),
                Constraint::Fill(2),
                Constraint::Fill(3),
            ],
        )
    };
    let mut state = TableState::default().with_selected(selected.map(|index| index - rows.start));
    let table = Table::new(body, widths).header(header).row_highlight_style(ui::highlight());
    frame.render_stateful_widget(table, inner, &mut state);
}

fn header(titles: &[&'static str]) -> Row<'static> {
    Row::new(titles.iter().copied()).style(Style::new().bold().dark_gray())
}

fn packet_row(
    app: &App,
    event: &Event,
    packet: &PacketEvent,
    previous: Option<Timestamp>,
) -> Row<'static> {
    let state = Span::raw(packet.decode_state.as_str()).dim();
    let description = match (&packet.advert, &packet.channel, &packet.text) {
        (Some(advert), _, _) => {
            let mut spans = vec![
                Span::raw(advert.name.clone().unwrap_or_else(|| "(unnamed)".into())).bold(),
                Span::raw(format!(" {}", advert.role.as_deref().unwrap_or_default())).dim(),
            ];
            if !advert.signature_ok {
                spans.push(Span::styled(" bad signature", Color::Red));
            }
            Line::from(spans)
        }
        (None, Some(channel), text) => Line::from(vec![
            Span::styled(format!("{channel} "), ui::name_color(channel)),
            text.as_ref().map_or(state, |text| Span::raw(text.clone())),
        ]),
        (None, None, _) => match packet.channel_hash {
            Some(hash) => Line::from(vec![Span::raw(format!("channel {hash:02x} ")).dim(), state]),
            None => Line::from(state),
        },
    };
    Row::new(vec![
        Cell::from(ui::watch_marker(app, event)),
        Cell::from(day(app, packet.first_seen_at, previous)),
        Cell::from(Span::raw(ui::clock(app, packet.first_seen_at)).dim()),
        Cell::from(Span::styled(packet.payload_type.clone(), ui::type_color(&packet.payload_type))),
        Cell::from(Line::from(format!("×{}", app.heard(event))).right_aligned()),
        Cell::from(Line::from(format!("{}b", packet.size)).right_aligned().dim()),
        Cell::from(description),
        Cell::from(Span::raw(packet.hash.clone()).dark_gray()),
    ])
}

fn observation_row(
    app: &App,
    event: &Event,
    observation: &ObservationEvent,
    previous: Option<Timestamp>,
) -> Row<'static> {
    let what = match (&observation.advert_name, &observation.channel, &observation.text) {
        (Some(name), _, _) => Line::from(Span::raw(name.clone()).bold()),
        (None, Some(channel), text) => Line::from(vec![
            Span::styled(format!("{channel} "), ui::name_color(channel)),
            Span::raw(text.clone().unwrap_or_default()),
        ]),
        (None, None, _) => Line::from(Span::raw(observation.hash.clone()).dark_gray()),
    };
    Row::new(vec![
        Cell::from(ui::watch_marker(app, event)),
        Cell::from(day(app, observation.rx_at, previous)),
        Cell::from(Span::raw(ui::clock(app, observation.rx_at)).dim()),
        Cell::from(Span::styled(
            observation.observer.clone(),
            ui::name_color(&observation.observer),
        )),
        Cell::from(Span::styled(
            observation.payload_type.clone(),
            ui::type_color(&observation.payload_type),
        )),
        Cell::from(Span::raw(observation.route.replace("transport-", "t-")).dim()),
        Cell::from(Line::from(observation.hops.len().to_string()).right_aligned()),
        Cell::from(observation.snr.map_or_else(Line::default, snr_bar)),
        Cell::from(
            Line::from(observation.rssi.map(|rssi| rssi.to_string()).unwrap_or_default())
                .right_aligned()
                .dim(),
        ),
        Cell::from(what),
        Cell::from(path(app, &observation.hops)),
    ])
}

fn day(app: &App, at: Timestamp, previous: Option<Timestamp>) -> Span<'static> {
    Span::raw(ui::day_label(app, at, previous).trim_end().to_owned()).dim()
}

pub(super) fn snr_bar(snr: f64) -> Line<'static> {
    let filled = ((snr + 20.0) / 32.0 * BAR as f64).round().clamp(0.0, BAR as f64) as usize;
    Line::from(vec![
        Span::styled("█".repeat(filled), ui::snr_color(snr)),
        Span::raw("░".repeat(BAR - filled)).dark_gray(),
        Span::styled(format!("{snr:>6.1}"), ui::snr_color(snr)),
    ])
}

/// Hops by node name where the prefix names exactly one node.
pub(super) fn path(app: &App, hops: &[String]) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, hop) in hops.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" › ").dark_gray());
        }
        spans.push(match app.hop_name(hop) {
            Some(name) => Span::styled(ui::truncate(name, 12), ui::name_color(name)),
            None => Span::raw(hop.clone()).dim(),
        });
    }
    Line::from(spans)
}

/// Every node heard advertising, with what the radio holds of each: kept
/// (a favourite, never replaced), on the radio, or the contact it would
/// replace next. On the radio's own list, contacts come in its order, by the
/// advert each last sent: that timestamp is the sender's own clock and some
/// are wrong, but it's what the radio goes by when it makes room.
pub fn draw_nodes(frame: &mut Frame, area: Rect, app: &App, screen: &mut Screen) {
    let rows_shown = app.node_rows();
    let held = app.contacts.as_ref().map(Vec::len).unwrap_or_default();
    let title = if app.radio_only {
        format!("Nodes · on the radio ({} of {held})", rows_shown.len())
    } else {
        format!("Nodes ({}) · {held} on the radio", rows_shown.len())
    };
    let block = pane(Line::from(title), true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if let (true, Err(error)) = (app.radio_only, &app.contacts) {
        let note = format!("couldn't load the radio's contacts: {error}");
        frame.render_widget(Paragraph::new(note).red().wrap(Wrap { trim: true }), inner);
        return;
    }
    if rows_shown.is_empty() {
        let note = match (app.radio_only, app.filter_text(View::Nodes)) {
            (_, Some(_)) => "no nodes match",
            (true, None) => {
                "the radio has no contacts yet; it adds chat radios as it hears them advertise"
            }
            (false, None) => "no nodes yet",
        };
        frame.render_widget(Paragraph::new(note).dim().wrap(Wrap { trim: true }), inner);
        return;
    }
    let next_out = app.next_replaced();
    let selected = Some(app.node_selected.min(rows_shown.len() - 1));
    let height = usize::from(inner.height.saturating_sub(1));
    let rows = ui::window(rows_shown.len(), selected, height, &mut screen.nodes, |_| 1);
    let body: Vec<Row> = rows_shown[rows.clone()]
        .iter()
        .map(|row| {
            let name = row.name().unwrap_or("(unnamed)").to_owned();
            let favourite = row.contact.is_some_and(|contact| contact.favourite);
            let radio = match row.contact {
                Some(contact) if contact.favourite => Span::raw("kept").cyan(),
                Some(contact) if next_out == Some(contact.pubkey.as_str()) => {
                    Span::raw("next out").yellow()
                }
                Some(_) => Span::raw("on radio").dim(),
                None => Span::raw(""),
            };
            let route = row.contact.map_or_else(String::new, |contact| {
                contact.route_hops.map_or_else(|| "flood".into(), |hops| format!("{hops} hops"))
            });
            // Heard by the observers, or failing that the clock in the
            // advert the radio holds, which is the sender's and may be off.
            let seen = match (row.node, row.contact.and_then(|contact| contact.last_advert)) {
                (Some(node), _) => Span::raw(ui::ago(app, node.last_seen_at)),
                (None, Some(at)) => {
                    Span::raw(at.to_zoned(app.zone.clone()).strftime("%Y-%m-%d").to_string()).dim()
                }
                (None, None) => Span::raw(""),
            };
            let location = match row.node.and_then(|node| node.lat.zip(node.lon)) {
                Some((lat, lon)) => format!("{lat:.4}, {lon:.4}"),
                None => String::new(),
            };
            let adverts = row.node.map(|node| node.adverts.to_string()).unwrap_or_default();
            Row::new(vec![
                Cell::from(Span::raw(if favourite { "★" } else { " " }).cyan()),
                Cell::from(Span::styled(
                    name.clone(),
                    Style::new().bold().fg(ui::name_color(&name)),
                )),
                Cell::from(Span::raw(row.role().unwrap_or_default().to_owned()).dim()),
                Cell::from(Line::from(seen).right_aligned()),
                Cell::from(Line::from(adverts).right_aligned()),
                Cell::from(radio),
                Cell::from(Span::raw(route).dim()),
                Cell::from(Span::raw(location).dim()),
                Cell::from(Span::raw(row.pubkey.to_owned()).dark_gray()),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(1),
        Constraint::Length(24),
        Constraint::Length(11),
        Constraint::Length(10),
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Length(19),
        Constraint::Fill(1),
    ];
    let header = header(&[
        "",
        "name",
        "role",
        "seen",
        "adverts",
        "radio",
        "route",
        "location",
        "public key",
    ]);
    let mut state = TableState::default().with_selected(selected.map(|index| index - rows.start));
    let table = Table::new(body, widths).header(header).row_highlight_style(ui::highlight());
    frame.render_stateful_widget(table, inner, &mut state);
}

/// Each observer's health: its state, figures with hourly trends, and
/// warnings.
pub fn draw_health(frame: &mut Frame, area: Rect, app: &App) {
    let block = pane(Line::from("Health"), true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let observers = match &app.health {
        Err(error) => {
            let note = format!("couldn't load observer health: {error}");
            frame.render_widget(
                Paragraph::new(note).red().wrap(ratatui::widgets::Wrap { trim: true }),
                inner,
            );
            return;
        }
        Ok(observers) if observers.is_empty() => {
            frame.render_widget(
                Paragraph::new("no observer has sent a status report yet").dim(),
                inner,
            );
            return;
        }
        Ok(observers) => observers,
    };
    let mut lines = Vec::new();
    for observer in observers {
        let color = match observer.state {
            ObserverState::Online => Color::Green,
            ObserverState::Stale => Color::Yellow,
            ObserverState::Offline => Color::Red,
        };
        lines.push(Line::from(vec![
            Span::styled("● ", color),
            Span::raw(health::title(observer)).bold(),
            Span::raw("  "),
            Span::raw(health::status(observer, app.now)).dim(),
        ]));
        for figure in health::figures(observer) {
            lines.push(Line::from(vec![
                Span::raw(format!("  {:<12}", figure.label)).dim(),
                Span::raw(format!("{:<52} ", figure.value)),
                Span::styled(figure.trend, Color::Cyan),
            ]));
        }
        for warning in &observer.warnings {
            lines.push(Line::from(Span::styled(format!("  ! {warning}"), Color::Yellow)));
        }
        lines.push(Line::default());
    }
    let hours = observers.first().map_or(24, |observer| observer.history.len());
    lines.push(Line::from(health::trend_note(hours)).dim());
    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn draw_alerts(frame: &mut Frame, area: Rect, app: &App, screen: &mut Screen) {
    let [left, right] =
        Layout::horizontal([Constraint::Length(36), Constraint::Fill(1)]).areas(area);

    let block = pane(Line::from(format!("Watches ({})", app.watches.len())), app.watch_focus);
    let inner = block.inner(left);
    frame.render_widget(block, left);
    if app.watches.is_empty() {
        let note = "none yet: press w in a view to watch its filter";
        frame.render_widget(
            Paragraph::new(note).dim().wrap(ratatui::widgets::Wrap { trim: true }),
            inner,
        );
    } else {
        let selected = Some(app.watch_selected.min(app.watches.len() - 1));
        let rows = ui::window(
            app.watches.len(),
            selected,
            usize::from(inner.height),
            &mut screen.watches,
            |_| 1,
        );
        let items: Vec<ListItem> = app.watches[rows.clone()]
            .iter()
            .map(|watch| {
                let kind = match watch.config.kind {
                    Kind::Messages => "msg ",
                    Kind::Packets => "pkt ",
                    Kind::Observations => "rf  ",
                };
                ListItem::new(Line::from(vec![
                    Span::raw(kind).dim(),
                    Span::raw(watch.config.filter.clone()),
                ]))
            })
            .collect();
        let mut state =
            ListState::default().with_selected(selected.map(|index| index - rows.start));
        let style = if app.watch_focus { ui::highlight() } else { Style::new() };
        frame.render_stateful_widget(List::new(items).highlight_style(style), inner, &mut state);
    }

    let bell = if app.bell_enabled { "bell on" } else { "bell off" };
    let title = Line::from(format!("Alerts ({}) · {bell}", app.alerts.len()));
    let block = pane(title, !app.watch_focus);
    let inner = block.inner(right);
    frame.render_widget(block, right);
    if app.alerts.is_empty() {
        frame.render_widget(Paragraph::new("no alerts since starting").dim(), inner);
        return;
    }
    let selected = Some(app.alert_selected.min(app.alerts.len() - 1));
    let rows = ui::window(
        app.alerts.len(),
        selected,
        usize::from(inner.height),
        &mut screen.alerts,
        |_| 1,
    );
    let items: Vec<ListItem> = app
        .alerts
        .iter()
        .rev()
        .skip(rows.start)
        .take(rows.len())
        .map(|alert| {
            let mut spans = vec![
                Span::raw(format!("{} ", ui::clock(app, ui::at(&alert.event)))).dim(),
                Span::styled(format!("{} ", ui::truncate(&alert.watch, 16)), Color::Yellow),
            ];
            spans.extend(summary(&alert.event));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default().with_selected(selected.map(|index| index - rows.start));
    let style = if app.watch_focus { Style::new() } else { ui::highlight() };
    frame.render_stateful_widget(List::new(items).highlight_style(style), inner, &mut state);
}

/// One event, briefly.
fn summary(event: &Event) -> Vec<Span<'static>> {
    match event {
        Event::Message(message) => {
            let mut spans = vec![Span::styled(
                format!("{} ", message.channel),
                ui::name_color(&message.channel),
            )];
            if let Some(sender) = &message.sender {
                spans.push(Span::raw(format!("{sender}: ")).bold());
            }
            spans.extend(ui::body_spans(&message.body));
            spans
        }
        Event::Packet(packet) => {
            let what = match (&packet.advert, &packet.text) {
                (Some(advert), _) => advert.name.clone().unwrap_or_default(),
                (None, Some(text)) => text.clone(),
                (None, None) => packet.decode_state.as_str().to_owned(),
            };
            vec![
                Span::styled(
                    format!("{} ", packet.payload_type),
                    ui::type_color(&packet.payload_type),
                ),
                Span::raw(what),
            ]
        }
        Event::Observation(observation) => {
            let snr = observation.snr.map(|snr| format!(" SNR {snr:.1}")).unwrap_or_default();
            vec![
                Span::styled(
                    format!("{} ", observation.observer),
                    ui::name_color(&observation.observer),
                ),
                Span::styled(
                    observation.payload_type.clone(),
                    ui::type_color(&observation.payload_type),
                ),
                Span::raw(format!("{snr} {} hops", observation.hops.len())).dim(),
            ]
        }
    }
}

/// The channels the server decrypts, above the channel hashes on stored
/// traffic that no known key opens, with any names a guess found for them.
pub fn draw_channels(frame: &mut Frame, area: Rect, app: &App, screen: &mut Screen) {
    let undecrypted = app.undecrypted_list();
    let channels = app.ordered_channels();
    // The known list takes the room it needs, up to half, leaving the rest
    // to traffic still waiting for a key.
    let wanted = (app.channels.len().max(1) as u16 + 3).min(area.height / 2);
    let [top, bottom] =
        Layout::vertical([Constraint::Length(wanted), Constraint::Fill(1)]).areas(area);

    let title = format!("Channels ({}) · {}", channels.len(), app.channel_sort.label());
    let block = pane(Line::from(title), !app.undecrypted_focus);
    let inner = block.inner(top);
    frame.render_widget(block, top);
    if channels.is_empty() {
        frame.render_widget(Paragraph::new("no channels yet; + adds one").dim(), inner);
    } else {
        let selected = Some(app.channel_selected.min(channels.len() - 1));
        let height = usize::from(inner.height.saturating_sub(1));
        let rows = ui::window(channels.len(), selected, height, &mut screen.channel_list, |_| 1);
        let body: Vec<Row> = channels[rows.clone()]
            .iter()
            .map(|channel| {
                let last = channel.last_message_at.map(|at| ui::ago(app, at)).unwrap_or_default();
                Row::new(vec![
                    Cell::from(Span::styled(channel.name.clone(), ui::name_color(&channel.name))),
                    Cell::from(Span::raw(channel.kind.clone()).dim()),
                    Cell::from(Span::raw(format!("{:02x}", channel.hash)).dim()),
                    Cell::from(Line::from(channel.messages.to_string()).right_aligned()),
                    Cell::from(Line::from(last).right_aligned()),
                    Cell::from(Span::raw(channel.scope.label().to_owned()).dim()),
                ])
            })
            .collect();
        let widths = [
            Constraint::Length(24),
            Constraint::Length(8),
            Constraint::Length(4),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Fill(1),
        ];
        let header = header(&["name", "kind", "hash", "messages", "last message", "scope"]);
        let mut state = TableState::default()
            .with_selected(selected.filter(|_| !app.undecrypted_focus).map(|i| i - rows.start));
        let table = Table::new(body, widths).header(header).row_highlight_style(ui::highlight());
        frame.render_stateful_widget(table, inner, &mut state);
    }

    let block =
        pane(Line::from(format!("Undecrypted ({})", undecrypted.len())), app.undecrypted_focus);
    let inner = block.inner(bottom);
    frame.render_widget(block, bottom);
    let note = match &app.undecrypted {
        None => Some(Span::raw("loading…").dim()),
        Some(Err(error)) => {
            Some(Span::raw(format!("couldn't load undecrypted traffic: {error}")).red())
        }
        Some(Ok(list)) if list.is_empty() => {
            Some(Span::raw("every stored channel packet opens with a known key").dim())
        }
        Some(Ok(_)) => None,
    };
    if let Some(note) = note {
        frame.render_widget(Paragraph::new(Line::from(note)).wrap(Wrap { trim: true }), inner);
        return;
    }
    let selected = Some(app.undecrypted_selected.min(undecrypted.len() - 1));
    let height = usize::from(inner.height.saturating_sub(1));
    let rows = ui::window(undecrypted.len(), selected, height, &mut screen.undecrypted, |_| 1);
    let body: Vec<Row> = undecrypted[rows.clone()]
        .iter()
        .map(|unknown| {
            let guesses = app.guesses_for(unknown.hash);
            let named = |guess: &Guess| {
                let s = if guess.messages == 1 { "" } else { "s" };
                format!("{} ({} msg{s})", guess.name, guess.messages)
            };
            let guess = match guesses.as_slice() {
                [] => Span::raw(""),
                [only] => Span::raw(named(only)).green(),
                [first, rest @ ..] => {
                    Span::raw(format!("{} +{}", named(first), rest.len())).green()
                }
            };
            let shares = unknown
                .shares_hash_with
                .as_ref()
                .map(|name| format!("same hash as {name}"))
                .unwrap_or_default();
            Row::new(vec![
                Cell::from(Span::raw(format!("{:02x}", unknown.hash)).bold()),
                Cell::from(Line::from(unknown.packets.to_string()).right_aligned()),
                Cell::from(Line::from(unknown.heard.to_string()).right_aligned()),
                Cell::from(Line::from(unknown.text_packets.to_string()).right_aligned()),
                Cell::from(Line::from(ui::ago(app, unknown.last_seen_at)).right_aligned()),
                Cell::from(guess),
                Cell::from(Span::raw(shares).dark_gray()),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(4),
        Constraint::Length(7),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(5),
        Constraint::Length(30),
        Constraint::Fill(1),
    ];
    let header = header(&["hash", "packets", "heard", "text", "seen", "guess", ""]);
    let mut state = TableState::default()
        .with_selected(selected.filter(|_| app.undecrypted_focus).map(|i| i - rows.start));
    let table = Table::new(body, widths).header(header).row_highlight_style(ui::highlight());
    frame.render_stateful_widget(table, inner, &mut state);
}
