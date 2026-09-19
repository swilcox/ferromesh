//! Observer health: rates from consecutive status reports, restarts, the
//! delivery check against stored receptions, and warnings.

use ferromesh_model::ObserverState;
use ferromesh_store::{ObserverInfo, Reception, StatusReport, Store};

const MINUTE: i64 = 60_000_000;
/// Up for about 11 days when the reports start.
const UP: i64 = 1_000_000;
/// On the hour, so the history's hourly buckets line up with the test.
const T0: i64 = 1_789_002_000 * 1_000_000;

fn tanyard() -> ObserverInfo {
    ObserverInfo { pubkey: [1; 32], name: Some("Tanyard".into()), iata: Some("BNA".into()) }
}

/// The report after `n` five-minute intervals: 10 packets received, 4 sent
/// and 5 errors per interval, 3 s transmitting and 6 s receiving.
fn report(n: i64, uptime_secs: i64, noise_floor: i64) -> StatusReport {
    StatusReport {
        observer: tanyard(),
        at: T0 + n * 5 * MINUTE,
        status: Some("online".into()),
        model: Some("Heltec V4 OLED".into()),
        firmware_version: Some("v1.17.1".into()),
        radio: Some("910.525,62.5,7,5".into()),
        battery_mv: Some(4200),
        uptime_secs: Some(uptime_secs),
        noise_floor: Some(noise_floor),
        tx_air_secs: Some(3 * n),
        rx_air_secs: Some(6 * n),
        packets_sent: Some(4 * n),
        packets_received: Some(10 * n),
        recv_errors: Some(5 * n),
        queue_len: Some(0),
        raw: format!("{{\"n\":{n}}}"),
    }
}

fn reception(n: i64, k: i64) -> Reception {
    // An undecryptable channel packet, unique per (n, k).
    let mut frame = vec![0x15, 0x00, 0x42, 0x00, 0x00];
    frame.extend((n * 100 + k).to_le_bytes());
    frame.extend([0; 8]);
    Reception {
        observer: tanyard(),
        rx_at: T0 + (n - 1) * 5 * MINUTE + (k + 1) * 20_000_000,
        frame,
        snr: Some(5.0),
        rssi: Some(-80),
        score: None,
        direction: None,
    }
}

fn store() -> Store {
    let mut store = Store::open_in_memory().unwrap();
    store
        .write(|batch| {
            // Two hours of reports; 10 receptions stored per interval, bar one.
            for n in 0..=24 {
                batch.record_status(&report(n, UP + n * 300, if n == 24 { -90 } else { -105 }))?;
            }
            for n in 1..=24 {
                for k in 0..10 {
                    if (n, k) != (7, 3) {
                        batch.record_reception(&reception(n, k))?;
                    }
                }
                // A packet it transmitted isn't among those it counted receiving.
                if n == 9 {
                    let mut sent = reception(n, 99);
                    sent.direction = Some("tx".into());
                    batch.record_reception(&sent)?;
                }
            }
            Ok(())
        })
        .unwrap();
    store
}

#[test]
fn rates_delivery_and_warnings() {
    let store = store();
    let now = T0 + 24 * 5 * MINUTE + MINUTE;
    let health = store.observer_health(now, 3).unwrap();
    assert_eq!(health.len(), 1);
    let tanyard = &health[0];
    assert_eq!(
        (tanyard.name.as_deref(), tanyard.state, tanyard.reboots),
        (Some("Tanyard"), ObserverState::Online, 0)
    );
    assert_eq!(tanyard.received_per_hour, Some(120.0));
    assert_eq!(tanyard.sent_per_hour, Some(48.0));
    assert_eq!(tanyard.tx_air_share, Some(0.01));
    assert_eq!(tanyard.rx_air_share, Some(0.02));
    assert!((tanyard.receive_error_share.unwrap() - 1.0 / 3.0).abs() < 1e-9);
    assert_eq!((tanyard.counted, tanyard.stored), (240, 239));
    assert_eq!((tanyard.noise_floor, tanyard.noise_floor_median), (Some(-90), Some(-105)));
    assert_eq!(tanyard.battery_mv, Some(4200));
    assert_eq!(tanyard.warnings, ["noise floor -90 dBm is 15 dB above its usual -105 dBm"]);

    // Three hours from T0: intervals ending at 5-55, 60-115 and 120 minutes.
    let received: Vec<i64> = tanyard.history.iter().map(|hour| hour.received).collect();
    assert_eq!(received, [110, 120, 10]);
    let stored: Vec<i64> = tanyard.history.iter().map(|hour| hour.stored).collect();
    assert_eq!(stored.iter().sum::<i64>(), 239);
}

#[test]
fn missing_packets_are_flagged() {
    // One more interval, with nothing stored from it: 239 of 250.
    let mut store = store();
    store.write(|batch| batch.record_status(&report(25, UP + 25 * 300, -105))).unwrap();
    let now = T0 + 25 * 5 * MINUTE;
    let health = &store.observer_health(now, 3).unwrap()[0];
    assert_eq!((health.counted, health.stored), (250, 239));
    assert!(
        health.warnings.iter().any(|w| w.starts_with("only 95.6% of the 250 packets")),
        "{:?}",
        health.warnings
    );
}

#[test]
fn restarts_and_silence() {
    let mut store = store();
    // It restarts: the next report's uptime starts again.
    store.write(|batch| batch.record_status(&report(25, 60, -105))).unwrap();
    let last = T0 + 25 * 5 * MINUTE;
    let health = &store.observer_health(last + MINUTE, 3).unwrap()[0];
    assert_eq!(health.reboots, 1);
    assert!(
        health.warnings.iter().any(|w| w == "restarted once in the last 3 h; up 1 min"),
        "{:?}",
        health.warnings
    );
    assert_eq!(health.counted, 240, "the pair across the restart isn't counted");

    let health = &store.observer_health(last + 30 * MINUTE, 3).unwrap()[0];
    assert_eq!(health.state, ObserverState::Stale);
    let health = &store.observer_health(last + 3 * 60 * MINUTE, 3).unwrap()[0];
    assert_eq!(health.state, ObserverState::Offline);
    assert_eq!(health.warnings[0], "no report for 3 h 0 min");
    assert_eq!(health.received_per_hour, None, "no reports in the window");
}
