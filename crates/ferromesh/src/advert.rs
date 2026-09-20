//! `ferromesh advert`: ask the server's companion radio to advertise
//! itself, then watch the advert come back through the observers.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::Result;
use ferromesh_model::{AdvertRequest, AdvertSent, Event, HistoryQuery, ObservationEvent};

use crate::channels::local;
use crate::server::Server;

const CHECK_EVERY: Duration = Duration::from_secs(1);
/// Enough of the radio's key to pick its adverts out of the traffic.
const KEY_PREFIX: usize = 12;

pub async fn advert(
    server: &Server,
    flood: bool,
    token: Option<&str>,
    follow: Duration,
) -> Result<()> {
    let sent: AdvertSent = server.post("/api/v1/advert", &AdvertRequest { flood }, token).await?;
    let reach = if sent.flood { "across the mesh" } else { "to its neighbours" };
    println!("{} advertised itself {reach} at {}", sent.name, local(sent.sent_at));

    let prefix: String = sent.pubkey.chars().take(KEY_PREFIX).collect();
    let path = server.path(
        "/api/v1/observations",
        &HistoryQuery {
            filter: Some(format!("type:advert node:{prefix}")),
            since: Some(sent.sent_at),
            limit: Some(100),
            ..HistoryQuery::default()
        },
    )?;
    let deadline = tokio::time::Instant::now() + follow;
    let mut heard = BTreeSet::new();
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(CHECK_EVERY).await;
        for event in server.get::<Vec<Event>>(&path).await? {
            let Event::Observation(observation) = event else { continue };
            if heard.insert((observation.observer.clone(), observation.hops.len())) {
                println!("  {}", reception(&observation));
            }
        }
    }
    if heard.is_empty() && !follow.is_zero() {
        println!("  no observer heard it");
    }
    Ok(())
}

fn reception(observation: &ObservationEvent) -> String {
    let where_from = match observation.hops.len() {
        0 => "directly".to_owned(),
        1 => "at 1 hop".to_owned(),
        hops => format!("at {hops} hops"),
    };
    match observation.snr {
        Some(snr) => format!("heard by {} {where_from}, SNR {snr:.1}", observation.observer),
        None => format!("heard by {} {where_from}", observation.observer),
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;

    use super::*;

    fn observation(observer: &str, hops: usize, snr: Option<f64>) -> ObservationEvent {
        ObservationEvent {
            id: 1,
            packet_id: 1,
            hash: "aa".into(),
            payload_type: "advert".into(),
            rx_at: Timestamp::UNIX_EPOCH,
            observer: observer.into(),
            route: "flood".into(),
            hops: vec!["ab".into(); hops],
            snr,
            rssi: None,
            channel: None,
            advert_pubkey: None,
            advert_name: None,
            text: None,
        }
    }

    #[test]
    fn reception_lines() {
        assert_eq!(
            reception(&observation("Tanyard", 0, Some(-4.25))),
            "heard by Tanyard directly, SNR -4.2"
        );
        assert_eq!(reception(&observation("Tanyard", 1, None)), "heard by Tanyard at 1 hop");
        assert_eq!(
            reception(&observation("BNA Bot", 3, Some(2.0))),
            "heard by BNA Bot at 3 hops, SNR 2.0"
        );
    }
}
