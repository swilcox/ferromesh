# Running the server

`ferromeshd` records what your sources hear and serves it to clients. It needs a data directory it can write to, at least one source, and a port.

## Building

Rust 1.94 or newer (2024 edition).

```sh
cargo build --release -p ferromeshd     # target/release/ferromeshd
cargo run --release -p ferromeshd -- serve
```

`--config` (or `FERROMESH_CONFIG`) points at the configuration file; it defaults to `ferromesh.toml` in the working directory.

## Configuration

Copy `ferromesh.example.toml` to `ferromesh.toml`. Unknown keys are rejected rather than ignored, so a typo fails at startup instead of going quiet.

```toml
data_dir = "/data"          # holds ferromesh.db and raw/
```

### Sources

At least one of `[mqtt]` and `[companion]` is required; with neither, the server has nothing to record and says so.

```toml
[mqtt]
host = "127.0.0.1"
port = 1883                 # default
client_id = "ferromeshd"    # must be unique per instance, or the broker
                            # disconnects whichever connected first
username = "..."            # optional
password = "..."            # optional
topics = ["meshcore/+/+/packets", "meshcore/+/+/status"]
```

The broker is polled for reconnection every 5 seconds if it's unreachable, so the server survives a broker restart. Delete the whole section to run from a radio alone.

```toml
[companion]
device = "auto"             # the one Espressif USB device, or a path such as
                            # /dev/serial/by-id/usb-Espressif_... or /dev/cu.usbmodem2101
```

See [companion.md](companion.md) for what the radio does and how to set it up.

### The API

```toml
[api]
listen = "0.0.0.0:7373"     # default
token = "..."               # at least 16 characters; openssl rand -hex 24
```

Anyone who can reach the address can read. Changes — adding a channel, sending a message, pinning a contact — need the token, which clients send as `--token` or `FERROMESH_TOKEN`. Without a token configured, the API is read-only.

### Channels

```toml
[[channel]]
name = "#test"              # a hashtag channel derives its key from the name

[[channel]]
name = "My Group"
key = "0123456789abcdef0123456789abcdef"   # hex, as in a MeshCore QR code, or base64
```

Channels listed here are added at startup and backfilled against everything already stored. More can be added at runtime; see [using.md](using.md#channels).

## Docker

```sh
mkdir -p data                # before the first start, so it isn't owned by root
docker compose up -d --build
```

The container uses host networking, so a broker on the same machine is `127.0.0.1` — `.local` names don't resolve inside containers. The data directory is mounted at `/data`.

To pass a companion radio in, put a `compose.override.yaml` beside `compose.yaml`; Compose merges it automatically, and it stays out of version control:

```yaml
services:
  ferromeshd:
    devices:
      - /dev/ttyACM0:/dev/companion   # the radio's port on the host
    group_add:
      - "20"                          # the host group that owns the port (dialout)
```

with `device = "/dev/companion"` in the config. A replugged radio can come back as a different `ttyACM` number; update the path and run `docker compose up -d`.

## The raw log

Every record — MQTT messages, and the companion's receptions, messages, status reports, sends and acknowledgements — is appended to `data/raw/` before it reaches the database. The database is therefore disposable:

```sh
ferromeshd rebuild            # rebuild from the raw log and compare with the current one
ferromeshd rebuild --replace  # swap the rebuilt one in, keeping a backup (stop serve first)
ferromeshd stats              # row counts
ferromeshd import capture.jsonl --label mqtt_observer   # load an older JSONL capture
```

`rebuild` compares a digest of both databases, so a decoder change that alters stored data shows up as a mismatch rather than a surprise. Expect roughly 1 GB a year on a busy regional mesh, raw log included.

## Upgrading

Schema migrations run at startup; nothing is needed by hand. Under Docker:

```sh
git pull && docker compose up -d --build
```

The server finishes writing what's queued before it exits, so a restart loses nothing already received.
