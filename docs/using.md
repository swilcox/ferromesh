# Using ferromesh

The `ferromesh` client only talks to the server's API, so it runs from any machine that can reach it.

```sh
cargo install --path crates/ferromesh
export FERROMESH_SERVER=truffles.local   # host[:port] or a URL; default localhost
export FERROMESH_TOKEN=...               # only needed for changes
```

Defaults for both can go in `~/.config/ferromesh/config.toml` instead, so a bare `ferromesh tui` finds your server:

```toml
server = "truffles.local"
token = "..."
```

## Watching traffic

```sh
ferromesh tail                                # the last 20 channel messages, then live
ferromesh tail chan:#test --last 50
ferromesh tail --kind observations 'snr>-5'   # every reception, with signal and path
ferromesh tail --live --json | jq .           # new traffic only, one JSON event per line

ferromesh query from:BNA* --since 6h          # history, oldest first, then exit
ferromesh query --kind packets type:advert --json
ferromesh query --since 2d --until 1d
```

`--kind` chooses what an event is: `messages` (decoded channel messages, each once), `packets` (every distinct packet, decoded where possible) or `observations` (every reception by every observer, with signal and path).

`tail` reconnects by itself and resumes after the last event it printed, so a dropped connection or a server restart neither loses nor repeats anything the server stored.

## The filter language

The same terms work in `tail`, `query`, the TUI's filter bar, and watches. Terms are space-separated and must all match:

| Term | Matches |
|---|---|
| `chan:#test,#wx` | either channel |
| `from:BNA*` | sender name, `*` wildcard |
| `storm` | a word in the message text |
| `text:"storm warning"` | a phrase |
| `type:advert` | packet type |
| `node:4d1727` | a hex prefix of an advert's public key |
| `observer:Tanyard` | which radio heard it |
| `'snr>-5'` `'rssi<-100'` `'hops>2'` | observations by signal or path length |

A leading `-` negates a term: `-type:advert`. Quote `>` and `<` so the shell leaves them alone, and put double quotes around values with spaces: `'from:"BNA Bot"'`. One quoted argument can hold a whole filter: `'type:advert snr>-5'`.

## Direct messages and sending

Both need a companion radio on the server; see [companion.md](companion.md).

```sh
ferromesh dms                                 # direct messages sent to your radio

export FERROMESH_TOKEN=...
ferromesh send '#test' hello from the terminal
ferromesh send KK4SW are you there?           # by name, or a hex prefix of a key
ferromesh send '#test' quick --follow 0       # don't wait around
```

`send` follows the message for 20 seconds by default. For a channel message it shows who heard it: ferromesh knows the exact packet the radio will transmit, so every observer's reception of it counts, including your repeater hearing your own radio. For a direct message it shows whether the recipient acknowledged it, and how long the round trip took. Either way the send is kept in the outbox (`GET /api/v1/outbox`).

A channel must be added before you can send to it, and a node must have been heard advertising before you can address it.

## Advertising your radio

Other nodes can only message a radio they have as a contact, and they add one
by hearing it advertise. After a new radio, a rename, or a move:

```sh
ferromesh advert                  # to the radios that hear it directly
ferromesh advert --flood          # across the whole mesh
ferromesh advert --follow 0       # don't wait to see who heard it
```

Afterwards it watches for the advert coming back through the observers and
prints who heard it, at how many hops and at what signal, for 20 seconds by
default.

A flood advert is relayed by every node in range of every hop, so it costs the
whole mesh airtime. Use it when you want to be reachable from far away, and the
plain one otherwise. Two floods in a row are almost always a mistake, so the
server refuses a second within a minute of the first.

### Naming someone in a message

MeshCore carries a mention as plain text in the message body: `@[their name]`, brackets included, which is how a name with a space in it stays in one piece. Other clients highlight it and strip the brackets when they display it, and so does ferromesh — you'll see `@Bob` in the feed, and `@[Bob]` only if you look at the raw packet.

