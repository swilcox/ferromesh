# ferromesh — MeshCore MQTT ingest, store, watch, alert

Status (2026-09-13):
- **Phase 0 complete:** decoder crate plus golden test.
- **Phase 1 deployed on truffles:** only the 24h no-loss check is pending.
- **Phase 2 complete:** API, stream, and `ferromesh tail`/`query`, verified live across a forced server restart.
- **Next:** phase 3.

Prior art is in `../mqtt_observer`.

## 0. Decisions so far

| Topic | Decision |
|---|---|
| Name | **ferromesh**. Server binary `ferromeshd`, client binary `ferromesh`. crates.io and GitHub names were free as of 2026-09-12. |
| Host | **truffles**: amd64 Linux with Docker and plenty of resources. The server ships as a container (docker compose). |
| Data scope | **Your own repeater's local feed first.** It works without internet. NashMesh-wide ingest is an optional later add-on; the design stays multi-source so it slots in cleanly. |
| Database | **SQLite** (WAL) plus a verbatim raw MQTT log. See §3. |
| Channels | **Dynamic.** Stored in the DB and seeded from `channels.json`. Adding one decodes past traffic too. Discovery helps find more. |
| Your own traffic | **Via a companion radio attached to truffles** (§4.6), not by extracting private keys. |
| Sender identity | By display name (the only thing the protocol carries), with a best-effort link to node pubkeys from adverts, shown as a hint. |
| Alerts | v1 = active filters highlighted in live traffic (TUI). ntfy and other sinks come later. |
| Order | **Data backend → TUI → companion/send → web.** |
| Web frontend | **Deferred** (you're deciding). The API stays frontend-agnostic. |

## 1. What we have today (measured, not assumed)

Source: `../mqtt_observer/meshcore_packets.jsonl`, 9,193 lines, 02:54–21:24 UTC on 2026-09-12, plus a live `mosquitto_sub` sample.

| Fact | Value |
|---|---|
| Broker | `truffles.local:1883`, Mosquitto 2.1.2, anonymous read OK (shared with teslamate) |
| Topics | `meshcore/BNA/<observer-pubkey>/packets` and `.../status` (meshcoretomqtt format) |
| Observer | one: "Tanyard", Heltec V4, fw `v1.17.1.3-observer`, 910.525 MHz / 62.5 kHz / SF7 / CR5 |
| Rate | ≈311 receptions/hour ≈ 7.5k/day ≈ 2.7M/year. Corrected 2026-09-13: the Python capture wrote 3,254 lines twice between 03:01 and 15:30 UTC, and the first count included them. |
| Dedupe | 5,749 receptions → 4,359 unique packets (1.32 copies avg, max 6). The 9,193-line phase 0 fixture still contains the doubled lines, which is harmless for decode tests. |
| Size | avg 85 bytes raw per reception |
| Mix | GRP_TXT 2292, ANON_REQ 1815, ADVERT 1412, REQ 1170, RESPONSE 795, PATH 718, TXT_MSG 549, ACK 43, CONTROL 42, GRP_DATA 16, TRACE 1 |
| Path hash size | varies per packet: 1-byte 2214, 2-byte 4107, 3-byte 2553 — parser must honor `path_len` bits 6–7 |
| Channel decode | 13 configured channels + public → 875 of 2292 GRP_TXT decrypt (426 unique msgs); **62% undecrypted** |
| Unknown channels | 89 distinct channel-hash bytes; `0x81` alone = 606 packets (likely a private-key channel) |
| Back-decode test | guessing ~90 common hashtag names unlocked 6 more channels (`#chattanooga`, `#tn-east`, `#georgia`, `#bna-test`, `#bots`, `#testing`) from stored raw |
| Sender names | 145 distinct names in decrypted channel text |

### Protocol facts that shape the design (plain-language)
- **Node roles.** A *repeater* only relays packets, broadcasts adverts (its "I exist, here's my name/location" beacon) and answers admin logins. **It cannot send chat messages.** A *room server* is a tiny BBS that stores posts for users who log in; it doesn't post to channels either. A *companion* is the radio a person chats through (normally paired to a phone over Bluetooth); it owns an identity (keypair).
- **Packet hash** (firmware `calculatePacketHash`): `SHA256(payload_type [+ path_len if TRACE] + payload)`, path excluded. The MQTT `hash` field matches this (same hash seen at len 75/77/79 as the path grew). It is the dedupe key across relays and observers. **TRACE quirk:** `path_len` is one byte on the wire, but the firmware stores it as `uint16_t` and hashes `sizeof(path_len)`, so TRACE hashes include `[path_len, 0x00]`. This was confirmed against the observer's hash in phase 0.
- **Channel msgs (GRP_TXT)**: `chan_hash(1) | HMAC-SHA256(secret)[:2] | AES-128-ECB(secret)`. Plaintext = `sender_ts(4) | type/attempt(1) | "Sender Name: text"`. Hashtag channel secret = `SHA256("#name")[:16]`, so knowing the name is enough. The 1-byte hash collides; the HMAC confirms the channel.
- **Who sent a channel message?** Only the name the sender typed into their radio, carried inside the encrypted text. It is not signed and not tied to a key, so anyone can pick any name. Filtering "from BNABot" works in practice but is trust-by-name. We link names to nodes when an advert carries the same name.
- **DMs / REQ / RESPONSE / PATH**: `dst_hash(1) | src_hash(1) | MAC(2) | ciphertext`, keyed by the ECDH secret between the two endpoints. Without an endpoint's private key they stay opaque. A companion radio decrypts its own DMs on-device (§4.6).
- **Adverts** are cleartext and Ed25519-signed, and form the node directory. We verify the signature.
- **Path** = the list of repeaters a packet passed through, each written as a 1–3-byte prefix of its pubkey. We resolve prefixes against known repeaters; 1-byte ones are ambiguous.

## 2. Architecture

```
 truffles (docker, network_mode: host)
 ┌──────────────────────────────────────────────────────────┐
 │ mosquitto ◄── Tanyard repeater (WiFi/MQTT)               │
 │     │                                                    │
 │     ▼ MQTT                   USB serial / TCP:5000       │
 │ ferromeshd ◄──────────────── companion radio (phase 5)   │
 │  ingest → decode → store → fan-out → watches             │
 │     │                                                    │
 │  SQLite ferromesh.db + raw/YYYY-MM-DD.jsonl.zst (volume) │
 └─────┬────────────────────────────────────────────────────┘
       │ WebSocket (stream: backfill then live) + REST (query, channels, send)
       ├── ferromesh tui   (mac arm64 / linux amd64 static binary)
       ├── ferromesh tail  (plain ANSI lines, pipeable)
       └── web UI          (later; stack TBD)
```

Principles:
1. **Raw first, decode second.** Every MQTT message is kept verbatim. Decoded tables can always be rebuilt (`ferromesh rebuild`).
2. **Clients never touch the DB.** Everything goes through the server API, so clients work from any LAN machine.
3. **One filter model, two evaluators.** The same filter compiles to SQL (history) and an in-memory predicate (live). This gives backfill-then-live and alerts from one piece of code.
4. **Multi-source from day one.** A source is anything that yields observations: an MQTT broker, and later the companion's RX log. Adding NashMesh is a config entry, not a redesign.

### Cargo workspace

| Crate | Role | Key deps |
|---|---|---|
| `meshcore-proto` | Pure, no I/O. Header/transport/path/payload parse, packet hash, advert parse + Ed25519 verify, GRP_TXT/GRP_DATA decrypt | `aes`, `hmac`, `sha2`, `ed25519-dalek` |
| `ferromesh-model` | Shared types: events, filter AST + parser, API/WS wire protocol | `serde`, `chrono` |
| `ferromesh-store` | Schema, migrations, single writer thread, filter → SQL | `rusqlite` (bundled, FTS5) |
| `ferromeshd` | Sources (MQTT, later companion), raw log, decode pipeline, broadcast hub, axum API, watch engine. Admin subcommands `import`, `rebuild` and `stats` work on the data directory directly | `tokio`, `rumqttc`, `zstd`, `jiff`, `axum`, `tracing`; later `meshcore-rs` |
| `ferromesh` (client) | `tui`, `tail`, `query`, `channels`, `watches`, later `send` | `ratatui`, `crossterm`, `tokio-tungstenite`, `clap` |

External crates evaluated:
- `MeshCore` 0.0.1 (packet parsing, Nov 2025): self-described "very early, API in flux", 11% documented. **Write our own `meshcore-proto`** and use `michaelhart/meshcore-decoder` plus our Python decoder as test oracles.
- `meshcore-rs` 0.2.0 (MIT, 2026-08-16, port of `meshcore_py`): a **companion-protocol client** over serial/TCP/BLE with send DM/channel, event stream, contacts, repeater login, RX log. **Good candidate for phase 5**; evaluate it then.

### Deployment on truffles
- Multi-stage Dockerfile (rust builder → debian-slim/distroless) and a compose service with a `./data` volume (db + raw log) and a TOML config.
- `network_mode: host`, so the container reaches the broker without depending on `.local` mDNS inside Docker and can advertise itself on mDNS for clients. Alternative: if mosquitto is itself a compose service, join its network and use the service name.
- meshcoretomqtt publishes at **QoS 0 with no retain**, so the broker never queues for ferromeshd: anything published while it's down is gone. Keep restarts short (`restart: unless-stopped`) and backfill gaps with `ferromeshd import` from another capture if needed. Imported copies of messages already stored count as duplicates.
- Phase 5: pass the companion's USB device into the container (`devices: /dev/serial/by-id/...`), or point it at a WiFi companion's `host:5000`.
- Clients: release binaries for macOS arm64 and Linux amd64 (and `cargo install` for development).

## 3. Persistence evaluation

Workload: ~11.5k small rows/day (maybe 10–50× if NashMesh-wide later), a single writer, a few concurrent readers, relational lookups (channel↔message↔node↔observer), text search, time-range scans, and rebuild-from-raw.

| Option | Fit | Verdict |
|---|---|---|
| **SQLite** (WAL, rusqlite, FTS5) | Zero ops, one file on a volume, ~1 GB/yr est., readers alongside a batched single writer, full SQL + FTS5, trivial backup. DuckDB can attach it read-only | **Primary** |
| PostgreSQL (+Timescale) | Strong SQL, LISTEN/NOTIFY, pg_trgm. Easy on truffles via Docker, but it's a second service to run with little gain at this volume | Revisit if multi-app / NashMesh-scale |
| DuckDB | Great analytics; poor for a stream of tiny inserts; single-process write lock | **Analysis sidecar** |
| ClickHouse / QuestDB / InfluxDB | Built for far higher ingest; weak text + relational | Overkill |
| redb / fjall / sled | Tiny and fast, but we'd hand-build indexes and a query engine | No |
| NATS JetStream / Redpanda | Native replay-then-live, but still needs a query DB | Not needed |
| JSONL.zst / Parquet files | Immutable archive | **Raw log / cold archive** |
| Tantivy | Real full-text engine | Only if FTS5 falls short |

### Schema sketch (SQLite)
```
sources        (id, kind{mqtt,companion}, name, config)
observers      (id, pubkey UNIQUE, name, iata, source_id, first_seen, last_seen)
observer_status(observer_id, ts, battery_mv, noise_floor, uptime_s, tx_air_s, rx_air_s, recv_errors, queue_len, raw_json)
packets        (id, hash BLOB(8) UNIQUE, first_seen, payload_type, route_type, payload BLOB,
                chan_hash INT NULL, dst_hash INT NULL, src_hash INT NULL, decode_state)
observations   (id, packet_id, observer_id, rx_ts, snr, rssi, score, hash_size, hops, path BLOB, transport BLOB NULL)
channels       (id, name, secret BLOB, hash INT, kind{public,hashtag,key,discovered}, enabled, added_at)
messages       (id, packet_id NULL UNIQUE, channel_id NULL, dm_contact NULL, direction{rx,tx}, sender_ts,
                txt_type, sender_name, body, first_seen, delivery{sent,acked,failed} NULL)
messages_fts   FTS5(sender_name, body)
nodes          (pubkey PK, name, role, lat, lon, first_seen, last_seen, advert_count, sig_ok)
node_names     (pubkey, name, first_seen, last_seen)
adverts        (packet_id, pubkey, adv_ts, flags, lat, lon, name, sig_ok)
contacts       (pubkey PK, name, synced_from_companion_at)                 -- phase 5
watches        (id, name, filter, actions JSON, cooldown_s, enabled)
alert_events   (id, watch_id, ref_kind, ref_id, ts, delivered JSON)
```
`messages` covers channel messages decoded from MQTT, DMs delivered by the companion (no packet row, or linked by hash when we can match), and messages you send. Row ids are monotonic and double as stream cursors.

## 4. Key mechanisms

### 4.1 Backfill-then-live, no gaps or dupes
*Built in phase 2.* A client opens `GET /api/v1/stream?kind=&filter=` with one of `after=<id>` (resume), `since=<time>`, or `last=<n>`; with none it starts live. The server:
1. Subscribes to the broadcast of newly committed rows.
2. Reads the newest id `H` for the kind, replays matching history with `id ≤ H` oldest first, and sends `caught_up {last_id}`.
3. Forwards live events with `id > H` that match the filter.

If a subscriber falls behind the broadcast buffer, the server resubscribes and fills the gap from the database, so the client never has to resync. A client that loses its connection reconnects with `after=<last id it saw>`.

### 4.2 Stream event kinds
Built: `message` (decoded, once per packet), `packet` (first sighting), and `observation` (every reception). Each kind has its own id sequence and its own stream. Still to come: `node` (new or changed advert), `status` (observer health), `channel` (added / backfill progress), `alert`.

### 4.3 Filter language (TUI, CLI, watches, later web)
*Built in phase 2* (`ferromesh-model::filter`). Terms must all match, commas give alternatives, a leading `-` negates, and double quotes allow spaces:
```
chan:#bna-bot,#bna-wx  from:BNA*  storm          messages (a bare word searches the body)
type:advert,grp_txt  node:4d1727  -chan:#test    packets
observer:Tanyard  snr>-5  rssi<-100  hops>10     observations
```
The same filter compiles to SQL for history and runs in memory for live events. Case folding is ASCII-only on both sides, and a store test checks they select identical rows. Time windows are `--since`/`--until` flags and API parameters, not filter terms. Not yet built: regex text search, `role:`, and `dm:me` (phase 5).

### 4.4 Channels, back-decode, discovery
- `ferromesh channels add '#name' | --key <b64>` (TUI too) stores the channel and starts a backfill job: undecrypted GRP_TXT/GRP_DATA with a matching hash → HMAC check → decrypt → `messages`, streamed as progress.
- Discovery lists unknown channel hashes by volume and first/last seen, and runs a guessing job over a wordlist that includes hashtags seen mentioned in decoded text, node names, and TN/regional place names. Hits are marked `discovered` so you can review and keep them.

### 4.5 Watches (v1 = active filter)
A watch is a saved filter with a colour and a bell, evaluated inline in the pipeline. The TUI highlights matches in any view and keeps an "alerts" pane. Sinks (ntfy etc.) are a later phase that plugs into the same watch record.

### 4.6 Your own traffic and sending: companion radio on truffles
The repeater can't originate chat, and the MQTT feed only shows ciphertext for DMs. The clean route to both "see my stuff" and "send from my computer without Bluetooth" is:

- **A second radio flashed with companion firmware, attached to truffles** by USB serial (or on WiFi, where the companion firmware serves TCP port 5000). ferromeshd acts as its "phone app" via the companion protocol (`meshcore-rs`, or our own thin client).
- **This is your identity.** DMs are decrypted on the radio and handed over as plaintext, so no private key ever leaves the device or lands in our DB. Your contacts sync from it.
- **Send:** the TUI (later web) calls `POST /send {channel|contact, text}`; the server relays to the companion and tracks the ACK (sent → acked/failed) in `messages.delivery`.
- **Bonus:** its RX log is a second observation source (a second vantage point on the mesh).
- The repeater stays a repeater. Keep the companion a short distance from it; a companion radio is inexpensive.

Things to verify in phase 5: whether the companion can also stay paired to your phone concurrently (the device has a single message queue, so clients may compete), and whether the Heltec V4 companion build has a WiFi or USB variant or whether USB-serial is the practical route.

## 5. Phases

| # | Deliverable | Exit criteria |
|---|---|---|
| 0 ✅ | Workspace + `meshcore-proto` + golden tests from the capture (done 2026-09-12; fixture via `tools/gen_fixture.py`) | All 8,853 packets parse. Computed payload len = MQTT `payload_len` and computed hash = MQTT `hash`. Python results reproduced (426 unique msgs, 1,412 adverts). Advert signatures verify |
| 1 🟡 | `ferromesh-store` + `ferromeshd` MQTT source + raw log + `import` of old capture + **Docker image & compose on truffles**. Code done 2026-09-13 and tested live from the Mac (serve, import, rebuild with matching digest). Truffles deploy and the 24h run are pending. | Runs 24h on truffles with no loss; `rebuild` from raw gives an identical DB |
| 2 ✅ | API: REST query + WS stream (backfill→live); `ferromesh tail` / `query`. Done 2026-09-13: integration tests cover resume, filters and lag catch-up. A live run across a forced server restart delivered 57 consecutive observation ids, matching the database exactly. | `tail chan:#test --last 50` shows history then live, with no gap or dupe across a forced reconnect |
| 3 | Dynamic channels + back-decode + discovery (CLI) | `channels add '#chattanooga'` backfills its 37 packets; discovery lists `0x81` etc. |
| 4 | **TUI**: channels, feed, RF view, nodes, packet inspector, filter bar, watches-as-highlights | Usable from a Mac and a Linux box on the LAN |
| 5 | Companion source + send: own DMs, contacts, send channel/DM from TUI with ACK status | Send to `#test` from the TUI and see it echoed back via the repeater's MQTT feed |
| 6 | Web UI (stack TBD) | Live feed + chat + search + nodes |
| 7 | Alert sinks (ntfy), map, health charts, NashMesh source, retention/Parquet archive | as scoped then |

## 6. Open questions

**Answered**, see §0: host, scope, dynamic channels, own DMs (via companion), identity, alerts timing, web deferral, send direction.

**Accepted proposals** (2026-09-12)
- LAN access: reads are open; writes (send, channel keys) require a token in the client config.
- Retention: keep everything (raw + decoded), ~1 GB/yr.
- Ordering: observer receive time, with the sender timestamp shown as secondary.
- Display: each message once with a "heard ×N via paths" badge; every reception appears in the RF view.

**Still open**
1. **Web frontend stack.** You're deciding (not blocking phases 0–5).
2. **Companion hardware.** Spare radio? USB into truffles OK physically (antenna placement)? (Phase 5.)
3. **Companion sharing.** Dedicated to the server, or should your phone still use it? (Phase 5.)
4. **Mosquitto on truffles.** Container (join its compose network) or host (host networking)? (Phase 1.)

## References
- MeshCore packet format: https://github.com/meshcore-dev/MeshCore/blob/main/docs/packet_format.md
- MeshCore payloads: https://github.com/meshcore-dev/MeshCore/blob/main/docs/payloads.md
- Packet hash / serialization: https://github.com/meshcore-dev/MeshCore/blob/main/src/Packet.cpp
- MeshCore FAQ (repeater / room server / companion roles): https://github.com/meshcore-dev/MeshCore/blob/main/docs/faq.md
- meshcoretomqtt (feed format): https://github.com/Cisien/meshcoretomqtt
- TS decoder (test oracle): https://github.com/michaelhart/meshcore-decoder
- meshcore-rs (companion protocol client): https://docs.rs/crate/meshcore-rs/latest
- meshcore-cli (TCP/serial companion CLI, protocol reference): https://pypi.org/project/meshcore-cli/
- meshcore-proxy (USB/BLE companion → TCP): https://github.com/rgregg/meshcore-proxy
- CoreScope (prior art): https://github.com/Kpa-clawbot/CoreScope
- Rust `MeshCore` crate: https://docs.rs/crate/MeshCore/latest
- MeshCore flasher: https://flasher.meshcore.io/
- NashMesh MQTT settings: https://nashme.sh/mqtt/
