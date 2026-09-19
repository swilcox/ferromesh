//! How each observer is doing, for `ferromesh health` and the TUI's Health
//! view: the same figures, worded the same way.

use anyhow::Result;
use ferromesh_model::{HealthQuery, ObserverHealth, ObserverState};
use jiff::Timestamp;
use owo_colors::{OwoColorize, Stream};

use crate::server::Server;

const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// Longer histories merge neighbouring hours to stay this wide.
const MAX_BARS: usize = 48;

/// One figure: a label, its value, and an hourly trend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Figure {
    pub label: &'static str,
    pub value: String,
    pub trend: String,
}

/// `Tanyard (4d172767)`.
pub fn title(health: &ObserverHealth) -> String {
    let key = &health.pubkey[..8.min(health.pubkey.len())];
    match &health.name {
        Some(name) => format!("{name} ({key})"),
        None => key.to_owned(),
    }
}

/// `online · Heltec V4 OLED · v1.17.1 · up 6 d 22 h · reported 2 min ago`.
pub fn status(health: &ObserverHealth, now: Timestamp) -> String {
    let state = match health.state {
        ObserverState::Online => "online",
        ObserverState::Stale => "late",
        ObserverState::Offline => "offline",
    };
    let mut parts = vec![state.to_owned()];
    parts.extend(health.model.clone());
    parts.extend(health.firmware.clone());
    if let Some(up) = health.uptime_secs.filter(|_| health.state == ObserverState::Online) {
        parts.push(format!("up {}", duration(up)));
    }
    let ago = now.duration_since(health.last_report_at).as_secs().max(0);
    parts.push(format!("reported {} ago", duration(ago)));
    parts.join(" · ")
}

pub fn figures(health: &ObserverHealth) -> Vec<Figure> {
    let trend = |pick: fn(&ferromesh_model::HealthHour) -> Option<f64>| {
        let hourly: Vec<Option<f64>> = health.history.iter().map(pick).collect();
        sparkline(&condense(&hourly, hours_per_bar(hourly.len())))
    };
    let range = |min: Option<i64>, max: Option<i64>, show: fn(i64) -> String| match (min, max) {
        (Some(min), Some(max)) if min != max => format!("  ({} to {})", show(min), show(max)),
        _ => String::new(),
    };
    let volts = |mv: i64| format!("{:.2} V", mv as f64 / 1000.0);
    let dbm = |dbm: i64| format!("{dbm} dBm");
    let percent = |share: f64| format!("{:.1}%", share * 100.0);
    let missing = || "no data".to_owned();

    vec![
        Figure {
            label: "battery",
            value: health.battery_mv.map_or_else(missing, |mv| {
                volts(mv) + &range(health.battery_min_mv, health.battery_max_mv, volts)
            }),
            trend: trend(|hour| hour.battery_mv.map(|mv| mv as f64)),
        },
        Figure {
            label: "noise floor",
            value: health.noise_floor.map_or_else(missing, |now| {
                let usual =
                    health.noise_floor_median.map(|m| format!(", usually {m}")).unwrap_or_default();
                format!(
                    "{}{usual}{}",
                    dbm(now),
                    range(health.noise_floor_min, health.noise_floor_max, dbm)
                )
            }),
            trend: trend(|hour| hour.noise_floor.map(|dbm| dbm as f64)),
        },
        Figure {
            label: "packets",
            value: match (health.received_per_hour, health.sent_per_hour) {
                (Some(received), Some(sent)) => {
                    format!("{received:.0} received, {sent:.0} sent an hour")
                }
                (Some(received), None) => format!("{received:.0} received an hour"),
                _ => missing(),
            },
            trend: trend(|hour| Some(hour.received as f64)),
        },
        Figure {
            label: "errors",
            value: health.receive_error_share.map_or_else(missing, |share| {
                format!("{} of receptions didn't decode", percent(share))
            }),
            trend: trend(|hour| {
                let total = hour.received + hour.receive_errors;
                (total > 0).then(|| hour.receive_errors as f64 / total as f64)
            }),
        },
        Figure {
            label: "airtime",
            value: match (health.tx_air_share, health.rx_air_share) {
                (Some(tx), Some(rx)) => {
                    format!("transmitting {}, receiving {}", percent(tx), percent(rx))
                }
                _ => missing(),
            },
            trend: String::new(),
        },
        Figure {
            label: "delivery",
            value: health.delivered_share.map_or_else(missing, |share| {
                format!("{} of {} counted packets stored", percent(share), health.counted)
            }),
            trend: trend(|hour| {
                (hour.received > 0).then(|| (hour.stored as f64 / hour.received as f64).min(1.0))
            }),
        },
    ]
}

/// Hours each trend bar covers, so a history fits in [`MAX_BARS`].
pub fn hours_per_bar(hours: usize) -> usize {
    hours.div_ceil(MAX_BARS).max(1)
}

/// `trends: the last 24 hours, one bar an hour`.
pub fn trend_note(hours: usize) -> String {
    match hours_per_bar(hours) {
        1 => format!("trends: the last {hours} hours, one bar an hour"),
        n => format!("trends: the last {hours} hours, one bar per {n} hours"),
    }
}

/// Averages each run of `size` values, skipping missing ones.
fn condense(values: &[Option<f64>], size: usize) -> Vec<Option<f64>> {
    values
        .chunks(size)
        .map(|chunk| {
            let present: Vec<f64> = chunk.iter().flatten().copied().collect();
            (!present.is_empty()).then(|| present.iter().sum::<f64>() / present.len() as f64)
        })
        .collect()
}

