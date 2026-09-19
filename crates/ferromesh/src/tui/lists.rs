//! The table views: packets, RF, nodes and alerts.

use ferromesh_model::{Event, Kind, ObservationEvent, PacketEvent};
use jiff::Timestamp;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, List, ListItem, ListState, Paragraph, Row, Table, TableState};

use super::app::{App, View};
use super::ui::{self, Screen, pane};

/// SNR bar cells, spanning -20 dB to +12 dB.
const BAR: usize = 8;

pub fn draw_events(frame: &mut Frame, area: Rect, app: &App, screen: &mut Screen) {
    let view = app.view;
    let Some(kind) = view.kind() else {
        return;
    };
    let visible = app.visible(view);
    let title = format!("{} ({})", view.title(), visible.len());
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

pub fn draw_nodes(frame: &mut Frame, area: Rect, app: &App, screen: &mut Screen) {
    let nodes = app.visible_nodes();
    let block = pane(Line::from(format!("Nodes ({})", nodes.len())), true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if nodes.is_empty() {
        let note = if app.nodes.is_empty() { "no nodes yet" } else { "no nodes match" };
        frame.render_widget(Paragraph::new(note).dim(), inner);
        return;
    }
    let selected = Some(app.node_selected.min(nodes.len() - 1));
    let height = usize::from(inner.height.saturating_sub(1));
    let rows = ui::window(nodes.len(), selected, height, &mut screen.nodes, |_| 1);
    let body: Vec<Row> = nodes[rows.clone()]
        .iter()
        .map(|node| {
            let name = node.name.clone().unwrap_or_else(|| "(unnamed)".into());
            let location = match (node.lat, node.lon) {
                (Some(lat), Some(lon)) => format!("{lat:.4}, {lon:.4}"),
                _ => String::new(),
            };
            let first_seen = node.first_seen_at.to_zoned(app.zone.clone()).strftime("%Y-%m-%d");
            Row::new(vec![
                Cell::from(Span::styled(
                    name.clone(),
                    Style::new().bold().fg(ui::name_color(&name)),
                )),
                Cell::from(Span::raw(node.role.clone().unwrap_or_default()).dim()),
                Cell::from(Line::from(ui::ago(app, node.last_seen_at)).right_aligned()),
                Cell::from(Span::raw(first_seen.to_string()).dim()),
                Cell::from(Line::from(node.adverts.to_string()).right_aligned()),
                Cell::from(Span::raw(location).dim()),
                Cell::from(Span::raw(node.pubkey.clone()).dark_gray()),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(24),
        Constraint::Length(11),
        Constraint::Length(5),
        Constraint::Length(10),
        Constraint::Length(7),
        Constraint::Length(19),
        Constraint::Fill(1),
    ];
    let header =
        header(&["name", "role", "seen", "first seen", "adverts", "location", "public key"]);
    let mut state = TableState::default().with_selected(selected.map(|index| index - rows.start));
    let table = Table::new(body, widths).header(header).row_highlight_style(ui::highlight());
    frame.render_stateful_widget(table, inner, &mut state);
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
            spans.push(Span::raw(message.body.clone()));
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
