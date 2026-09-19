//! Boxes drawn over the view: the packet inspector and help.

use ferromesh_model::{Event, PacketDetail};
use meshcore_proto::{Packet, PayloadType};
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};

use super::app::{App, Inspector};
use super::inspect;
use super::lists;
use super::ui::{self, pane};

/// Frame bytes per hex line.
const HEX_PER_LINE: usize = 8;
const LABEL: usize = 13;
/// Field colours cycle so neighbouring fields are told apart.
const FIELD_COLORS: [Color; 4] = [Color::Cyan, Color::Yellow, Color::Green, Color::Magenta];

pub fn draw_inspector(frame: &mut Frame, area: Rect, app: &App, inspector: &Inspector) {
    let area = area.centered(Constraint::Percentage(94), Constraint::Percentage(94));
    frame.render_widget(Clear, area);
    let block = pane(Line::from(format!("Packet {}", inspector.hash)), true)
        .title_bottom(Line::from(" j/k scroll · Esc close ").dark_gray());
    let lines = match &inspector.detail {
        None => vec![Line::from("loading…").dim()],
        Some(Err(error)) => vec![Line::styled(error.clone(), Color::Red)],
        Some(Ok(detail)) => detail_lines(app, detail),
    };
    let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    frame.render_widget(paragraph.scroll((inspector.scroll, 0)), area);
}

fn detail_lines(app: &App, detail: &PacketDetail) -> Vec<Line<'static>> {
    let packet = &detail.packet;
    let local = |at: jiff::Timestamp| {
        at.to_zoned(app.zone.clone()).strftime("%Y-%m-%d %H:%M:%S").to_string()
    };
    let mut lines = vec![
        row(
            "type",
            vec![
                Span::styled(packet.payload_type.clone(), ui::type_color(&packet.payload_type)),
                Span::raw(format!(
                    "   heard ×{}   {} bytes",
                    app.heard(&Event::Packet(packet.clone())),
                    packet.size
                ))
                .dim(),
            ],
        ),
        row(
            "seen",
            vec![Span::raw(format!(
                "{} to {}",
                local(packet.first_seen_at),
                local(packet.last_seen_at)
            ))],
        ),
        row("decode", vec![Span::raw(packet.decode_state.as_str())]),
    ];
    if let Some(channel) = &packet.channel {
        lines.push(row("channel", vec![Span::styled(channel.clone(), ui::name_color(channel))]));
    } else if let Some(hash) = packet.channel_hash {
        lines.push(row("channel", vec![Span::raw(format!("hash {hash:02x}, no known key"))]));
    }
    if let Some(text) = &packet.text {
        lines.push(row("text", vec![Span::raw(text.clone())]));
    }
    if let Some(advert) = &packet.advert {
        let mut spans =
            vec![Span::raw(advert.name.clone().unwrap_or_else(|| "(unnamed)".into())).bold()];
        if let Some(role) = &advert.role {
            spans.push(Span::raw(format!("  {role}")));
        }
        if let (Some(lat), Some(lon)) = (advert.lat, advert.lon) {
            spans.push(Span::raw(format!("  {lat:.5}, {lon:.5}")));
        }
        spans.push(if advert.signature_ok {
            Span::styled("  signature ok", Color::Green)
        } else {
            Span::styled("  bad signature", Color::Red)
        });
        lines.push(row("advert", spans));
        lines.push(row("public key", vec![Span::raw(advert.pubkey.clone()).dim()]));
    }

    lines.push(Line::default());
    lines.push(Line::from(format!("Receptions ({})", detail.receptions.len())).bold());
    for reception in &detail.receptions {
        let frame = hex::decode(&reception.frame).unwrap_or_default();
        let mut spans = vec![
            Span::raw(format!("  {}  ", ui::clock(app, reception.rx_at))).dim(),
            Span::styled(
                format!("{:<10} ", reception.observer),
                ui::name_color(&reception.observer),
            ),
            reception.snr.map_or_else(
                || Span::raw(" ".repeat(15)),
                |snr| Span::styled(format!("SNR {snr:>6.1}  "), ui::snr_color(snr)),
            ),
            Span::raw(
                reception.rssi.map_or_else(|| " ".repeat(11), |rssi| format!("RSSI {rssi:>4}  ")),
            )
            .dim(),
        ];
        match Packet::parse(&frame) {
            Ok(parsed) => {
                spans.push(Span::raw(format!("{:<16}", parsed.route_type().name())).dim());
                let path = parsed.path();
                if parsed.payload_type() == PayloadType::Trace {
                    let snrs: Vec<String> = path
                        .as_bytes()
                        .iter()
                        .map(|&byte| format!("{:.2}", f64::from(byte as i8) / 4.0))
                        .collect();
                    spans.push(Span::raw(format!("SNRs so far: {}", snrs.join(" "))));
                } else {
                    let hops: Vec<String> = path.hops().map(hex::encode_upper).collect();
                    spans.extend(lists::path(app, &hops).spans);
                }
            }
            Err(error) => spans.push(Span::styled(error.to_string(), Color::Red)),
        }
        lines.push(Line::from(spans));
    }

    let Some(first) = detail.receptions.first() else {
        return lines;
    };
    let frame = hex::decode(&first.frame).unwrap_or_default();
    lines.push(Line::default());
    lines.push(
        Line::from(format!("Frame, as {} received it ({} bytes)", first.observer, frame.len()))
            .bold(),
    );
    let trace = packet.payload_type == "TRACE";
    for (index, field) in inspect::fields(&frame).into_iter().enumerate() {
        let color = FIELD_COLORS[index % FIELD_COLORS.len()];
        let value = if field.label == "path" && !trace {
            named_path(app, &field.value)
        } else {
            field.value
        };
        let bytes = &frame[field.bytes.clone()];
        for (chunk_index, chunk) in bytes.chunks(HEX_PER_LINE).enumerate() {
            let label = if chunk_index == 0 { field.label } else { "" };
            let hex: Vec<String> = chunk.iter().map(|byte| format!("{byte:02X}")).collect();
            let mut spans = vec![
                Span::raw(format!("  {label:<LABEL$}")).dim(),
                Span::styled(format!("{:<width$}", hex.join(" "), width = HEX_PER_LINE * 3), color),
            ];
            if chunk_index == 0 {
                spans.push(Span::raw(value.clone()));
            }
            lines.push(Line::from(spans));
        }
    }
    lines
}

