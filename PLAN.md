# ferromesh — MeshCore MQTT ingest, store, watch, alert

Status (2026-09-19):
- **Phase 0 complete:** decoder crate plus golden test.
- **Phase 1 deployed on truffles:** only the 24h no-loss check is pending.
- **Phase 2 complete:** API, stream, and `ferromesh tail`/`query`, verified live across a forced server restart.
- **Phase 3 complete:** channels, back-decode and discovery, verified live on real data.
- **Phase 4 deployed:** the TUI, on truffles since 2026-09-19. An interactive Linux run is pending.
- **Phase 5a complete:** the companion link, verified on the Mac with the Heltec V4 (`scw`) on USB.
- **Phase 5b:** sending works, verified live on the Mac; moving the radio to truffles remains.

Prior art is in `../mqtt_observer`.

## 0. Decisions so far

| Topic | Decision |
|---|---|
| Name | **ferromesh**. Server binary `ferromeshd`, client binary `ferromesh`. crates.io and GitHub names were free as of 2026-09-12. |
| Host | **truffles**: amd64 Linux with Docker and plenty of resources. The server ships as a container (docker compose). |
| Data scope | **Your own repeater's local feed first.** It works without internet. NashMesh-wide ingest is an optional later add-on; the design stays multi-source so it slots in cleanly. |
| Database | **SQLite** (WAL) plus a verbatim raw MQTT log. See §3. |
| Channels | **Dynamic.** Stored in the DB, seeded from the config's `[[channel]]` list, and added through the API or `ferromesh channels add` with a token. Adding one decodes past traffic too, and discovery finds more. |
| Your own traffic | **Via a companion radio** (§4.6), not by extracting private keys: a Heltec V4 on stock USB companion firmware, owned by ferromeshd. Developed on the Mac, then moved to truffles. |
| Sender identity | By display name (the only thing the protocol carries), with a best-effort link to node pubkeys from adverts, shown as a hint. |
| Alerts | v1 = active filters highlighted in live traffic (TUI). Next sink: Home Assistant (on marbles.local) via MQTT discovery, which reaches your phone; ntfy only if still wanted. |
| Order | **Data backend → TUI → companion/send → Home Assistant → web.** |
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
| Unknown channels | 89 distinct channel-hash bytes; `0x81` alone = 606 packets. **Identified in phase 3:** `0x81` is `#wardriving`, a hashtag mentioned in decoded messages. |
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
1. **Raw first, decode second.** Every MQTT message is kept verbatim. Decoded tables can always be rebuilt (`ferromeshd rebuild`).
2. **Clients never touch the DB.** Everything goes through the server API, so clients work from any LAN machine.
3. **One filter model, two evaluators.** The same filter compiles to SQL (history) and an in-memory predicate (live). This gives backfill-then-live and alerts from one piece of code.
4. **Multi-source from day one.** A source is anything that yields observations: an MQTT broker, and later the companion's RX log. Adding NashMesh is a config entry, not a redesign.
5. **One writer.** Every database change, MQTT ingest or channel add, goes through the writer thread in order, so row ids follow commit order and streams see each change once.

### Cargo workspace

| Crate | Role | Key deps |
|---|---|---|
| `meshcore-proto` | Pure, no I/O. Header/transport/path/payload parse, packet hash, advert parse + Ed25519 verify, GRP_TXT/GRP_DATA encrypt and decrypt | `aes`, `hmac`, `sha2`, `ed25519-dalek` |
| `ferromesh-model` | Shared types: events, filter AST + parser, channel types, API/WS wire protocol | `serde`, `jiff` |
| `ferromesh-store` | Schema, migrations, ingest, queries, filter → SQL, channel backfill and guessing | `rusqlite` (bundled, FTS5) |
| `ferromeshd` | Sources (MQTT, later companion), raw log, writer thread, broadcast hub, axum API, later watch engine. Admin subcommands `import`, `rebuild` and `stats` work on the data directory directly | `tokio`, `rumqttc`, `zstd`, `jiff`, `axum`, `tracing`; later `meshcore-rs` |
| `ferromesh` (client) | Built: `tail`, `query`, `channels`. Later: `tui`, `watches`, `send` | `clap`, `reqwest`, `tokio-tungstenite`, `owo-colors`; later `ratatui`, `crossterm` |

