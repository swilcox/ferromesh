//! Observer health from status reports. Rates come from counter differences
//! between consecutive reports; a pair where the uptime went backwards spans
//! a restart and is left out. The delivery check compares the packets an
//! observer counted in each pair with the observations stored from it.

use ferromesh_model::{HealthHour, ObserverHealth, ObserverState};
use jiff::Timestamp;
use rusqlite::{Connection, Row, params};

use crate::{Micros, Result};

const SECOND: Micros = 1_000_000;
const HOUR: Micros = 3600 * SECOND;
/// Observers report every 5 minutes; this allows two to go missing.
const STALE_AFTER: Micros = 12 * 60 * SECOND;
const OFFLINE_AFTER: Micros = HOUR;
const LOW_BATTERY_MV: i64 = 3600;
/// A noise floor this far above its median is worth a look.
const NOISE_JUMP_DB: i64 = 10;
/// The delivery check needs enough packets to mean anything.
const MIN_COUNTED: i64 = 100;
const DELIVERY_WARNING_BELOW: f64 = 0.99;

#[derive(Debug, Clone)]
struct Report {
    at: Micros,
    model: Option<String>,
    firmware: Option<String>,
    radio: Option<String>,
    battery_mv: Option<i64>,
    uptime_secs: Option<i64>,
    noise_floor: Option<i64>,
    tx_air_secs: Option<i64>,
    rx_air_secs: Option<i64>,
    sent: Option<i64>,
    received: Option<i64>,
    errors: Option<i64>,
}

impl Report {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            at: row.get(0)?,
            model: row.get(1)?,
            firmware: row.get(2)?,
            radio: row.get(3)?,
            battery_mv: row.get(4)?,
            uptime_secs: row.get(5)?,
            noise_floor: row.get(6)?,
            tx_air_secs: row.get(7)?,
            rx_air_secs: row.get(8)?,
            sent: row.get(9)?,
            received: row.get(10)?,
            errors: row.get(11)?,
        })
    }

    /// Observers report 0 before they've measured anything.
    fn noise(&self) -> Option<i64> {
        self.noise_floor.filter(|&dbm| dbm < 0)
    }

    fn battery(&self) -> Option<i64> {
        self.battery_mv.filter(|&mv| mv > 0)
    }
}

const REPORT_COLUMNS: &str =
    "at, model, firmware_version, radio, battery_mv, uptime_secs, noise_floor,
     tx_air_secs, rx_air_secs, packets_sent, packets_received, recv_errors";

