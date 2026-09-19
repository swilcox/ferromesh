//! The nodes and packet-detail endpoints against a real server.

use std::net::SocketAddr;

use ed25519_dalek::{Signer, SigningKey};
use ferromesh_model::{NodeInfo, PacketDetail};
use ferromesh_store::{ChannelKind, Store};
use ferromeshd::api::{self, AppState};
use ferromeshd::pipeline::{self, Tally};
use ferromeshd::rawlog::RawRecord;
use jiff::Timestamp;
use meshcore_proto::{ChannelKey, GroupText, Packet};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, watch};

struct Server {
    addr: SocketAddr,
    _stop: watch::Sender<bool>,
    _dir: TempDir,
}

impl Server {
    async fn start(records: &[RawRecord]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("ferromesh.db");
        let mut store = Store::open(&db).unwrap();
        store
            .add_channel("#test", &ChannelKey::from_hashtag("#test"), ChannelKind::Hashtag, 0)
            .unwrap();
        pipeline::ingest(&mut store, records, &mut Tally::default()).unwrap();

        let (events, _) = broadcast::channel(8);
        let (stop, stopped) = watch::channel(false);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(api::serve(listener, AppState::new(db, events, stopped)));
        Self { addr, _stop: stop, _dir: dir }
    }

    async fn get(&self, path: &str) -> (u16, String) {
        let mut socket = TcpStream::connect(self.addr).await.unwrap();
        let request = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
        socket.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        (head.split(' ').nth(1).unwrap().parse().unwrap(), body.to_owned())
    }
}

/// An MQTT packet message from `observer` (name, key byte) carrying `frame`.
fn record(observer: (&str, u8), second: i64, frame: &[u8]) -> RawRecord {
    let at = Timestamp::from_second(1_789_000_000 + second).unwrap();
    let payload = serde_json::json!({
        "timestamp": at.to_string(),
        "origin": observer.0,
        "raw": hex::encode_upper(frame),
        "SNR": "-2.5",
        "RSSI": "-91",
    });
    RawRecord {
        received_at: at,
        source: "test".into(),
        topic: format!("meshcore/BNA/{}/packets", hex::encode_upper([observer.1; 32])),
        payload: payload.to_string(),
    }
}

fn advert(name: &str) -> Vec<u8> {
    let key = SigningKey::from_bytes(&[9; 32]);
    let pubkey = key.verifying_key().to_bytes();
    let timestamp = 1_789_000_000u32;
    let app_data = [&[0x82][..], name.as_bytes()].concat();
    let signed = [&pubkey[..], &timestamp.to_le_bytes(), &app_data].concat();
    [&[0x11, 0x00][..], &pubkey, &timestamp.to_le_bytes(), &key.sign(&signed).to_bytes(), &app_data]
        .concat()
}

#[tokio::test(flavor = "multi_thread")]
async fn nodes_and_packet_detail() {
    let message =
        GroupText { sender_timestamp: 1, txt_type: 0, attempt: 0, text: b"Bob: hi".to_vec() };
    let payload = ChannelKey::from_hashtag("#test").encrypt(&message.to_plaintext());
    let direct = [&[0x15, 0x00][..], &payload].concat();
    let relayed = [&[0x15, 0x01, 0xAB][..], &payload].concat();
    let server = Server::start(&[
        record(("Tanyard", 1), 1, &advert("Hilltop")),
        record(("Tanyard", 1), 2, &direct),
        record(("Ridge", 2), 3, &relayed),
    ])
    .await;

    let (status, body) = server.get("/api/v1/nodes").await;
    assert_eq!(status, 200, "{body}");
    let nodes: Vec<NodeInfo> = serde_json::from_str(&body).unwrap();
    let summary: Vec<_> = nodes.iter().map(|n| (n.name.as_deref(), n.role.as_deref())).collect();
    assert_eq!(summary, [(Some("Hilltop"), Some("repeater"))]);

    let hash = Packet::parse(&direct).unwrap().hash().to_string();
    let (status, body) = server.get(&format!("/api/v1/packets/{hash}")).await;
    assert_eq!(status, 200, "{body}");
    let detail: PacketDetail = serde_json::from_str(&body).unwrap();
    assert_eq!(detail.packet.text.as_deref(), Some("Bob: hi"));
    let receptions: Vec<_> =
        detail.receptions.iter().map(|r| (r.observer.as_str(), r.frame.clone())).collect();
    assert_eq!(
        receptions,
        [("Tanyard", hex::encode_upper(&direct)), ("Ridge", hex::encode_upper(&relayed))]
    );

    assert_eq!(server.get("/api/v1/packets/not-a-hash").await.0, 400);
    assert_eq!(server.get("/api/v1/packets/0000000000000000").await.0, 404);
    assert_eq!(server.get("/api/v1/nodes?limit=0").await.0, 200);
}