External crates evaluated:
- `MeshCore` 0.0.1 (packet parsing, Nov 2025): self-described "very early, API in flux", 11% documented. **Write our own `meshcore-proto`** and use `michaelhart/meshcore-decoder` plus our Python decoder as test oracles.
- `meshcore-rs` 0.2.0 (MIT, 2026-08-16, port of `meshcore_py`): a **companion-protocol client** over serial/TCP/BLE with send DM/channel, event stream, contacts, repeater login, RX log. **Good candidate for phase 5**; evaluate it then.

### Deployment on truffles
- Multi-stage Dockerfile (rust builder → debian-slim/distroless) and a compose service with a `./data` volume (db + raw log) and a TOML config.
- `network_mode: host`, so the container reaches the broker without depending on `.local` mDNS inside Docker and can advertise itself on mDNS for clients. Alternative: if mosquitto is itself a compose service, join its network and use the service name.
- meshcoretomqtt publishes at **QoS 0 with no retain**, so the broker never queues for ferromeshd: anything published while it's down is gone. Keep restarts short (`restart: unless-stopped`) and backfill gaps with `ferromeshd import` from another capture if needed. Imported copies of messages already stored count as duplicates.
- Changes through the API need `[api] token` in the server config (at least 16 characters); clients send it as `--token` or `FERROMESH_TOKEN`. Without a token the API is read-only.
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
The built schema is in `crates/ferromesh-store/src/schema.rs`; rows marked "phase 5" and later are still plans.
```
sources        (id, kind{mqtt,companion}, name, config)
observers      (id, pubkey UNIQUE, name, iata, source_id, first_seen, last_seen)
observer_status(observer_id, ts, battery_mv, noise_floor, uptime_s, tx_air_s, rx_air_s, recv_errors, queue_len, raw_json)
packets        (id, hash BLOB(8) UNIQUE, first_seen, payload_type, route_type, payload BLOB,
                chan_hash INT NULL, dst_hash INT NULL, src_hash INT NULL, decode_state)
observations   (id, packet_id, observer_id, rx_ts, snr, rssi, score, hash_size, hops, path BLOB, transport BLOB NULL)
channels       (id, name, secret BLOB, hash INT, kind{public,hashtag,key}, enabled, added_at)
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
*Built in phase 3.*
- **Adding.** `ferromesh channels add '#name'` (or `'Name' --key <b64>`) posts to `/api/v1/channels` with the token. The writer thread stores the channel, then tries its key on stored undecrypted packets with its hash, 1,000 per transaction. Decrypted GRP_TXT become messages, which also go out on live streams. Channels in the server config are added and backfilled the same way at startup. Without a key, a name must start with `#`.
- **Same result as having it all along.** Back-decoded messages take the packet's first-seen time, so backfilling gives the same database digest as having had the channel from the start (store test, and a live `rebuild` check).
- **Discovery.** `ferromesh channels unknown` lists channel hashes on undecrypted traffic, busiest first, noting any known channel sharing the hash byte.
- **Guessing.** `ferromesh channels guess [names]` tries names you pass, hashtags mentioned in recent decoded messages, and a built-in list of common and US-state names. A name counts only if its key passes the MAC and decrypts at least one packet to readable text, because the two-byte MAC lets a wrong key through about once in 65,536 packets. `--add` adds every hit. There is no separate `discovered` kind: hits are either added, or not.
- **Not built:** removing or disabling a channel (the `enabled` column exists), and guessing from node names or place names beyond the built-in list.

### 4.5 Watches (v1 = active filter)
A watch is a saved filter with a colour and a bell, evaluated inline in the pipeline. The TUI highlights matches in any view and keeps an "alerts" pane. Sinks (ntfy etc.) are a later phase that plugs into the same watch record.

### 4.6 Your own traffic and sending: companion radio on truffles
The repeater can't originate chat, and the MQTT feed only shows ciphertext for DMs. The clean route to both "see my stuff" and "send from my computer without Bluetooth" is:

- **A second radio flashed with companion firmware, attached to truffles** by USB serial (or on WiFi, where the companion firmware serves TCP port 5000). ferromeshd acts as its "phone app" via the companion protocol (`meshcore-rs`, or our own thin client).
- **This is your identity.** DMs are decrypted on the radio and handed over as plaintext, so no private key ever leaves the device or lands in our DB. Your contacts sync from it.
- **Send:** the TUI (later web) calls `POST /send {channel|contact, text}`; the server relays to the companion and tracks the ACK (sent → acked/failed) in `messages.delivery`.
- **Bonus:** its RX log is a second observation source (a second vantage point on the mesh).
- The repeater stays a repeater. Keep the companion a short distance from it; a companion radio is inexpensive.

