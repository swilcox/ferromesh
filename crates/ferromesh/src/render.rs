//! Terminal output: one coloured line per event, or JSON lines.
//!
//! Colour is used only when the stream is a terminal, and never with `NO_COLOR`.

use std::fmt::Display;
use std::io::{self, Write};

use ferromesh_model::{Advert, Event, MessageEvent, ObservationEvent, PacketEvent};
use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use owo_colors::{AnsiColors, OwoColorize, Stream, Style};

/// Names are coloured by hash, so a channel or sender keeps its colour.
const PALETTE: [AnsiColors; 10] = [
    AnsiColors::Cyan,
    AnsiColors::Green,
    AnsiColors::Yellow,
    AnsiColors::Blue,
    AnsiColors::Magenta,
    AnsiColors::BrightCyan,
    AnsiColors::BrightGreen,
    AnsiColors::BrightYellow,
    AnsiColors::BrightBlue,
    AnsiColors::BrightMagenta,
];

pub struct Printer {
    json: bool,
    zone: TimeZone,
    day: Option<Date>,
}

impl Printer {
    pub fn new(json: bool) -> Self {
        Self { json, zone: TimeZone::system(), day: None }
    }

    pub fn event(&mut self, event: &Event) -> io::Result<()> {
        if self.json {
            return line(&serde_json::to_string(event).map_err(io::Error::other)?);
        }
        let at = match event {
            Event::Message(message) => message.first_seen_at,
            Event::Packet(packet) => packet.first_seen_at,
            Event::Observation(observation) => observation.rx_at,
        };
        let local = at.to_zoned(self.zone.clone());
        if self.day != Some(local.date()) {
            // Mark day changes, and a first line from any day but today.
            let today = Timestamp::now().to_zoned(self.zone.clone()).date();
            if self.day.is_some() || local.date() != today {
                line(&paint(&format!("── {} ──", local.strftime("%a %Y-%m-%d")), dim()))?;
            }
            self.day = Some(local.date());
        }
        let text = match event {
            Event::Message(message) => message_line(message),
            Event::Packet(packet) => packet_line(packet),
            Event::Observation(observation) => observation_line(observation),
        };
        line(&format!("{} {text}", paint(&local.strftime("%H:%M:%S").to_string(), dim())))
    }

    /// Marks where history ends and live traffic begins.
    pub fn live(&mut self) -> io::Result<()> {
        if self.json {
            return Ok(());
        }
        line(&paint("── live ──", dim()))
    }
}

/// A note on stderr, kept out of the event output.
pub fn status(message: impl Display) {
    let text = format!("· {message}");
    eprintln!("{}", text.if_supports_color(Stream::Stderr, |t| t.style(dim().yellow())));
}

fn line(text: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "{text}")?;
    out.flush()
}

fn message_line(message: &MessageEvent) -> String {
    let mut text = paint(&format!("{:<14}", message.channel), named(&message.channel));
    text.push(' ');
    if let Some(sender) = &message.sender {
        text.push_str(&paint(sender, named(sender).bold()));
        text.push_str(": ");
    }
    text.push_str(&message.body);
    push_heard(&mut text, message.heard);
    text
}

fn packet_line(packet: &PacketEvent) -> String {
    let mut text = paint(&format!("{:<9}", packet.payload_type), type_style(&packet.payload_type));
    text.push(' ');
    match (&packet.advert, &packet.channel, &packet.text) {
        (Some(advert), _, _) => text.push_str(&advert_label(advert)),
        (None, Some(channel), Some(message)) => {
            text.push_str(&paint(channel, named(channel)));
            text.push(' ');
            text.push_str(message);
        }
        _ => text.push_str(&paint(packet.decode_state.as_str(), dim())),
    }
    push_heard(&mut text, packet.heard);
    text
}

fn observation_line(observation: &ObservationEvent) -> String {
    let mut text = format!(
        "{} {} {}",
        paint(&format!("{:<10}", observation.observer), named(&observation.observer)),
        paint(&format!("{:<9}", observation.payload_type), type_style(&observation.payload_type)),
        paint(&format!("{:>2} hops", observation.hops.len()), dim()),
    );
    if let Some(snr) = observation.snr {
        text.push_str(&paint(&format!("  SNR {snr:>5.1}"), snr_style(snr)));
    }
    if let Some(rssi) = observation.rssi {
        text.push_str(&paint(&format!(" RSSI {rssi:>4}"), dim()));
    }
    if let Some(name) = &observation.advert_name {
        text.push_str("  ");
        text.push_str(&paint(name, Style::new().bold()));
    } else if let (Some(channel), Some(message)) = (&observation.channel, &observation.text) {
        text.push_str("  ");
        text.push_str(&paint(channel, named(channel)));
        text.push(' ');
        text.push_str(message);
    }
    if !observation.hops.is_empty() {
        text.push_str(&paint(&format!("  via {}", observation.hops.join(">")), dim()));
    }
    text
}

