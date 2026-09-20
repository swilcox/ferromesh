//! Sending through the API, with the real writer thread and a stand-in for
//! the companion radio's thread, which records each send and answers the way
//! the real one does.

use std::net::SocketAddr;

use ed25519_dalek::{Signer, SigningKey};
use ferromesh_model::{RadioContact, SendStatus, SentMessageInfo};
use ferromesh_store::{ChannelKind, Store};
use ferromeshd::api::{self, AppState};
use ferromeshd::companion::session::{Identity, Received};
use ferromeshd::companion::{Accepted, Advertised, Request, SendError, record};
use ferromeshd::rawlog::{RawLogWriter, RawRecord};
use ferromeshd::writer::{self, Job};
use jiff::Timestamp;
use meshcore_proto::companion::{Contact, DeviceInfo, SelfInfo, Sent, code};
use meshcore_proto::{ChannelKey, GroupText, Packet};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

const TOKEN: &str = "open-sesame-open-sesame";
const ACK: u32 = 0xC0FFEE;

struct Server {
    addr: SocketAddr,
    jobs: mpsc::Sender<Job>,
    _stop: watch::Sender<bool>,
    _dir: TempDir,
}

fn identity() -> Identity {
    Identity {
        info: SelfInfo {
            advert_type: 1,
            tx_power_dbm: 10,
            max_tx_power_dbm: 22,
            pubkey: [0xAB; 32],
            lat_e6: 0,
            lon_e6: 0,
            manual_add_contacts: 0,
            freq_khz: 910_525,
            bandwidth_hz: 62_500,
            spreading_factor: 7,
            coding_rate: 5,
            name: "desk".into(),
        },
        device: DeviceInfo {
            protocol_version: 13,
            max_contacts: 350,
            max_channels: 40,
            build_date: String::new(),
            model: "fake".into(),
            version: "v0".into(),
        },
    }
}

/// The frame a channel message from `desk` has on the air.
fn channel_frame(timestamp: u32, text: &str) -> Vec<u8> {
    let plaintext = GroupText {
        sender_timestamp: timestamp,
        txt_type: 0,
        attempt: 0,
        text: format!("desk: {text}").into_bytes(),
    };
    [&[0x15, 0x00][..], &ChannelKey::from_hashtag("#test").encrypt(&plaintext.to_plaintext())]
        .concat()
}

/// Stands in for the companion thread: records each send, then answers.
/// Text starting "refuse" is refused, as a full contact table would be.
fn fake_radio(requests: std::sync::mpsc::Receiver<Request>, jobs: mpsc::Sender<Job>) {
    let identity = identity();
    let mut timestamp = 1_789_000_000;
    let mut contacts: Vec<Contact> = Vec::new();
    for request in requests {
        let request = match request {
            Request::Contacts { reply } => {
                let _ = reply.send(Ok(contacts.clone()));
                continue;
            }
            Request::Pin { mut contact, pinned, reply } => {
                contacts.retain(|held| held.pubkey != contact.pubkey);
                contact.flags = if pinned { Contact::FAVOURITE } else { 0 };
                contacts.push(contact.clone());
                let _ = reply.send(Ok(contact));
                continue;
            }
            other => other,
        };
        timestamp += 1;
        // Like the real thread, stamp the send with the time it was sent.
        let now = Timestamp::now();
        let (record, reply, outcome) = match request {
            Request::Channel { name, text, reply, .. } => {
                let hash = Packet::parse(&channel_frame(timestamp, &text)).unwrap().hash().0;
                let record = record::sent_channel(
                    &identity,
                    "companion:fake",
                    now,
                    &name,
                    &text,
                    timestamp,
                    &hash,
                    None,
                );
                (record, reply, Ok(()))
            }
            Request::Direct { contact, text, reply } => {
                if text.starts_with("refuse") {
                    let error = "the radio refused adding the contact: table full";
                    let record = record::sent_direct(
                        &identity,
                        "companion:fake",
                        now,
                        &contact,
                        &text,
                        timestamp,
                        Err(error),
                    );
                    (record, reply, Err(error.to_owned()))
                } else {
                    let sent = Sent { flood: true, expected_ack: ACK, timeout_ms: 3000 };
                    let record = record::sent_direct(
                        &identity,
                        "companion:fake",
                        now,
                        &contact,
                        &text,
                        timestamp,
                        Ok(&sent),
                    );
                    (record, reply, Ok(()))
                }
            }
            Request::Advert { flood, reply } => {
                let record = record::advert(&identity, "companion:fake", now, flood, None);
                jobs.blocking_send(Job::Record(record)).unwrap();
                let _ = reply.send(Ok(Advertised {
                    pubkey: identity.info.pubkey,
                    name: identity.info.name.clone(),
                    flood,
                }));
                continue;
            }
            Request::Contacts { .. } | Request::Pin { .. } => unreachable!("answered above"),
        };
        jobs.blocking_send(Job::Record(record)).unwrap();
        let _ = reply.send(
            outcome.map(|()| Accepted { sender_timestamp: timestamp }).map_err(SendError::Failed),
        );
    }
}

impl Server {
    async fn start(with_radio: bool) -> Self {
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
        let mut state = AppState::new(db, events.clone(), stopped)
            .with_writer(jobs.clone(), Some(TOKEN.into()));
        if with_radio {
            let (requests, waiting) = std::sync::mpsc::channel();
            let radio_jobs = jobs.clone();
            std::thread::spawn(move || fake_radio(waiting, radio_jobs));
            state = state.with_companion(requests);
        }
        tokio::spawn(api::serve(listener, state));
        std::thread::spawn(move || writer::run(store, raw, queue, events));
        Self { addr, jobs, _stop: stop, _dir: dir }
    }