/// Every observer that has sent a status report, by name.
pub(crate) fn observer_health(
    conn: &Connection,
    now: Micros,
    hours: u32,
) -> Result<Vec<ObserverHealth>> {
    let observers: Vec<(i64, Vec<u8>, Option<String>)> = conn
        .prepare_cached(
            "SELECT o.id, o.pubkey, o.name FROM observers o
             WHERE EXISTS (SELECT 1 FROM observer_status s WHERE s.observer_id = o.id)
             ORDER BY o.name, o.pubkey",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    observers
        .into_iter()
        .map(|(id, pubkey, name)| health(conn, id, &pubkey, name, now, hours))
        .collect()
}

fn health(
    conn: &Connection,
    observer_id: i64,
    pubkey: &[u8],
    name: Option<String>,
    now: Micros,
    hours: u32,
) -> Result<ObserverHealth> {
    let hours = hours.max(1);
    let since = now - i64::from(hours) * HOUR;
    let latest = conn
        .prepare_cached(&format!(
            "SELECT {REPORT_COLUMNS} FROM observer_status WHERE observer_id = ?1 ORDER BY at DESC LIMIT 1"
        ))?
        .query_row([observer_id], Report::read)?;
    let reports: Vec<Report> = conn
        .prepare_cached(&format!(
            "SELECT {REPORT_COLUMNS} FROM observer_status
             WHERE observer_id = ?1 AND at > ?2 AND at <= ?3 ORDER BY at"
        ))?
        .query_map(params![observer_id, since, now], Report::read)?
        .collect::<rusqlite::Result<_>>()?;

    // Hourly buckets, the last holding `now`.
    let last_start = now.div_euclid(HOUR) * HOUR;
    let first_start = last_start - i64::from(hours - 1) * HOUR;
    let bucket = |at: Micros| usize::try_from((at - first_start).div_euclid(HOUR)).ok();
    let mut history: Vec<HealthHour> = (0..hours)
        .map(|index| HealthHour {
            start: timestamp(first_start + i64::from(index) * HOUR),
            battery_mv: None,
            noise_floor: None,
            received: 0,
            sent: 0,
            receive_errors: 0,
            stored: 0,
        })
        .collect();
    // Receptions stored in the window, to count between reports. Packets an
    // observer sends itself (direction `tx`) aren't in its received count.
    let stored_at: Vec<Micros> = conn
        .prepare_cached(
            "SELECT rx_at FROM observations
             WHERE observer_id = ?1 AND direction = 'rx' AND rx_at > ?2 AND rx_at <= ?3
             ORDER BY rx_at",
        )?
        .query_map(params![observer_id, since, now], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let stored_between = |after: Micros, until: Micros| {
        let count = |at: Micros| stored_at.partition_point(|&stored| stored <= at);
        (count(until) - count(after)) as i64
    };
    let mut hourly_levels: Vec<(Vec<i64>, Vec<i64>)> =
        vec![(Vec::new(), Vec::new()); history.len()];
    for report in &reports {
        if let Some(levels) = bucket(report.at).and_then(|i| hourly_levels.get_mut(i)) {
            levels.0.extend(report.battery());
            levels.1.extend(report.noise());
        }
    }
    for (hour, (batteries, noises)) in history.iter_mut().zip(&hourly_levels) {
        hour.battery_mv = average(batteries);
        hour.noise_floor = average(noises);
    }

    // Counter differences across each continuous pair of reports.
    let mut reboots = 0;
    let mut totals = Totals::default();
    for pair in reports.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        match (a.uptime_secs, b.uptime_secs) {
            (Some(before), Some(after)) if after >= before => {}
            (Some(_), Some(_)) => {
                reboots += 1;
                continue;
            }
            _ => continue,
        }
        let span = (b.at - a.at) as f64 / SECOND as f64;
        let delta = |x: Option<i64>, y: Option<i64>| Some(y? - x?).filter(|d| *d >= 0);
        let hour = bucket(b.at).and_then(|i| history.get_mut(i));
        let (received, sent, errors) =
            (delta(a.received, b.received), delta(a.sent, b.sent), delta(a.errors, b.errors));
        let stored = received.map(|_| stored_between(a.at, b.at));
        if let Some(hour) = hour {
            hour.received += received.unwrap_or(0);
            hour.sent += sent.unwrap_or(0);
            hour.receive_errors += errors.unwrap_or(0);
            hour.stored += stored.unwrap_or(0);
        }
        if let Some(received) = received {
            totals.received += received;
            totals.received_span += span;
            totals.stored += stored.unwrap_or(0);
            if let Some(errors) = errors {
                totals.errors += errors;
                totals.errors_counted += received;
            }
        }
        if let Some(sent) = sent {
            totals.sent += sent;
            totals.sent_span += span;
        }
        if let Some(tx) = delta(a.tx_air_secs, b.tx_air_secs) {
            totals.tx_air += tx;
            totals.tx_span += span;
        }
        if let Some(rx) = delta(a.rx_air_secs, b.rx_air_secs) {
            totals.rx_air += rx;
            totals.rx_span += span;
        }
    }
    // A restart inside the window before its first report counts too.
    if let Some(first) = reports.first()
        && first.uptime_secs.is_some_and(|up| first.at - up * SECOND > since)
    {
        reboots += 1;
    }

    let mut noises: Vec<i64> = reports.iter().filter_map(Report::noise).collect();
    noises.sort_unstable();
    let batteries: Vec<i64> = reports.iter().filter_map(Report::battery).collect();
    let age = now - latest.at;
    let state = match age {
        age if age <= STALE_AFTER => ObserverState::Online,
        age if age <= OFFLINE_AFTER => ObserverState::Stale,
        _ => ObserverState::Offline,
    };
    let per_hour = |count: i64, span: f64| (span > 0.0).then(|| count as f64 * 3600.0 / span);
    let share = |part: f64, whole: f64| (whole > 0.0).then(|| part / whole);
    let mut health = ObserverHealth {
        pubkey: hex::encode(pubkey),
        name,
        model: latest.model.clone(),
        firmware: latest.firmware.clone(),
        radio: latest.radio.clone(),
        state,
        last_report_at: timestamp(latest.at),
        uptime_secs: latest.uptime_secs,
        reboots,
        battery_mv: reports.last().and_then(Report::battery),
        battery_min_mv: batteries.iter().copied().min(),
        battery_max_mv: batteries.iter().copied().max(),
        noise_floor: reports.last().and_then(Report::noise),
        noise_floor_min: noises.first().copied(),
        noise_floor_median: noises.get(noises.len() / 2).copied(),
        noise_floor_max: noises.last().copied(),
        received_per_hour: per_hour(totals.received, totals.received_span),
        sent_per_hour: per_hour(totals.sent, totals.sent_span),
        receive_error_share: share(
            totals.errors as f64,
            (totals.errors + totals.errors_counted) as f64,
        ),
        tx_air_share: share(totals.tx_air as f64, totals.tx_span),
        rx_air_share: share(totals.rx_air as f64, totals.rx_span),
        counted: totals.received,
        stored: totals.stored,
        delivered_share: share(totals.stored as f64, totals.received as f64),
        warnings: Vec::new(),
        history,
    };
    health.warnings = warnings(&health, age, hours);
    Ok(health)
}

#[derive(Default)]
struct Totals {
    received: i64,
    received_span: f64,
    stored: i64,
    sent: i64,
    sent_span: f64,
    errors: i64,
    /// Receptions in the pairs where errors were also counted.
    errors_counted: i64,
    tx_air: i64,
    tx_span: f64,
    rx_air: i64,
    rx_span: f64,
}

fn warnings(health: &ObserverHealth, age: Micros, hours: u32) -> Vec<String> {
    let mut warnings = Vec::new();
    if health.state != ObserverState::Online {
        warnings.push(format!("no report for {}", duration(age / SECOND)));
    }
    if health.reboots > 0 && health.state == ObserverState::Online {
        let up =
            health.uptime_secs.map(|secs| format!("; up {}", duration(secs))).unwrap_or_default();
        let times =
            if health.reboots == 1 { "once".into() } else { format!("{} times", health.reboots) };
        warnings.push(format!("restarted {times} in the last {hours} h{up}"));
    }
    if let Some(mv) = health.battery_mv.filter(|&mv| mv < LOW_BATTERY_MV) {
        warnings.push(format!("battery low: {:.2} V", mv as f64 / 1000.0));
    }
    if let (Some(now), Some(usual)) = (health.noise_floor, health.noise_floor_median)
        && now - usual >= NOISE_JUMP_DB
    {
        warnings.push(format!(
            "noise floor {now} dBm is {} dB above its usual {usual} dBm",
            now - usual
        ));
    }
    if let Some(delivered) = health.delivered_share.filter(|_| health.counted >= MIN_COUNTED)
        && delivered < DELIVERY_WARNING_BELOW
    {
        warnings.push(format!(
            "only {:.1}% of the {} packets it counted reached ferromesh",
            delivered * 100.0,
            health.counted
        ));
    }
    warnings
}

/// `3 d 4 h`, `2 h 13 min`, `45 min`, `20 s`.
fn duration(secs: i64) -> String {
    let (days, hours, minutes) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{secs} s"),
        (0, 0, m) => format!("{m} min"),
        (0, h, m) => format!("{h} h {m} min"),
        (d, h, _) => format!("{d} d {h} h"),
    }
}

fn average(values: &[i64]) -> Option<i64> {
    (!values.is_empty()).then(|| values.iter().sum::<i64>() / values.len() as i64)
}

fn timestamp(micros: Micros) -> Timestamp {
    Timestamp::from_microsecond(micros).unwrap_or(Timestamp::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::duration;

    #[test]
    fn durations() {
        assert_eq!(duration(20), "20 s");
        assert_eq!(duration(45 * 60), "45 min");
        assert_eq!(duration(2 * 3600 + 13 * 60), "2 h 13 min");
        assert_eq!(duration(6 * 86_400 + 22 * 3600 + 5), "6 d 22 h");
    }
}