Settled 2026-09-19, from the firmware source (`examples/companion_radio/MyMesh.cpp`):
- **One client.** Stock companion firmware tracks one connection, and whichever client fetches a waiting message consumes it. So ferromeshd is the radio's only client; the TUI, web and Home Assistant all go through ferromesh's API. Your phone stays off this radio.
- **Stock USB firmware.** USB is always connected and passes into Docker with a `devices:` line. Sending over MQTT isn't possible with any firmware: MQTT firmwares (OffbandMesh and similar) only publish what they hear. meshcomod's simultaneous USB, BLE and TCP adds nothing when there is a single client.
- **What the protocol gives us:** `CMD_SEND_TXT_MSG` and `CMD_SEND_CHANNEL_TXT_MSG`; `RESP_CODE_SENT` with the expected ACK and a timeout, then `PUSH_CODE_SEND_CONFIRMED` with the round-trip time; a queue drained with `CMD_SYNC_NEXT_MESSAGE` after `PUSH_CODE_MSG_WAITING`; `PUSH_CODE_LOG_RX_DATA` with every raw packet heard plus SNR and RSSI (only while a client is connected); contacts, channel slots, clock and adverts; repeater login, status and telemetry requests; trace and path discovery; and `CMD_SEND_RAW_PACKET`.