`r` writes the brackets for you. There's no protocol field behind any of this: it's a convention other clients settled on rather than something MeshCore documents, so it's worth keeping to the exact form if you type one by hand.

## Health

```sh
ferromesh health                              # each observer, over the last 24 hours
ferromesh health --hours 168                  # a week
ferromesh health --json
```

Every observer — your repeater over MQTT, and your companion radio — reports its state every few minutes. `health` turns those reports into battery, noise floor, packets received and sent, receive errors, airtime and restarts, each with an hourly trend.

It also checks delivery: the packets an observer counted receiving, against the receptions actually stored from it. Anything lost between the radio and the database shows up as less than 100%.

Warnings flag an overdue report, a restart, a low battery, a noise floor well above that observer's usual level, and a delivery shortfall — below 99% for an observer publishing over MQTT, or below 95% for a companion radio, which can never report packets over 173 bytes (see [companion.md](companion.md#limits-worth-knowing)).

## Channels

Everything is stored, including channel traffic nobody can read yet, so adding a channel later decodes its history too:

```sh
ferromesh channels                            # what the server decrypts
ferromesh channels unknown                    # channel hashes on traffic no known key opens
ferromesh channels guess chattanooga tn-east  # try hashtag names, plus common and mentioned ones
ferromesh channels guess --add                # add everything it identifies

export FERROMESH_TOKEN=...
ferromesh channels add '#chattanooga'         # add it, and decrypt what was waiting
ferromesh channels add 'My Group' --key SECRET   # hex, as in a MeshCore QR code, or base64
```

Hashtag channels derive their key from the name, which is why guessing works; private channels need their key. A guess only counts when the key both passes the packet's MAC and decrypts to readable text.

## The terminal UI

```sh
ferromesh tui
```

Eight views, switched with `1` to `8`:

| | |
|---|---|
| **1 Messages** | A channel list with unread counts, and each message once with how many times it was heard |
| **2 DMs** | Direct messages, as a conversation per person: what they sent, what you sent, and whether it was acknowledged. `n` starts one with anyone |
| **3 Packets** | Every distinct packet, decoded where possible |
| **4 RF** | Every reception, with signal strength and path, naming repeaters where a hop prefix identifies one |
| **5 Nodes** | Every node that has advertised |
| **6 Alerts** | Your watches, and new traffic that matched them |
| **7 Health** | Each observer's battery, noise floor, traffic and delivery, with trends and warnings |
| **8 Contacts** | The companion radio's own contact list, favourites first, marking the one it would replace next |

Keys:

- `c` composes: a message to the selected channel, a reply in the conversation the DM view is showing, or a message to the selected contact (needs the token).
- `Tab` moves between the channel list and the feed, or the people list and the conversation in DMs.
- `p` keeps a contact on the radio, or lets it go again; in the Nodes view it adds the selected node as a kept contact (needs the token).
- `n` in the DM view starts a conversation with anyone, by name or the start of their key; `m` in the Nodes, Contacts or Messages view does the same for whoever is selected — in Messages, that's the sender of the selected message.
- `r` replies to the selected channel message, on the channel it arrived on and naming its sender.
- `a` advertises the radio: `l` to its neighbours, `f` across the mesh (needs the token).
- `a` advertises the radio, then `l` for its neighbours or `f` for the whole mesh (needs the token).
- `Enter` opens the inspector on the selected packet: each reception's signal and path, and the frame's bytes labelled field by field.
- `/` filters the current view, with the filter language above.
- `w` saves the current filter as a watch. Matching traffic is highlighted, and new matches ring the bell and land in Alerts.
- Scrolling past the oldest row loads older history from the server.
- `?` lists every key.

Watches are kept in `~/.config/ferromesh/watches.toml`.

For scripting or a screenshot, the TUI can render one screen as plain text and exit:

```sh
ferromesh tui --snapshot --size 120x40 --keys '4<enter>'
```