/// One bar per value, scaled between the smallest and largest; a gap for
/// each missing one.
pub fn sparkline(values: &[Option<f64>]) -> String {
    let present: Vec<f64> = values.iter().flatten().copied().collect();
    let (Some(min), Some(max)) =
        (present.iter().copied().reduce(f64::min), present.iter().copied().reduce(f64::max))
    else {
        return String::new();
    };
    values
        .iter()
        .map(|value| match value {
            None => ' ',
            Some(_) if max <= min => BARS[3],
            Some(value) => {
                let level = ((value - min) / (max - min) * (BARS.len() - 1) as f64).round();
                BARS[level as usize]
            }
        })
        .collect()
}

/// `3 d 4 h`, `2 h 13 min`, `45 min`, `20 s`.
pub fn duration(secs: i64) -> String {
    let (days, hours, minutes) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{secs} s"),
        (0, 0, m) => format!("{m} min"),
        (0, h, m) => format!("{h} h {m} min"),
        (d, h, _) => format!("{d} d {h} h"),
    }
}

pub async fn show(server: &Server, hours: u32, json: bool) -> Result<()> {
    let path = server.path("/api/v1/observers", &HealthQuery { hours: Some(hours) })?;
    let observers: Vec<ObserverHealth> = server.get(&path).await?;
    if json {
        return crate::channels::json_lines(&observers);
    }
    if observers.is_empty() {
        crate::render::status("no observer has sent a status report yet");
        return Ok(());
    }
    let now = Timestamp::now();
    let mut out = String::new();
    for health in &observers {
        let title = title(health);
        out.push_str(&format!(
            "{}  {}\n",
            title.if_supports_color(Stream::Stdout, |t| t.bold()),
            status(health, now).if_supports_color(Stream::Stdout, |t| t.dimmed())
        ));
        for figure in figures(health) {
            let trend = figure.trend.if_supports_color(Stream::Stdout, |t| t.cyan()).to_string();
            out.push_str(&format!("  {:<12}{:<52} {trend}\n", figure.label, figure.value));
        }
        for warning in &health.warnings {
            out.push_str(&format!(
                "  {}\n",
                format!("! {warning}").if_supports_color(Stream::Stdout, |t| t.yellow())
            ));
        }
        out.push('\n');
    }
    let history = observers.first().map_or(hours as usize, |health| health.history.len());
    out.push_str(&trend_note(history));
    out.push('\n');
    crate::channels::print(&out)
}

#[cfg(test)]
mod tests {
    use ferromesh_model::HealthHour;

    use super::*;

    #[test]
    fn long_trends_are_condensed() {
        assert_eq!(
            (hours_per_bar(24), hours_per_bar(48), hours_per_bar(49), hours_per_bar(168)),
            (1, 1, 2, 4)
        );
        assert_eq!(
            condense(&[Some(1.0), Some(3.0), None, None, Some(5.0)], 2),
            [Some(2.0), None, Some(5.0)]
        );
        assert_eq!(trend_note(168), "trends: the last 168 hours, one bar per 4 hours");
    }

    #[test]
    fn sparklines() {
        assert_eq!(sparkline(&[Some(0.0), Some(7.0), None, Some(3.5)]), "▁█ ▅");
        assert_eq!(sparkline(&[Some(2.0), Some(2.0)]), "▄▄");
        assert_eq!(sparkline(&[None, None]), "");
    }

    #[test]
    fn figures_read_plainly() {
        let at: Timestamp = "2026-09-19T19:00:00Z".parse().unwrap();
        let hour = |received, stored| HealthHour {
            start: at,
            battery_mv: Some(4261),
            noise_floor: Some(-103),
            received,
            sent: 300,
            receive_errors: 400,
            stored,
        };
        let health = ObserverHealth {
            pubkey: "4d172767d319c09d".into(),
            name: Some("Tanyard".into()),
            model: Some("Heltec V4 OLED".into()),
            firmware: Some("v1.17.1".into()),
            radio: None,
            state: ObserverState::Online,
            last_report_at: at,
            uptime_secs: Some(597_953),
            reboots: 0,
            battery_mv: Some(4261),
            battery_min_mv: Some(4209),
            battery_max_mv: Some(4279),
            noise_floor: Some(-103),
            noise_floor_min: Some(-114),
            noise_floor_median: Some(-105),
            noise_floor_max: Some(-98),
            received_per_hour: Some(770.4),
            sent_per_hour: Some(330.0),
            receive_error_share: Some(0.329),
            tx_air_share: Some(0.0325),
            rx_air_share: Some(0.0718),
            counted: 18_480,
            stored: 18_480,
            delivered_share: Some(1.0),
            warnings: Vec::new(),
            history: vec![hour(700, 700), hour(800, 800)],
        };
        assert_eq!(title(&health), "Tanyard (4d172767)");
        let now: Timestamp = "2026-09-19T19:02:00Z".parse().unwrap();
        assert_eq!(
            status(&health, now),
            "online · Heltec V4 OLED · v1.17.1 · up 6 d 22 h · reported 2 min ago"
        );
        let values: Vec<String> = figures(&health).into_iter().map(|f| f.value).collect();
        assert_eq!(
            values,
            [
                "4.26 V  (4.21 V to 4.28 V)",
                "-103 dBm, usually -105  (-114 dBm to -98 dBm)",
                "770 received, 330 sent an hour",
                "32.9% of receptions didn't decode",
                "transmitting 3.2%, receiving 7.2%",
                "100.0% of 18480 counted packets stored",
            ]
        );
        assert_eq!(figures(&health)[2].trend, "▁█");
    }
}
