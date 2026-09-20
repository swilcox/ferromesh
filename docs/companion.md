# The companion radio

A MeshCore radio flashed with the stock **companion** firmware and plugged into USB gives ferromesh its own place on the mesh: its own vantage point for receiving, and an identity to send from. It is also a complete source on its own, so ferromesh runs with no broker and no repeater at all.

## Setting one up

1. Flash the radio with companion firmware (USB serial) from the [MeshCore flasher](https://flasher.meshcore.io/). Any supported board works; a Heltec V3/V4 is the usual cheap choice.
2. Set its name and region with a MeshCore phone app or the flasher's console, then unpair the phone. **ferromeshd must be the radio's only client:** whichever client fetches a queued message takes it, so a phone left connected will eat your direct messages.
3. Plug it into the server and add to the config:

```toml
[companion]
device = "auto"   # the one Espressif USB device, or a path
```

`auto` looks for a USB device with Espressif's vendor ID (0x303A), falling back to `/dev/serial/by-id`. Give an explicit path if the machine has more than one such device.

In Docker, pass the port through with a `compose.override.yaml`; see [running.md](running.md#docker).

## What it does

**Receives.** Every packet the radio hears becomes an observation with its SNR and RSSI, stored beside the observations from any MQTT observers. In the packet inspector you can then see the same packet from both.

**Takes direct messages.** Messages addressed to the radio are decrypted on the radio — its private key never leaves it — and stored. `ferromesh dms` lists them, and the TUI shows them too.

**Sends.** `ferromesh send`, `POST /api/v1/send`, and `c` in the TUI all go out through it. Sending to a channel gives that channel one of the radio's 40 slots the first time. Sending to a node adds that node to the radio's contacts from its advert.

**Reports its health.** Battery, noise floor, airtime and packet counters every 5 minutes, which is what `ferromesh health` shows. If the radio stops hearing anything for a long stretch, ferromesh warns: that usually means it needs a reboot.

**Keeps its clock right.** MeshCore timestamps messages on the sending radio, so ferromesh sets the radio's clock whenever it's behind the server's.

It also reconnects on its own if the radio is unplugged and plugged back in.

## Contacts

The radio holds a few hundred contacts — 350 on a Heltec V4 — and a busy mesh has far more nodes than that. In six days ours saw 1,132, of which 967 were repeaters, which don't need to be contacts at all.

So ferromesh sets the radio's policy to add only **chat radios** as it hears them, and to replace the contact it heard from least recently once the table is full. Favourites are never replaced:

- Everyone you exchange direct messages with becomes a favourite automatically.
- `ferromesh contacts pin NAME` makes any other node one — a repeater you administer, a room server you post to.

```sh
ferromesh contacts             # what's on the radio; ★ marks pinned
ferromesh contacts pin Tanyard
ferromesh contacts unpin Tanyard
```

**Rescue.** A companion can only decrypt a direct message from a node it has as a contact. If a message arrives that the radio can't read, because it has never heard the sender or has since forgotten them, ferromesh adds every node in its own records that could have sent it, so the sender's automatic retry can be read. This is why a first message from a stranger usually lands on their second try.

## Limits worth knowing

- **One client only.** Don't pair a phone with the radio while ferromeshd is using it.
- **It hears what its antenna hears.** A repeater on a hill covers a region; a radio on a shelf behind a server covers a neighbourhood. A USB extension away from the machine can be worth several dB — computers are noisy at 900 MHz. Our first spot measured a −74 dBm noise floor against a hilltop repeater's −103 dBm.
- **The largest frames don't make it.** The firmware's receive log drops some big packets, so a companion stores around 99.4% of what it counts hearing, against 100.0% from an observer repeater. `ferromesh health` shows the difference.
- **Channel messages are decoded twice.** The radio decrypts channels in its own slots, but ferromesh ignores those copies and decodes the raw receptions with the server's own channel keys instead — so the server's channel list, not the radio's slots, decides what you can read.

## Troubleshooting

**The radio hears nothing at all.** A freshly flashed radio sometimes comes up deaf; reboot it. ferromesh warns after a long silence, so this shows up in the log.

**The port disappeared after a replug.** On Linux the radio can come back as a different `/dev/ttyACM*`. Use `/dev/serial/by-id/...`, which is stable, or update the Docker device mapping.

**Permission denied on the port.** The account running ferromeshd needs the group that owns the device — `dialout` on most Linux distributions. Under Docker that's what `group_add` is for.

**Talking to the radio from your own scripts.** Anything else holding the port will fight ferromeshd for messages, so stop the server first. On macOS, open the port with `CLOCAL` set, or the read will block waiting for a carrier signal that never comes.
