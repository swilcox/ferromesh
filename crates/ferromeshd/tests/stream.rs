//! The API's contract, against a real server on a random port: history then
//! live, no gaps or repeats across a reconnect, filters, catch-up for a
//! subscriber that falls behind, and useful errors.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ferromesh_model::{Event, Frame, Kind, StreamQuery};
use ferromesh_store::{ChannelKind, Store};
use ferromeshd::api::{self, AppState};
use ferromeshd::pipeline::{self, Tally};
use ferromeshd::rawlog::RawRecord;
use futures_util::StreamExt;
use jiff::Timestamp;
use meshcore_proto::{ChannelKey, GroupText};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, watch};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Server {
    addr: SocketAddr,
    store: Store,
    events: broadcast::Sender<Arc<Event>>,
    said: u32,
    _stop: watch::Sender<bool>,
    _dir: TempDir,
}

impl Server {
    /// `buffer` is the live-event buffer per subscriber.
    async fn start(buffer: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("ferromesh.db");
        let mut store = Store::open(&db).unwrap();
        store
            .add_channel("#test", &ChannelKey::from_hashtag("#test"), ChannelKind::Hashtag, 0)
            .unwrap();
        store.track_changes();

        let (events, _) = broadcast::channel(buffer);
        let (stop, stopped) = watch::channel(false);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(api::serve(listener, AppState::new(db, events.clone(), stopped)));
        Self { addr, store, events, said: 0, _stop: stop, _dir: dir }
    }

    /// Stores `count` new #test messages the way the writer thread does.
    fn say(&mut self, count: u32) {
        let records: Vec<RawRecord> = (0..count)
            .map(|_| {
                self.said += 1;
                message_record(self.said)
            })
            .collect();
        pipeline::ingest(&mut self.store, &records, &mut Tally::default()).unwrap();
        pipeline::publish(&mut self.store, &self.events).unwrap();
    }

    fn stream_url(&self, query: &StreamQuery) -> String {
        format!("ws://{}/api/v1/stream?{}", self.addr, serde_urlencoded::to_string(query).unwrap())
    }

    async fn connect(&self, query: &StreamQuery) -> Socket {
        connect_async(self.stream_url(query)).await.unwrap().0
    }

    /// A bare HTTP/1.1 GET: (status, body).
    async fn get(&self, path_and_query: &str) -> (u16, String) {
        let mut socket = TcpStream::connect(self.addr).await.unwrap();
        let request =
            format!("GET {path_and_query} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
        socket.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        (status, body.to_owned())
    }
}

/// An MQTT packet message carrying GRP_TXT "Bot: message {n}" on #test.
fn message_record(n: u32) -> RawRecord {
    let text = GroupText {
        sender_timestamp: n,
        txt_type: 0,
        attempt: 0,
        text: format!("Bot: message {n}").into_bytes(),
    };
    let mut frame = vec![0x15, 0x00];
    frame.extend(ChannelKey::from_hashtag("#test").encrypt(&text.to_plaintext()));
    let at = Timestamp::from_second(1_789_000_000 + i64::from(n)).unwrap();
    let payload = serde_json::json!({
        "timestamp": at.to_string(),
        "origin": "Tanyard",
        "raw": hex::encode_upper(frame),
        "SNR": "-1.5",
        "RSSI": "-90",
    });
    RawRecord {
        received_at: at,
        source: "test".into(),
        topic: format!("meshcore/BNA/{}/packets", "AB".repeat(32)),
        payload: payload.to_string(),
    }
}

fn stream(kind: Kind, filter: Option<&str>) -> StreamQuery {
    StreamQuery { kind, filter: filter.map(Into::into), after: None, since: None, last: None }
}

async fn next_frame(socket: &mut Socket) -> Frame {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("a frame within 5s")
            .expect("stream still open")
            .expect("valid message");
        if let Message::Text(text) = message {
            return serde_json::from_str(text.as_str()).unwrap();
        }
    }
}

/// Event (id, label) pairs up to `CaughtUp`, and its `last_id`.
async fn history(socket: &mut Socket) -> (Vec<(i64, String)>, i64) {
    let mut events = Vec::new();
    loop {
        match next_frame(socket).await {
            Frame::Event { event } => events.push(label(&event)),
            Frame::CaughtUp { last_id } => return (events, last_id),
            Frame::Error { message } => panic!("stream error: {message}"),
        }
    }
}

async fn live(socket: &mut Socket, count: usize) -> Vec<(i64, String)> {
    let mut events = Vec::with_capacity(count);
    while events.len() < count {
        match next_frame(socket).await {
            Frame::Event { event } => events.push(label(&event)),
            other => panic!("expected an event, got {other:?}"),
        }
    }
    events
}

