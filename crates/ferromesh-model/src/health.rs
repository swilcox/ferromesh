//! How each observer is doing, from the status reports it sends.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

pub const DEFAULT_HEALTH_HOURS: u32 = 24;
pub const MAX_HEALTH_HOURS: u32 = 24 * 31;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthQuery {
    /// How far back the figures and history reach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hours: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObserverState {
    /// Reporting on schedule.
    Online,
    /// A report or two overdue.
    Stale,
    /// Not heard from for an hour or more.
    Offline,
}

/// `GET /api/v1/observers?hours=`: one observer's health over the last
/// `hours`. Figures are `None` when there were too few reports to work
/// them out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObserverHealth {
    /// Lowercase hex.
    pub pubkey: String,
    pub name: Option<String>,
    /// How ferromesh hears from it: `mqtt` for one publishing to a broker,
    /// `companion` for a radio on our own USB port. A companion can't report
    /// packets over 173 bytes, so less is expected of its delivery share.
    #[serde(default)]
    pub kind: String,
    pub model: Option<String>,
    pub firmware: Option<String>,
    /// `MHz,kHz,SF,CR`.
    pub radio: Option<String>,
    pub state: ObserverState,
    pub last_report_at: Timestamp,
    pub uptime_secs: Option<i64>,
    /// Restarts seen in the window: the uptime counter went backwards.
    pub reboots: u32,
    pub battery_mv: Option<i64>,
    pub battery_min_mv: Option<i64>,
    pub battery_max_mv: Option<i64>,
    /// dBm; lower is quieter.
    pub noise_floor: Option<i64>,
    pub noise_floor_min: Option<i64>,
    pub noise_floor_median: Option<i64>,
    pub noise_floor_max: Option<i64>,
    pub received_per_hour: Option<f64>,
    pub sent_per_hour: Option<f64>,
    /// Of everything picked up, the share that failed to decode.
    pub receive_error_share: Option<f64>,
    /// Share of the time spent transmitting: its duty cycle.
    pub tx_air_share: Option<f64>,
    pub rx_air_share: Option<f64>,
    /// Packets the observer counted receiving while its reports were
    /// continuous.
    pub counted: i64,
    /// Of those, the ones stored by ferromesh.
    pub stored: i64,
    /// `stored / counted`.
    pub delivered_share: Option<f64>,
    /// Things worth a look, in plain words.
    pub warnings: Vec<String>,
    /// Hourly, oldest first, covering the whole window.
    pub history: Vec<HealthHour>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthHour {
    pub start: Timestamp,
    /// Averages of the reports in the hour.
    pub battery_mv: Option<i64>,
    pub noise_floor: Option<i64>,
    /// Counted by the observer, from its reports.
    pub received: i64,
    pub sent: i64,
    pub receive_errors: i64,
    /// Of the hour's counted packets, the ones ferromesh stored.
    pub stored: i64,
}
