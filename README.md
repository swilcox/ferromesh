# ferromesh

Records and explores [MeshCore](https://github.com/meshcore-dev/MeshCore) mesh traffic from MQTT, in Rust.

A MeshCore repeater running observer firmware (or [meshcoretomqtt](https://github.com/Cisien/meshcoretomqtt)) publishes every packet it hears to an MQTT broker. ferromesh subscribes, keeps every message verbatim, decodes what it can (adverts, plus channel messages for channels you know), stores it all in SQLite, and serves it to clients that show history and follow live traffic.

**Status:** early. Decoding, storage, the API, channel discovery, a command-line client, a terminal UI, and sending through a companion radio work. Alerts beyond the terminal UI are planned; see [PLAN.md](PLAN.md).

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

ferromeshd then records every packet the radio hears as observations beside your MQTT observers', stores direct messages sent to it (the radio decrypts them; its private key never leaves it), and logs its battery, noise floor and packet counts every 5 minutes. It warns if the radio stops hearing anything, and reconnects if it's unplugged. ferromeshd must be the radio's only client: don't pair a phone with it too, because whichever client fetches a queued message takes it. Sending goes through it too: to a channel, or directly to a node (see below). ferromeshd gives a channel one of the radio's slots the first time you send to it, and adds a node to the radio's contacts from its advert. It also sets the radio's clock when it's behind, so messages carry the right time.

The radio holds a few hundred contacts, and a busy mesh has more nodes than that, most of them repeaters, which don't need to be contacts. So ferromeshd sets the radio to add only chat radios as it hears them, replacing the contact it heard from least recently once full. Favourites are never replaced: everyone you exchange direct messages with becomes one, and `ferromesh contacts pin NAME` makes any other node one (a repeater you administer, a room server). If a direct message arrives that the radio can't read because it has never heard, or has forgotten, the sender, ferromeshd adds the possible senders from everything it has recorded, so the sender's automatic retry can be read. `ferromesh contacts` lists what's on the radio.

In Docker, pass the radio into the container with a `compose.override.yaml` beside `compose.yaml` (Compose merges it automatically), and set `device = "/dev/companion"`:

```yaml
services:
  ferromeshd:
    devices:
      - /dev/ttyACM0:/dev/companion   # the radio's port on the host
    group_add:
      - "20"                          # the host's dialout group, which owns the port
```

A replugged radio can come back as a different `ttyACM` device; update the path and run `docker compose up -d`.

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
ferromesh health                              # how each observer is doing (--hours 168 for a week)
```

`health` reads the status reports observers send every few minutes: battery, noise floor, packets received and sent, receive errors, airtime and restarts, with hourly trends. It also checks delivery: the packets each observer counted receiving against the receptions stored from it, so any loss between the radio and the database shows up. Warnings flag an overdue report, a restart, a low battery, a noise floor well above its usual level, and delivery below 99%. The TUI shows the same on its Health view (`6`).

### Sending

With a companion radio attached and the server's token:

```sh
export FERROMESH_TOKEN=...                    # the server's api.token
ferromesh send '#test' hello from the terminal
ferromesh send KK4SW are you there?           # a direct message, by name or key prefix
ferromesh contacts                            # the radio's contacts; ★ marks pinned ones
ferromesh contacts pin Tanyard
```

`send` then follows the message for 20 seconds (`--follow`). For a channel message it shows who heard it: ferromesh knows the exact packet the radio will send, so every observer's reception of it counts, including your repeater's. For a direct message it shows whether the recipient acknowledged it, and how long that took. Every send is kept in the outbox (`GET /api/v1/outbox`). A channel must be added first, and a node must have been heard advertising.

`tail` reconnects by itself and resumes after the last event it printed, so a dropped connection or a server restart doesn't lose or repeat anything the server stored.

Filters are space-separated terms that must all match: `chan:#test,#wx`, `from:BNA*`, a bare word to search message text, `type:advert`, `node:4d1727`, `observer:Tanyard`, and `'snr>-5'`, `'rssi<-100'` or `'hops>2'` for observations. A leading `-` negates a term. Quote `>` and `<` so the shell leaves them alone, and put double quotes around values with spaces: `'from:"BNA Bot"'`. One quoted argument can hold a whole filter: `'type:advert snr>-5'`.

## Terminal UI

```sh
ferromesh tui
```

Six views, switched with `1` to `6`:

- **Messages:** a channel list with unread counts, and each message once with how many times it was heard.
- **Packets:** every distinct packet, decoded where possible.
- **RF:** every reception, with its signal strength and its path, naming repeaters where the hop prefix identifies one.
- **Nodes:** every node that has advertised.
- **Alerts:** your watches, and new traffic that matched them.
- **Health:** each observer's battery, noise floor, traffic and delivery, with trends and warnings.

`c` composes a message to the selected channel (it needs the token, from `--token`, `FERROMESH_TOKEN` or the config file). `Enter` opens the inspector on the selected packet: each reception's signal and path, and the frame's bytes labelled field by field. `/` filters the current view, using the same filter language as `tail`. `w` saves a filter as a watch: matching traffic is highlighted, and new matches ring the bell and land in Alerts. Scrolling past the oldest row loads older history from the server. `?` lists every key.

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
- `GET /api/v1/observers?hours=` returns each observer's health: figures, hourly history, and warnings.
- `GET /api/v1/contacts` lists the companion radio's contacts; `POST /api/v1/contacts` (with the token) pins or unpins one: `{"to": ..., "pinned": true}`.
- `POST /api/v1/send` (with the token) sends `{"to": ..., "text": ...}` through the companion radio; `GET /api/v1/outbox?limit=` lists what was sent, with who heard it or whether it was acknowledged.

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