fn label(event: &Event) -> (i64, String) {
    let text = match event {
        Event::Message(message) => message.body.clone(),
        Event::Packet(packet) => packet.payload_type.clone(),
        Event::Observation(observation) => observation.text.clone().unwrap_or_default(),
    };
    (event.id(), text)
}

fn bodies(events: &[(i64, String)]) -> Vec<&str> {
    events.iter().map(|(_, text)| text.as_str()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn history_then_live_then_resume_without_gaps() {
    let mut server = Server::start(64).await;
    server.say(5);

    let query = StreamQuery { last: Some(3), ..stream(Kind::Messages, Some("chan:#test")) };
    let mut socket = server.connect(&query).await;
    let (replayed, caught_up_at) = history(&mut socket).await;
    assert_eq!(bodies(&replayed), ["message 3", "message 4", "message 5"]);
    assert_eq!(caught_up_at, 5);

    server.say(2);
    let followed = live(&mut socket, 2).await;
    assert_eq!(bodies(&followed), ["message 6", "message 7"]);

    // Drop the connection, miss some traffic, then resume from the last id seen.
    let last_seen = followed.last().unwrap().0;
    drop(socket);
    server.say(3);
    let resumed = StreamQuery { after: Some(last_seen), last: None, ..query.clone() };
    let mut socket = server.connect(&resumed).await;
    let (missed, _) = history(&mut socket).await;
    assert_eq!(bodies(&missed), ["message 8", "message 9", "message 10"]);

    server.say(1);
    assert_eq!(bodies(&live(&mut socket, 1).await), ["message 11"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn live_only_stream_applies_its_filter() {
    let mut server = Server::start(64).await;
    server.say(2);

    let mut socket = server.connect(&stream(Kind::Messages, Some(r#"text:"message 4""#))).await;
    let (replayed, caught_up_at) = history(&mut socket).await;
    assert!(replayed.is_empty());
    assert_eq!(caught_up_at, 2);

    server.say(3);
    assert_eq!(bodies(&live(&mut socket, 1).await), ["message 4"]);
    let nothing_else =
        tokio::time::timeout(Duration::from_millis(300), next_frame(&mut socket)).await;
    assert!(nothing_else.is_err(), "unexpected frame: {nothing_else:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn lagging_subscriber_is_caught_up_from_the_database() {
    // A two-event buffer against one burst of 150 events (50 each of
    // messages, packets and observations) forces the subscriber to lag.
    let mut server = Server::start(2).await;
    let mut socket = server.connect(&stream(Kind::Observations, None)).await;
    history(&mut socket).await;

    server.say(50);
    let ids: Vec<i64> = live(&mut socket, 50).await.into_iter().map(|(id, _)| id).collect();
    assert_eq!(ids, (1..=50).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread")]
async fn since_replays_by_time() {
    let mut server = Server::start(64).await;
    server.say(4);
    let since = Timestamp::from_second(1_789_000_003).unwrap();
    let query = StreamQuery { since: Some(since), ..stream(Kind::Messages, None) };
    let mut socket = server.connect(&query).await;
    let (replayed, _) = history(&mut socket).await;
    assert_eq!(bodies(&replayed), ["message 3", "message 4"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn history_endpoint_and_errors() {
    let mut server = Server::start(64).await;
    server.say(3);

    let (status, body) = server.get("/api/v1/messages?limit=2").await;
    assert_eq!(status, 200);
    let events: Vec<Event> = serde_json::from_str(&body).unwrap();
    assert_eq!(bodies(&events.iter().map(label).collect::<Vec<_>>()), ["message 3", "message 2"]);

    let (status, body) = server.get("/api/v1/observations?filter=snr%3E-2").await;
    assert_eq!(status, 200);
    assert_eq!(serde_json::from_str::<Vec<Event>>(&body).unwrap().len(), 3);

    let (status, body) = server.get("/api/v1/messages?filter=snr%3E1").await;
    assert_eq!(status, 400);
    assert!(body.contains("doesn't apply to messages"), "{body}");

    let (status, _) = server.get("/api/v1/nodes").await;
    assert_eq!(status, 404);

    let (status, body) = server.get("/api/v1/health").await;
    assert_eq!(status, 200);
    assert!(body.contains("version"));

    let bad = stream(Kind::Messages, Some("colour:red"));
    match connect_async(server.stream_url(&bad)).await {
        Err(tungstenite::Error::Http(response)) => assert_eq!(response.status(), 400),
        other => panic!("expected HTTP 400, got {other:?}"),
    }
}