fn advert_label(advert: &Advert) -> String {
    let mut label = paint(advert.name.as_deref().unwrap_or("(unnamed)"), Style::new().bold());
    if let Some(role) = &advert.role {
        label.push_str(&paint(&format!(" {role}"), dim()));
    }
    if !advert.signature_ok {
        label.push_str(&paint(" bad signature", Style::new().red()));
    }
    label
}

fn push_heard(text: &mut String, heard: i64) {
    if heard > 1 {
        text.push_str(&paint(&format!("  ×{heard}"), dim()));
    }
}

fn paint(text: &str, style: Style) -> String {
    text.if_supports_color(Stream::Stdout, |t| t.style(style)).to_string()
}

fn dim() -> Style {
    Style::new().dimmed()
}

fn named(name: &str) -> Style {
    // FNV-1a over the lowercased name.
    let hash = name.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte.to_ascii_lowercase())).wrapping_mul(0x0100_0193)
    });
    Style::new().color(PALETTE[hash as usize % PALETTE.len()])
}

fn type_style(payload_type: &str) -> Style {
    match payload_type {
        "GRP_TXT" | "TXT_MSG" => Style::new().bright_cyan(),
        "ADVERT" => Style::new().bright_green(),
        "ACK" | "PATH" | "TRACE" => Style::new().blue(),
        "REQ" | "RESPONSE" | "ANON_REQ" => Style::new().magenta(),
        _ => Style::new(),
    }
}

/// LoRa SNR runs roughly from -20 dB (barely decodable) to +12 dB.
fn snr_style(snr: f64) -> Style {
    if snr >= 0.0 {
        Style::new().green()
    } else if snr >= -7.0 {
        Style::new().yellow()
    } else {
        Style::new().red()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferromesh_model::DecodeState;

    // Tests run with stdout captured, so no colour codes appear.

    #[test]
    fn message_lines() {
        let message = MessageEvent {
            id: 1,
            packet_hash: "00".into(),
            first_seen_at: Timestamp::UNIX_EPOCH,
            channel: "#test".into(),
            sender: Some("Bob".into()),
            body: "hi".into(),
            sender_timestamp: 0,
            txt_type: 0,
            attempt: 0,
            heard: 3,
        };
        assert_eq!(message_line(&message), "#test          Bob: hi  ×3");
    }

    #[test]
    fn packet_lines() {
        let packet = PacketEvent {
            id: 1,
            hash: "00".into(),
            payload_type: "ADVERT".into(),
            first_seen_at: Timestamp::UNIX_EPOCH,
            last_seen_at: Timestamp::UNIX_EPOCH,
            heard: 1,
            decode_state: DecodeState::Cleartext,
            size: 120,
            channel: None,
            channel_hash: None,
            advert: Some(Advert {
                pubkey: "ab".into(),
                name: Some("Hilltop".into()),
                role: Some("repeater".into()),
                lat: None,
                lon: None,
                signature_ok: true,
            }),
            text: None,
        };
        assert_eq!(packet_line(&packet), "ADVERT    Hilltop repeater");
    }

    #[test]
    fn observation_lines() {
        let observation = ObservationEvent {
            id: 1,
            packet_id: 1,
            hash: "00".into(),
            payload_type: "GRP_TXT".into(),
            rx_at: Timestamp::UNIX_EPOCH,
            observer: "Tanyard".into(),
            route: "flood".into(),
            hops: vec!["11".into(), "22".into()],
            snr: Some(-2.5),
            rssi: Some(-90),
            channel: Some("#test".into()),
            advert_pubkey: None,
            advert_name: None,
            text: Some("Bob: hi".into()),
        };
        assert_eq!(
            observation_line(&observation),
            "Tanyard    GRP_TXT    2 hops  SNR  -2.5 RSSI  -90  #test Bob: hi  via 11>22"
        );
    }
}