### 4.7 Where ferromesh differs from existing tools
Capture analyzers such as [CoreScope](https://github.com/Kpa-clawbot/CoreScope) read MQTT and can't send. Companion clients such as [RemoteTerm](https://github.com/jkingsman/Remote-Terminal-for-MeshCore) and [MeshMonitor](https://meshmonitor.org/features/meshcore.html) send, but only know what their own radio heard. ferromesh joins the two:
1. **Visible delivery.** Each message you send is matched against every observer (Tanyard, the companion, later NashMesh), giving which repeaters carried it, by which paths, at what SNR, and whether the ACK returned. Over time your own sends map your coverage.
2. **One radio owner, many front ends,** behind one token-protected API and outbox.
3. **One verbatim archive** of every observer, including your own DMs, rebuildable from the raw log.
4. **Terminal first,** plus a small Rust binary.
5. **Network health from your own repeater's view,** fed to Home Assistant.
Deliberately not chased yet: bots, auto-responders, Meshtastic.

## 5. Phases

| # | Deliverable | Exit criteria |
|---|---|---|
| 0 ✅ | Workspace + `meshcore-proto` + golden tests from the capture (done 2026-09-12; fixture via `tools/gen_fixture.py`) | All 8,853 packets parse. Computed payload len = MQTT `payload_len` and computed hash = MQTT `hash`. Python results reproduced (426 unique msgs, 1,412 adverts). Advert signatures verify |
| 1 🟡 | `ferromesh-store` + `ferromeshd` MQTT source + raw log + `import` of old capture + **Docker image & compose on truffles**. Code done 2026-09-13 and tested live from the Mac (serve, import, rebuild with matching digest). Truffles deploy and the 24h run are pending. | Runs 24h on truffles with no loss; `rebuild` from raw gives an identical DB |
| 2 ✅ | API: REST query + WS stream (backfill→live); `ferromesh tail` / `query`. Done 2026-09-13: integration tests cover resume, filters and lag catch-up. A live run across a forced server restart delivered 57 consecutive observation ids, matching the database exactly. | `tail chan:#test --last 50` shows history then live, with no gap or dupe across a forced reconnect |
| 3 ✅ | Dynamic channels + back-decode + discovery (CLI). Done 2026-09-13 on the Mac's copy of the imported capture. `channels add '#chattanooga'` decrypted 32 of 56 waiting packets and `#tn-east` 27, both matching an independent Python HMAC count. (The plan's "37" counted relayed copies in a different window.) `unknown` listed `81` first, `guess` identified it as `#wardriving` (651 packets), and a rebuild matched the backfilled database's digest. | `channels add '#chattanooga'` backfills its packets; discovery lists `0x81` etc. |
| 4 🟡 | **TUI**: channels, feed, RF view, nodes, packet inspector, filter bar, watches-as-highlights. Built 2026-09-13 as `ferromesh tui`, plus the server endpoints `/api/v1/nodes` and `/api/v1/packets/{hash}`. Tested on the Mac against a local server: text snapshots of every view (`--snapshot --keys`), and a real terminal session that drew the views, quit cleanly and restored the terminal. Lists sort by receive time, so a message decrypted when its channel is added appears where it was heard. The Linux run and the truffles deploy are pending. | Usable from a Mac and a Linux box on the LAN |
| 5a ✅ | **Companion link, read-only.** A Rust driver for the companion protocol (USB serial; TCP uses the same framing): app start, device info, contacts, the message queue. The companion's receptions become a second observer, and DMs to it are stored as messages. Developed on the Mac. Built 2026-09-19: the codec in `meshcore_proto::companion`, the `[companion]` source in ferromeshd (serial auto-detect, reconnect, status every 5 min, stall warning), a `direct_messages` table, `GET /api/v1/direct` and `ferromesh dms`. Verified live on the Mac: over 3.5 minutes the Heltec (`scw`) and Tanyard heard the same 26 packets; a rebuild from the raw log matched the digest; after three flood adverts from `scw`, a DM from KK4SW's phone was stored and named its sender. Lesson: a companion can only decrypt DMs from contacts, so the first DM (sent before `scw` had heard KK4SW's advert) went over the air but couldn't be read. 5c should preload known nodes as contacts. | The companion appears as an observer beside Tanyard, and a DM to it is recorded |
| 5b 🟡 | **Send.** `POST /api/v1/send` (token), an outbox (queued → sent → delivered or failed, with round-trip time), matching each send against what observers heard, a TUI compose line and `ferromesh send`. Then move the radio to truffles. Built 2026-09-19: channel sends take a radio slot on first use and carry a precomputed packet hash, so the outbox counts every observer's reception; direct sends add the recipient as a contact from its advert and track the ACK; the radio's clock is set when behind; sends and ACKs go through the raw log. Verified live on the Mac 2026-09-19: a DM to KK4SW was acknowledged in 0.6 s, and `#test` "hello from ferromesh" was heard 4 times (Tanyard at 0 and 2 hops, `scw` at 1 and 2), matching the precomputed hash. Moving the radio to truffles is what remains. | Send to `#test` from the TUI and see it come back through Tanyard, with a delivery report |
| 5c 🟡 | **Radio management.** Keep the radio's channel slots in step with the server's list, contact policy, clock sync, advert schedule, and Tanyard status and telemetry via repeater login. Done 2026-09-19, ahead of the move to truffles: the contact policy (auto-add chat radios only, overwrite the oldest non-favourite when full; the radio holds 350, and the mesh had 1,132 nodes in six days, 967 of them repeaters), favourites for everyone you exchange DMs with and `ferromesh contacts pin`, rescue of DMs the radio couldn't read, and `ferromesh contacts`. Clock sync came with 5b. Test whether `CMD_SEND_RAW_PACKET` can send to channels without a slot. | Tanyard's battery and noise floor are recorded every N minutes |
| 6 | **Home Assistant bridge** (marbles.local): connect Home Assistant to the Mosquitto on truffles, then publish sensors, watch events and a send command through MQTT discovery | A watch match reaches your phone, and an automation posts to a channel |
| 7 | Web UI (stack TBD) | Live feed + chat + search + nodes |
| 8 | Analytics and more sources: coverage and path maps, node health, NashMesh as an observer, optional forwarding to community maps, retention/Parquet archive | as scoped then |

## 6. Open questions

**Answered**, see §0 and §4.6: host, scope, dynamic channels, own DMs (via companion), identity, alerts timing, web deferral, send direction, companion hardware (Heltec V4, stock USB firmware), companion sharing (server only).

**Accepted proposals** (2026-09-12)
- LAN access: reads are open; changes (channel keys now, sending later) need the server's `api.token`, which clients send as `--token` or `FERROMESH_TOKEN`.
- Retention: keep everything (raw + decoded), ~1 GB/yr.
- Ordering: observer receive time, with the sender timestamp shown as secondary.
- Display: each message once with a "heard ×N via paths" badge; every reception appears in the RF view.

**Still open**
1. **Web frontend stack.** You're deciding (not blocking phases 0–5).
2. **Companion identity.** The node name others will see on what you send. (Phase 5a.)
3. **Radio placement on truffles.** It only needs to reach Tanyard; if truffles' spot is poor, use a Wi-Fi companion build instead. (Phase 5b.)
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
