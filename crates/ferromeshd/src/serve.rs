//! `ferromeshd serve`: subscribe to MQTT if a broker is configured, read the
//! companion radio if there is one, feed the writer, and serve the API.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use jiff::Timestamp;
use rumqttc::{AsyncClient, Event as MqttEvent, MqttOptions, Packet, QoS};
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{info, warn};

use crate::api::{self, AppState};
use crate::companion::Companion;
use crate::config::{Config, MqttConfig};
use crate::pipeline;
use crate::rawlog::{RawLogWriter, RawRecord};
use crate::writer::{self, Job};

/// Live events buffered per subscriber; one that falls further behind is
/// caught up from the database.
const EVENT_BUFFER: usize = 1024;
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

pub async fn run(config: Config) -> Result<()> {
    let listener = TcpListener::bind(config.api.listen)
        .await
        .with_context(|| format!("listening on {}", config.api.listen))?;
    let store = pipeline::open_store(&config)?;
    let raw = RawLogWriter::open(config.raw_dir())?;

    let (events, _) = broadcast::channel(EVENT_BUFFER);
    let (stop, stopped) = watch::channel(false);
    let (jobs, queue) = mpsc::channel(4096);
    let companion = config
        .companion
        .clone()
        .map(|companion| Companion::spawn(companion, jobs.clone(), config.db_path()))
        .transpose()?;
    let mut state = AppState::new(config.db_path(), events.clone(), stopped)
        .with_writer(jobs.clone(), config.api.token.clone());
    if let Some(companion) = &companion {
        state = state.with_companion(companion.requests());
    }
    let api = tokio::spawn(api::serve(listener, state));
    let changes = if config.api.token.is_some() { "with token" } else { "disabled" };
    info!(listen = %config.api.listen, changes, "API listening");

    let writer = std::thread::Builder::new()
        .name("writer".into())
        .spawn(move || writer::run(store, raw, queue, events))?;

    let subscribed = match &config.mqtt {
        Some(mqtt) => subscribe(mqtt, jobs).await,
        None => {
            info!("no [mqtt] broker configured; recording from the companion radio alone");
            wait_for_stop(jobs).await
        }
    };
    // Stopping the companion and then the API releases their handles on the
    // writer, which then drains what's queued and stops.
    if let Some(companion) = companion {
        tokio::task::spawn_blocking(move || companion.stop()).await?;
    }
    let _ = stop.send(true);
    let served = api.await.map_err(|_| anyhow!("API task panicked"))?.context("API server");
    let written = writer.join().map_err(|_| anyhow!("writer thread panicked"))?;
    written.and(subscribed).and(served)
}

/// With no broker to subscribe to, waits for Ctrl-C or SIGTERM instead. It
/// takes the writer handle, as [`subscribe`] does, because the writer only
/// finishes once every sender has been dropped.
async fn wait_for_stop(jobs: mpsc::Sender<Job>) -> Result<()> {
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
    drop(jobs);
    info!("shutting down");
    Ok(())
}

async fn subscribe(mqtt: &MqttConfig, jobs: mpsc::Sender<Job>) -> Result<()> {
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
                if jobs.send(Job::Record(record)).await.is_err() {
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
