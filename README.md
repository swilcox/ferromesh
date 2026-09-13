# ferromesh

Records and explores [MeshCore](https://github.com/meshcore-dev/MeshCore) mesh traffic from MQTT, in Rust.

A MeshCore repeater running observer firmware (or [meshcoretomqtt](https://github.com/Cisien/meshcoretomqtt)) publishes every packet it hears to an MQTT broker. ferromesh subscribes, keeps every message verbatim, decodes what it can (adverts, plus channel messages for channels you know), stores it all in SQLite, and serves it to clients that show history and follow live traffic.

**Status:** early. Decoding, storage, the API and a command-line client work. A terminal UI, back-decoding for newly added channels, alerts and sending are planned; see [PLAN.md](PLAN.md).

## Layout

| Path | What |
|---|---|
| `crates/meshcore-proto` | Packet parsing, hashing, advert signature checks, channel encryption and decryption. No I/O. |
| `crates/ferromesh-model` | Events, the filter language, and the API's wire format, shared by server and clients. |
| `crates/ferromesh-store` | SQLite schema, ingest and queries. |
| `crates/ferromeshd` | The server: MQTT ingest, raw log, HTTP/WebSocket API, and `import`, `rebuild`, `stats` commands. |
| `crates/ferromesh` | The command-line client: `tail` and `query`. |
| `tools/gen_fixture.py` | Builds the golden-test fixture from a capture. |

## Running the server

```sh
cp ferromesh.example.toml ferromesh.toml   # set the broker and your channels
cargo run --release -p ferromeshd -- serve
```

Or with Docker, on the machine running the broker:

```sh
mkdir -p data
docker compose up -d --build
```

The container uses host networking, so set `mqtt.host` to `127.0.0.1`; `.local` names don't resolve inside containers. The API listens on port 7373.

Every MQTT message is appended to `data/raw/` before it reaches the database, so the database can always be recreated:

- `ferromeshd import capture.jsonl` loads a capture written by a meshcoretomqtt watcher script.
- `ferromeshd rebuild [--replace]` rebuilds the database from the raw log and checks it matches.
- `ferromeshd stats` prints row counts.

## Watching traffic

The client only talks to the API, so it works from any machine that can reach the server:

```sh
cargo install --path crates/ferromesh
export FERROMESH_SERVER=truffles.local        # host[:port] or a URL

ferromesh tail                                # the last 20 channel messages, then live
ferromesh tail chan:#test --last 50
ferromesh tail --kind observations 'snr>-5'   # every reception, with signal and path
ferromesh query from:BNA* --since 6h
ferromesh query --kind packets type:advert --json
```

`tail` reconnects by itself and resumes after the last event it printed, so a dropped connection or a server restart doesn't lose or repeat anything the server stored.

Filters are space-separated terms that must all match: `chan:#test,#wx`, `from:BNA*`, a bare word to search message text, `type:advert`, `node:4d1727`, `observer:Tanyard`, and `'snr>-5'`, `'rssi<-100'` or `'hops>2'` for observations. A leading `-` negates a term. Quote `>` and `<` so the shell leaves them alone, and put double quotes around values with spaces: `'from:"BNA Bot"'`. One quoted argument can hold a whole filter: `'type:advert snr>-5'`.

The API itself is small: `GET /api/v1/{messages,packets,observations}?filter=&limit=&since=&until=` returns history, and a WebSocket at `/api/v1/stream?kind=&filter=&last=` sends history and then live events. See `crates/ferromesh-model/src/wire.rs`.

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
