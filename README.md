# ferromesh

Records and explores [MeshCore](https://github.com/meshcore-dev/MeshCore) mesh traffic from MQTT, in Rust.

A MeshCore repeater running observer firmware (or [meshcoretomqtt](https://github.com/Cisien/meshcoretomqtt)) publishes every packet it hears to an MQTT broker. ferromesh subscribes, keeps every message verbatim, decodes what it can (adverts, plus channel messages for channels you know), stores it all in SQLite, and serves it to clients that show history and follow live traffic.

**Status:** early. Decoding, storage, the API, channel discovery, a command-line client and a terminal UI work. Alerts beyond the terminal UI, and sending, are planned; see [PLAN.md](PLAN.md).

## Layout

| Path | What |
|---|---|
| `crates/meshcore-proto` | Packet parsing, hashing, advert signature checks, channel encryption and decryption, and the companion radio protocol. No I/O. |
| `crates/ferromesh-model` | Events, the filter language, and the API's wire format, shared by server and clients. |
| `crates/ferromesh-store` | SQLite schema, ingest, queries and channel backfill. |
| `crates/ferromeshd` | The server: MQTT and companion-radio ingest, raw log, HTTP/WebSocket API, and `import`, `rebuild`, `stats` commands. |
| `crates/ferromesh` | The client: `tail`, `query`, `channels`, `dms` and the `tui` terminal UI. |
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

The container uses host networking, so set `mqtt.host` to `127.0.0.1`; `.local` names don't resolve inside containers. The API listens on port 7373. Anyone who can reach it can read; changes, such as adding channels, need `api.token` set in the config and sent by clients.

Every MQTT message is appended to `data/raw/` before it reaches the database, so the database can always be recreated:

- `ferromeshd import capture.jsonl` loads a capture written by a meshcoretomqtt watcher script.
- `ferromeshd rebuild [--replace]` rebuilds the database from the raw log and checks it matches.
- `ferromeshd stats` prints row counts.

### A companion radio

A second radio, flashed with MeshCore's stock **companion** firmware and plugged in by USB, adds your own vantage point and your own messages. Add to the config:

```toml
[companion]
device = "auto"   # the one Espressif USB device, or a path such as /dev/serial/by-id/usb-Espressif_...
```

ferromeshd then records every packet the radio hears as observations beside your MQTT observers', stores direct messages sent to it (the radio decrypts them; its private key never leaves it), and logs its battery, noise floor and packet counts every 5 minutes. It warns if the radio stops hearing anything, and reconnects if it's unplugged. ferromeshd must be the radio's only client: don't pair a phone with it too, because whichever client fetches a queued message takes it. So far ferromeshd only listens; sending is next.

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
ferromesh dms                                 # direct messages to your companion radio
```

`tail` reconnects by itself and resumes after the last event it printed, so a dropped connection or a server restart doesn't lose or repeat anything the server stored.

Filters are space-separated terms that must all match: `chan:#test,#wx`, `from:BNA*`, a bare word to search message text, `type:advert`, `node:4d1727`, `observer:Tanyard`, and `'snr>-5'`, `'rssi<-100'` or `'hops>2'` for observations. A leading `-` negates a term. Quote `>` and `<` so the shell leaves them alone, and put double quotes around values with spaces: `'from:"BNA Bot"'`. One quoted argument can hold a whole filter: `'type:advert snr>-5'`.

## Terminal UI

```sh
ferromesh tui
```

Five views, switched with `1` to `5`:

- **Messages:** a channel list with unread counts, and each message once with how many times it was heard.
- **Packets:** every distinct packet, decoded where possible.
- **RF:** every reception, with its signal strength and its path, naming repeaters where the hop prefix identifies one.
- **Nodes:** every node that has advertised.
- **Alerts:** your watches, and new traffic that matched them.

`Enter` opens the inspector on the selected packet: each reception's signal and path, and the frame's bytes labelled field by field. `/` filters the current view, using the same filter language as `tail`. `w` saves a filter as a watch: matching traffic is highlighted, and new matches ring the bell and land in Alerts. Scrolling past the oldest row loads older history from the server. `?` lists every key.

Watches are kept in `~/.config/ferromesh/watches.toml`. Defaults for `server` and `token` can go in `~/.config/ferromesh/config.toml`, so a bare `ferromesh tui` finds your server.

`ferromesh tui --snapshot --size 120x40 --keys '3<enter>'` prints one screen as plain text, once everything has loaded, after pressing the given keys.

## Channels

Everything is stored, including channel traffic nobody can read yet, so adding a channel later decodes its history too:

```sh
ferromesh channels                            # what the server decrypts
ferromesh channels unknown                    # channel hashes on traffic no known key opens
ferromesh channels guess chattanooga tn-east  # try hashtag names, plus common and mentioned ones
export FERROMESH_TOKEN=...                    # the server's api.token
ferromesh channels add '#chattanooga'         # add it and decrypt what was waiting
ferromesh channels add 'My Group' --key SECRET  # hex, as in a MeshCore QR code, or base64
```

Hashtag channels derive their key from the name, which is why guessing works; private channels need their key. A guess only counts when the key both passes the packet's MAC and decrypts to readable text. Channels listed in the server's config are added, and backfilled, at startup.

## The API

- `GET /api/v1/{messages,packets,observations}?filter=&limit=&since=&until=` returns history.
- A WebSocket at `/api/v1/stream?kind=&filter=&last=` sends history and then live events.
- `GET /api/v1/channels`, `GET /api/v1/channels/unknown`, `POST /api/v1/channels/guess`, and `POST /api/v1/channels` (with `Authorization: Bearer <token>`) manage channels.
- `GET /api/v1/nodes?limit=` lists nodes, most recently heard first.
- `GET /api/v1/packets/{hash}` returns one packet with every reception's raw frame.
- `GET /api/v1/direct?limit=` returns direct messages to your companion radio, newest first.

See `crates/ferromesh-model/src/wire.rs`.

## Tests

```sh
cargo test --workspace
```

The golden tests check the decoder against an independent Python decoder over a real capture. That capture contains other people's messages and node locations, so it isn't in this repository, and the golden tests skip unless you generate the fixture locally:

```sh
tools/gen_fixture.py --capture path/to/meshcore_packets.jsonl --decoder path/to/decoder-dir --lines 9193
```

Set `FERROMESH_REQUIRE_FIXTURES=1` to make missing fixtures fail the tests instead of skipping them. CI runs formatting, clippy and the tests on every push.

## License

MIT; see [LICENSE](LICENSE).