/// `a1 b2` as `a1 (Hilltop) b2`.
fn named_path(app: &App, hops: &str) -> String {
    hops.split(' ')
        .map(|hop| match app.hop_name(hop) {
            Some(name) => format!("{hop} ({name})"),
            None => hop.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn row(label: &str, mut value: Vec<Span<'static>>) -> Line<'static> {
    value.insert(0, Span::raw(format!("  {label:<LABEL$}")).dim());
    Line::from(value)
}

const HELP: &[(&str, &str)] = &[
    ("1-5", "messages, packets, RF, nodes, alerts"),
    ("Tab", "channel list (messages), watch list (alerts)"),
    ("j k ↑ ↓", "move; PgUp PgDn by a page"),
    ("g Home", "oldest loaded; again to load older"),
    ("G End", "back to following the newest"),
    ("Enter", "inspect the selected packet"),
    ("/", "filter this view, Esc clears it"),
    ("w", "watch a filter: highlight and alert"),
    ("d", "delete the selected watch"),
    ("c", "clear alerts"),
    ("b", "bell on or off"),
    ("q", "quit"),
    ("", ""),
    ("chan:#a,#b", "channel          from:BNA*  sender"),
    ("storm", "words in text    type:advert"),
    ("'snr>-5'", "also rssi, hops   -term negates"),
];

pub fn draw_help(frame: &mut Frame, area: Rect) {
    let area = area.centered(Constraint::Length(62), Constraint::Length(HELP.len() as u16 + 3));
    frame.render_widget(Clear, area);
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!(" {key:<12}"), Style::new().bold().cyan()),
                Span::raw(*what),
            ])
        })
        .collect();
    let block =
        pane(Line::from("Keys"), true).title_bottom(Line::from(" any key closes ").dark_gray());
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
