# ferromesh

Records and explores [MeshCore](https://github.com/meshcore-dev/MeshCore) mesh traffic from MQTT, in Rust.

A MeshCore repeater running observer firmware (or [meshcoretomqtt](https://github.com/Cisien/meshcoretomqtt)) publishes every packet it hears to an MQTT broker. ferromesh subscribes, keeps every message verbatim, decodes what it can (adverts, plus channel messages for channels you know), and stores it all in SQLite.

**Status:** early. Decoding, storage and ingest work. The live terminal and web viewers, alerts and sending are planned; see [PLAN.md](PLAN.md).

## Layout

| Path | What |
|---|---|
| `crates/meshcore-proto` | Packet parsing, hashing, advert signature checks, channel decryption. No I/O. |
| `crates/ferromesh-store` | SQLite schema and ingest. |
| `crates/ferromeshd` | The server: MQTT ingest, raw log, and `import`, `rebuild`, `stats` commands. |
| `tools/gen_fixture.py` | Builds the golden-test fixture from a capture. |

## Running

```sh
cp ferromesh.example.toml ferromesh.toml   # set the broker and your channels
cargo run --release -p ferromeshd -- serve
```

Or with Docker, on the machine running the broker:

```sh
mkdir -p data
docker compose up -d --build
```

The container uses host networking, so set `mqtt.host` to `127.0.0.1`; `.local` names don't resolve inside containers.

Every MQTT message is appended to `data/raw/` before it reaches the database, so the database can always be recreated:

- `ferromeshd import capture.jsonl` loads a capture written by a meshcoretomqtt watcher script.
- `ferromeshd rebuild [--replace]` rebuilds the database from the raw log and checks it matches.
- `ferromeshd stats` prints row counts.

## Tests

```sh
cargo test --workspace
```

The golden tests check the decoder against an independent Python decoder over a real capture. That capture contains other people's messages and node locations, so it isn't in this repository, and the golden tests skip unless you generate the fixture locally:

```sh
tools/gen_fixture.py --capture path/to/meshcore_packets.jsonl --decoder path/to/decoder-dir --lines 9193
```

Set `FERROMESH_REQUIRE_FIXTURES=1` to make missing fixtures fail the tests instead of skipping them.

## License

MIT; see [LICENSE](LICENSE).
