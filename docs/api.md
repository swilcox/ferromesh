# The API

`ferromeshd` serves HTTP and WebSocket on `api.listen`, port 7373 by default. Everything is JSON. The wire types are in [`crates/ferromesh-model/src/wire.rs`](../crates/ferromesh-model/src/wire.rs), and the `ferromesh` client is the reference consumer.

**Authentication.** Reads are open to anyone who can reach the address. Anything that changes something — adding a channel, sending, pinning a contact — needs the server's `api.token` as `Authorization: Bearer <token>`. With no token configured, those endpoints refuse everything.

## History

```
GET /api/v1/{messages,packets,observations}
```

| Parameter | Meaning |
|---|---|
| `filter` | The filter language from [using.md](using.md#the-filter-language) |
| `limit` | Newest matches to return; 100 by default, 1000 at most |
| `since`, `until` | RFC 3339 timestamps |
| `before`, `after` | Event ids, for paging |

Returns events oldest first. Every event carries the id to resume from.

```
GET /api/v1/packets/{hash}
```

One packet by hex hash, with every observer's reception of it and each raw frame rebuilt.

```
GET /api/v1/nodes?limit=
```

Nodes that have advertised, most recently heard first.

## Live stream

```
GET /api/v1/stream?kind=&filter=&last=&after=&since=
```

A WebSocket. It sends history first — `last` events, or everything after an id or timestamp — then live events as they're stored. Resuming with the last id you saw gives no gap and no repeat, which is how `ferromesh tail` survives a reconnect.

## Channels

```
GET  /api/v1/channels             # what the server decrypts
GET  /api/v1/channels/unknown     # channel hashes on traffic no known key opens
POST /api/v1/channels/guess       # {"names": [...], "builtin": true, "mentions": true}
POST /api/v1/channels             # {"name": "#wx"} or {"name": "...", "key": "..."}   (token)
```

Adding a channel backfills: stored traffic that was waiting for the key is decoded and appears in history.

## Messaging

These need a companion radio on the server; without one they answer 503.

```
GET  /api/v1/direct?limit=        # direct messages to the radio, newest first
POST /api/v1/send                 # {"to": "#test" | "KK4SW" | "4d1727", "text": "..."}   (token)
GET  /api/v1/outbox?limit=        # what was sent, with who heard it or whether it was acked
GET  /api/v1/contacts             # the radio's contacts
POST /api/v1/contacts             # {"to": ..., "pinned": true}   (token)
```

`POST /api/v1/send` answers 201 with the message's outbox entry, once the radio has accepted it and the record is stored. Follow what happens to it in the outbox: a channel message collects the observers that heard it, a direct message collects its acknowledgement and round-trip time.

## Health

```
GET /api/v1/health                # the server itself: version, liveness
GET /api/v1/observers?hours=      # every observer: figures, hourly history, warnings
```

`hours` defaults to 24 and allows up to 31 days. Each observer comes back with its current state (online, stale, offline), battery, noise floor, traffic and airtime figures, a delivery share, plain-language warnings, and an hourly history covering the window.
