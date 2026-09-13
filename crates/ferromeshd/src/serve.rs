//! `ferromeshd serve`: subscribe, log raw, store, and serve the API.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use ferromesh_model::Event;
use ferromesh_store::Store;
use jiff::Timestamp;
use rumqttc::{AsyncClient, Event as MqttEvent, MqttOptions, Packet, QoS};
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{info, warn};

use crate::api::{self, AppState};
use crate::config::Config;
use crate::pipeline::{self, Tally};
use crate::rawlog::{RawLogWriter, RawRecord};

/// Messages that arrive together share one transaction and one fsync.
const MAX_BATCH: usize = 512;
/// Live events buffered per subscriber; one that falls further behind is
/// caught up from the database.
const EVENT_BUFFER: usize = 1024;
const REPORT_EVERY: Duration = Duration::from_secs(600);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

pub async fn run(config: Config) -> Result<()> {
    let listener = TcpListener::bind(config.api.listen)
        .await
        .with_context(|| format!("listening on {}", config.api.listen))?;
    let store = pipeline::open_store(&config)?;
    let raw = RawLogWriter::open(config.raw_dir())?;

    let (events, _) = broadcast::channel(EVENT_BUFFER);
    let (stop, stopped) = watch::channel(false);
    let api = tokio::spawn(api::serve(
        listener,
        AppState::new(config.db_path(), events.clone(), stopped),
    ));
    info!(listen = %config.api.listen, "API listening");

    let (records, incoming) = mpsc::channel(4096);
    let writer = std::thread::Builder::new()
        .name("writer".into())
        .spawn(move || write_loop(store, raw, incoming, events))?;

    // Returning drops the sender, which lets the writer drain and stop.
    let subscribed = subscribe(&config, records).await;
    let _ = stop.send(true);
    let served = api.await.map_err(|_| anyhow!("API task panicked"))?.context("API server");
    let written = writer.join().map_err(|_| anyhow!("writer thread panicked"))?;
    written.and(subscribed).and(served)
}

async fn subscribe(config: &Config, records: mpsc::Sender<RawRecord>) -> Result<()> {
    let mqtt = &config.mqtt;
    let source = format!("mqtt://{}:{}", mqtt.host, mqtt.port);
    let mut options = MqttOptions::new(&mqtt.client_id, &mqtt.host, mqtt.port);
    options.set_keep_alive(Duration::from_secs(30));
    if let Some(username) = &mqtt.username {
        options.set_credentials(username, mqtt.password.as_deref().unwrap_or_default());
    }
    let (client, mut events) = AsyncClient::new(options, 16);
    let mut terminate = signal(SignalKind::terminate())?;

    loop {
        let event = tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
            event = events.poll() => event,
        };
        match event {
            Ok(MqttEvent::Incoming(Packet::ConnAck(_))) => {
                info!(broker = %source, "connected");
                // Sessions are clean, so each connect subscribes afresh.
                // meshcoretomqtt publishes at QoS 0; asking for more buys nothing.
                for topic in &mqtt.topics {
                    client
                        .try_subscribe(topic, QoS::AtMostOnce)
                        .with_context(|| format!("subscribing to {topic}"))?;
                }
            }
            Ok(MqttEvent::Incoming(Packet::Publish(publish))) => {
                let topic = publish.topic.clone();
                let payload = String::from_utf8(publish.payload.to_vec()).unwrap_or_else(|e| {
                    warn!(%topic, "payload is not UTF-8; storing it lossily");
                    String::from_utf8_lossy(e.as_bytes()).into_owned()
                });
                let record = RawRecord {
                    received_at: Timestamp::now(),
                    source: source.clone(),
                    topic,
                    payload,
                };
                if records.send(record).await.is_err() {
                    bail!("writer stopped");
                }
            }
            Ok(_) => {}
            Err(e) => {
                warn!(broker = %source, "MQTT connection error: {e}; retrying in {RECONNECT_DELAY:?}");
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }
    info!("shutting down");
    Ok(())
}

fn write_loop(
    mut store: Store,
    mut raw: RawLogWriter,
    mut incoming: mpsc::Receiver<RawRecord>,
    events: broadcast::Sender<Arc<Event>>,
) -> Result<()> {
    store.track_changes();
    let mut tally = Tally::default();
    let mut last_report = Instant::now();
    let mut batch = Vec::with_capacity(MAX_BATCH);
    while let Some(first) = incoming.blocking_recv() {
        batch.push(first);
        while batch.len() < MAX_BATCH {
            let Ok(record) = incoming.try_recv() else { break };
            batch.push(record);
        }
        // Raw log first: if the database write fails, a rebuild still has it.
        raw.append(&batch)?;
        pipeline::ingest(&mut store, &batch, &mut tally)?;
        pipeline::publish(&mut store, &events)?;
        batch.clear();
        if last_report.elapsed() >= REPORT_EVERY {
            info!(%tally, "ingested");
            last_report = Instant::now();
        }
    }
    info!(%tally, "ingested");
    Ok(())
}