    async fn store(&self, records: Vec<RawRecord>) {
        for record in records {
            self.jobs.send(Job::Record(record)).await.unwrap();
        }
        let (done, stored) = oneshot::channel();
        self.jobs.send(Job::Sync(done)).await.unwrap();
        stored.await.unwrap();
    }

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

    async fn send(&self, to: &str, text: &str) -> (u16, String) {
        let body = serde_json::json!({ "to": to, "text": text }).to_string();
        self.request("POST", "/api/v1/send", Some(TOKEN), &body).await
    }

    async fn outbox(&self) -> Vec<SentMessageInfo> {
        let (status, body) = self.request("GET", "/api/v1/outbox", None, "").await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    }
}

/// An MQTT reception by Tanyard.
fn tanyard(second: i64, frame: &[u8]) -> RawRecord {
    let at = Timestamp::from_second(1_789_000_000 + second).unwrap();
    let payload = serde_json::json!({
        "timestamp": at.to_string(), "origin": "Tanyard", "raw": hex::encode_upper(frame), "SNR": "9.5",
    });
    RawRecord {
        received_at: at,
        source: "test".into(),
        topic: format!("meshcore/BNA/{}/packets", hex::encode_upper([1; 32])),
        payload: payload.to_string(),
    }
}

fn advert(name: &str) -> Vec<u8> {
    let key = SigningKey::from_bytes(&[9; 32]);
    let pubkey = key.verifying_key().to_bytes();
    let timestamp = 1_789_000_000u32;
    let app_data = [&[0x81][..], name.as_bytes()].concat();
    let signed = [&pubkey[..], &timestamp.to_le_bytes(), &app_data].concat();
    [&[0x11, 0x00][..], &pubkey, &timestamp.to_le_bytes(), &key.sign(&signed).to_bytes(), &app_data]
        .concat()
}

#[tokio::test(flavor = "multi_thread")]
async fn channel_messages_are_sent_and_heard() {
    let server = Server::start(true).await;
    let body = serde_json::json!({ "to": "#test", "text": "hello" }).to_string();
    assert_eq!(server.request("POST", "/api/v1/send", None, &body).await.0, 401);
    let (status, body) = server.send("#nowhere", "hello").await;
    assert_eq!(status, 404, "{body}");

    let (status, body) = server.send("#TEST", "hello").await;
    assert_eq!(status, 201, "{body}");
    let sent: SentMessageInfo = serde_json::from_str(&body).unwrap();
    assert_eq!((sent.to.as_str(), sent.from.as_str(), sent.direct), ("#test", "desk", false));
    assert_eq!((sent.body.as_str(), sent.status, sent.heard), ("hello", SendStatus::Sent, 0));

    // Tanyard hears the packet the radio sent.
    let frame = channel_frame(sent.sender_timestamp.as_second() as u32, "hello");
    server.store(vec![tanyard(5, &frame)]).await;
    let outbox = server.outbox().await;
    assert_eq!((outbox[0].status, outbox[0].heard), (SendStatus::Heard, 1));
    assert_eq!(outbox[0].heard_by, ["Tanyard"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_messages_are_sent_and_acknowledged() {
    let server = Server::start(true).await;
    server.store(vec![tanyard(1, &advert("Hilltop"))]).await;

    let (status, body) = server.send("hilltop", "hi there").await;
    assert_eq!(status, 201, "{body}");
    let sent: SentMessageInfo = serde_json::from_str(&body).unwrap();
    assert_eq!((sent.to.as_str(), sent.direct, sent.status), ("Hilltop", true, SendStatus::Sent));

    let confirmed =
        [&[code::PUSH_SEND_CONFIRMED][..], &ACK.to_le_bytes(), &1500u32.to_le_bytes()].concat();
    let at = Timestamp::from_second(sent.sent_at.as_second() + 2).unwrap();
    let ack = record::received(&identity(), "companion:fake", &Received { at, frame: confirmed })
        .unwrap();
    server.store(vec![ack]).await;
    let outbox = server.outbox().await;
    assert_eq!((outbox[0].status, outbox[0].round_trip_ms), (SendStatus::Delivered, Some(1500)));

    let (status, body) = server.send("Hilltop", "refuse this").await;
    assert_eq!((status, body.contains("table full")), (502, true), "{body}");
    let outbox = server.outbox().await;
    assert_eq!((outbox[0].status, outbox[0].error.is_some()), (SendStatus::Failed, true));
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_radio_sending_is_unavailable() {
    let server = Server::start(false).await;
    let (status, body) = server.send("#test", "hello").await;
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("[companion]"), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn contacts_are_listed_and_pinned() {
    let server = Server::start(true).await;
    server.store(vec![tanyard(1, &advert("Hilltop"))]).await;
    let pin =
        |to: &str, pinned: bool| serde_json::json!({ "to": to, "pinned": pinned }).to_string();

    assert_eq!(
        server.request("POST", "/api/v1/contacts", None, &pin("Hilltop", true)).await.0,
        401
    );
    let (status, body) =
        server.request("POST", "/api/v1/contacts", Some(TOKEN), &pin("#test", true)).await;
    assert_eq!(status, 400, "{body}");
    let (status, body) =
        server.request("POST", "/api/v1/contacts", Some(TOKEN), &pin("hilltop", true)).await;
    assert_eq!(status, 200, "{body}");
    let pinned: RadioContact = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (pinned.name.as_str(), pinned.kind.as_str(), pinned.favourite),
        ("Hilltop", "chat", true)
    );

    let (status, body) = server.request("GET", "/api/v1/contacts", None, "").await;
    assert_eq!(status, 200, "{body}");
    let listed: Vec<RadioContact> = serde_json::from_str(&body).unwrap();
    assert_eq!(listed, [pinned]);
}
