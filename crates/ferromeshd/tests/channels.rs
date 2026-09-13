//! Channel management through the API, with the real writer thread: finding
//! unknown channels, guessing names, and adding one, which needs the token and
//! decrypts traffic stored before it.

use std::net::SocketAddr;
use std::time::Duration;

use ferromesh_model::{ChannelAdded, ChannelInfo, Event, Frame, GuessReport, UnknownChannel};
use ferromesh_store::{ChannelKind, Store};
use ferromeshd::api::{self, AppState};
use ferromeshd::rawlog::{RawLogWriter, RawRecord};
use ferromeshd::writer::{self, Job};
use futures_util::StreamExt;
use jiff::Timestamp;
use meshcore_proto::{ChannelKey, GroupText};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const TOKEN: &str = "open-sesame-open-sesame";
const HIDDEN: &str = "#hidden-valley";

struct Server {
    addr: SocketAddr,
    jobs: mpsc::Sender<Job>,
    _stop: watch::Sender<bool>,
    _dir: TempDir,
}

impl Server {
    async fn start(token: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("ferromesh.db");
        let mut store = Store::open(&db).unwrap();
        store
            .add_channel("#test", &ChannelKey::from_hashtag("#test"), ChannelKind::Hashtag, 0)
            .unwrap();
        let raw = RawLogWriter::open(dir.path().join("raw")).unwrap();

        let (events, _) = broadcast::channel(64);
        let (stop, stopped) = watch::channel(false);
        let (jobs, queue) = mpsc::channel(64);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = AppState::new(db, events.clone(), stopped)
            .with_writer(jobs.clone(), token.map(Into::into));
        tokio::spawn(api::serve(listener, state));
        std::thread::spawn(move || writer::run(store, raw, queue, events));
        Self { addr, jobs, _stop: stop, _dir: dir }
    }

    /// Hands records to the writer and waits until they're stored.
    async fn store(&self, records: Vec<RawRecord>) {
        for record in records {
            self.jobs.send(Job::Record(record)).await.unwrap();
        }
        let (done, stored) = oneshot::channel();
        self.jobs.send(Job::Sync(done)).await.unwrap();
        stored.await.unwrap();
    }

    /// A bare HTTP/1.1 request: (status, body).
    async fn request(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: &str,
    ) -> (u16, String) {
        let mut socket = TcpStream::connect(self.addr).await.unwrap();
        let auth =
            token.map(|token| format!("Authorization: Bearer {token}\r\n")).unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n{auth}\r\n{body}",
            body.len()
        );
        socket.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        (head.split(' ').nth(1).unwrap().parse().unwrap(), body.to_owned())
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        let (status, body) = self.request("GET", path, None, "").await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    }
}

/// An MQTT packet message carrying GRP_TXT `text` on `channel`.
fn record(channel: &str, n: u32, text: &str) -> RawRecord {
    let message = GroupText { sender_timestamp: n, txt_type: 0, attempt: 0, text: text.into() };
    let mut frame = vec![0x15, 0x00];
    frame.extend(ChannelKey::from_hashtag(channel).encrypt(&message.to_plaintext()));
    let at = Timestamp::from_second(1_789_000_000 + i64::from(n)).unwrap();
    let payload = serde_json::json!({
        "timestamp": at.to_string(),
        "origin": "Tanyard",
        "raw": hex::encode_upper(frame),
    });
    RawRecord {
        received_at: at,
        source: "test".into(),
        topic: format!("meshcore/BNA/{}/packets", "AB".repeat(32)),
        payload: payload.to_string(),
    }
}

async fn next_frame(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
) -> Frame {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("a frame within 5s")
            .expect("stream open")
            .expect("valid message");
        if let Message::Text(text) = message {
            return serde_json::from_str(text.as_str()).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn find_guess_and_add_a_channel() {
    let server = Server::start(Some(TOKEN)).await;
    server
        .store(vec![
            record("#test", 1, "Alice: come over to #hidden-valley"),
            record(HIDDEN, 2, "Bob: first"),
            record(HIDDEN, 3, "Carol: second"),
        ])
        .await;

    let channels: Vec<ChannelInfo> = server.get("/api/v1/channels").await;
    let summary: Vec<_> = channels.iter().map(|c| (c.name.as_str(), c.messages)).collect();
    assert_eq!(summary, [("public", 0), ("#test", 1)]);

    let unknown: Vec<UnknownChannel> = server.get("/api/v1/channels/unknown").await;
    assert_eq!(unknown.len(), 1);
    assert_eq!((unknown[0].hash, unknown[0].packets), (ChannelKey::from_hashtag(HIDDEN).hash(), 2));

    let (status, body) =
        server.request("POST", "/api/v1/channels/guess", None, r#"{"builtin": false}"#).await;
    assert_eq!(status, 200, "{body}");
    let report: GuessReport = serde_json::from_str(&body).unwrap();
    let hits: Vec<_> = report.hits.iter().map(|hit| (hit.name.as_str(), hit.messages)).collect();
    assert_eq!(hits, [(HIDDEN, 2)]);

    // Follow the channel before it exists, so the backfill arrives live.
    let url =
        format!("ws://{}/api/v1/stream?kind=messages&filter=chan%3A%23hidden-valley", server.addr);
    let (mut socket, _) = connect_async(url).await.unwrap();
    assert!(matches!(next_frame(&mut socket).await, Frame::CaughtUp { .. }));

    let add = format!(r#"{{"name": "{HIDDEN}"}}"#);
    let (status, _) = server.request("POST", "/api/v1/channels", None, &add).await;
    assert_eq!(status, 401);
    let (status, _) =
        server.request("POST", "/api/v1/channels", Some("wrong-wrong-wrong"), &add).await;
    assert_eq!(status, 401);
    let (status, body) = server
        .request("POST", "/api/v1/channels", Some(TOKEN), r#"{"name": "hidden-valley"}"#)
        .await;
    assert_eq!(status, 400, "{body}");

    let (status, body) = server.request("POST", "/api/v1/channels", Some(TOKEN), &add).await;
    assert_eq!(status, 201, "{body}");
    let added: ChannelAdded = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (added.channel.name.as_str(), added.backfill.decrypted, added.backfill.messages),
        (HIDDEN, 2, 2)
    );

    let mut live = Vec::new();
    while live.len() < 2 {
        match next_frame(&mut socket).await {
            Frame::Event { event: Event::Message(message) } => live.push(message.body),
            other => panic!("expected a message, got {other:?}"),
        }
    }
    assert_eq!(live, ["first", "second"]);

    let (status, body) = server.request("POST", "/api/v1/channels", Some(TOKEN), &add).await;
    assert_eq!(status, 409, "{body}");
    let unknown: Vec<UnknownChannel> = server.get("/api/v1/channels/unknown").await;
    assert!(unknown.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_token_changes_are_refused() {
    let server = Server::start(None).await;
    let (status, body) =
        server.request("POST", "/api/v1/channels", Some(TOKEN), r##"{"name": "#wx"}"##).await;
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("api.token"));
}
