# Development

## Layout

| Path | What |
|---|---|
| `crates/meshcore-proto` | Packet parsing, hashing, advert signature checks, channel encryption and decryption, and the companion radio protocol. No I/O. |
| `crates/ferromesh-model` | Events, the filter language, and the API's wire format, shared by server and clients. |
| `crates/ferromesh-store` | SQLite schema, ingest, queries, health, and channel backfill. |
| `crates/ferromeshd` | The server: MQTT and companion ingest, the raw log, the HTTP/WebSocket API, and the `import`, `rebuild` and `stats` commands. |
| `crates/ferromesh` | The client: `tail`, `query`, `channels`, `dms`, `health`, `send`, `contacts`, and the `tui`. |
| `tools/gen_fixture.py` | Builds the golden-test fixture from a capture. |

The shape of it: a source produces a **raw record** (a topic and a JSON payload), which is appended to the raw log and then parsed into a `Message` by `source::parse`. Sources therefore differ only in how they produce records — MQTT messages arrive as they come, and the companion's frames are wrapped into records under `companion/<pubkey>/...` topics — and everything downstream is shared. That's also why a rebuild from the raw log reproduces the database exactly.

Writes all funnel through one writer thread, which owns the SQLite connection and the raw log, so there is no write contention and no partial state. Readers open their own connections.

## Tests

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --check
```

CI runs all three on every push.

Unit tests cover the codecs, the store and the client's rendering; integration tests cover the API, including stream resume, filters and a lagging consumer catching up. The companion tests drive a fake radio, so they exercise the session logic with no hardware.

### The golden fixture

The decoder is checked against an independent Python decoder over a real capture. That capture holds other people's messages and node locations, so it isn't in this repository, and the golden tests skip without it:

```sh
tools/gen_fixture.py --capture path/to/meshcore_packets.jsonl --decoder path/to/decoder-dir --lines 9193
```

Set `FERROMESH_REQUIRE_FIXTURES=1` to make missing fixtures fail instead of skip — worth doing on a machine that has them.

## Working on the protocol

`meshcore-proto` is pure: bytes in, structures out. Anything that touches a port, a socket or a database belongs in `ferromeshd`. When adding a companion command, put the encoder in `companion.rs` beside the others, add its reply to the `Frame` enum, and cover it with a round-trip test; the session layer in `ferromeshd/src/companion/session.rs` then drives it against `FakeRadio`.

A few protocol facts that shape the code:

- Companion frames are length-prefixed: `<` or `>`, then a u16 little-endian length.
- `DEVICE_QUERY` with version 3 asks for the v3 message frames, which carry the fields we need.
- A channel message's plaintext is `timestamp | 0 | "name: text"`, and the limit is 160 characters.
- Contacts carry a flags byte; bit 0 marks a favourite, which the radio won't evict.
- The radio takes one client at a time, and a fetched message is gone from its queue.

## References

- MeshCore packet format: https://github.com/meshcore-dev/MeshCore/blob/main/docs/packet_format.md
- MeshCore payloads: https://github.com/meshcore-dev/MeshCore/blob/main/docs/payloads.md
- Packet hash and serialization: https://github.com/meshcore-dev/MeshCore/blob/main/src/Packet.cpp
- MeshCore FAQ (repeater, room server and companion roles): https://github.com/meshcore-dev/MeshCore/blob/main/docs/faq.md
- meshcoretomqtt (the MQTT feed format): https://github.com/Cisien/meshcoretomqtt
- TypeScript decoder, used as the test oracle: https://github.com/michaelhart/meshcore-decoder
- meshcore-cli (companion protocol reference): https://pypi.org/project/meshcore-cli/
- meshcore-rs (a Rust companion client): https://docs.rs/crate/meshcore-rs/latest
- meshcore-proxy (USB/BLE companion to TCP): https://github.com/rgregg/meshcore-proxy
- CoreScope (prior art): https://github.com/Kpa-clawbot/CoreScope
- The `@[Name]` mention convention, which MeshCore doesn't document, as other clients implement it:
  [MeshMonitor](https://github.com/Yeraze/meshmonitor/blob/main/src/components/MeshCore/MeshCoreMessageStream.tsx),
  [meshcadet](https://github.com/jagoda/meshcadet/blob/main/protocol/src/mention.rs),
  [MeshCoreOne](https://github.com/Avi0n/MeshCoreOne/blob/main/MC1Services/Sources/MC1Services/Utilities/MentionUtilities.swift)
- MeshCore flasher: https://flasher.meshcore.io/
