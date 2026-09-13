#!/usr/bin/env python3
"""Build the meshcore-proto golden fixture from a meshcoretomqtt capture.

    tools/gen_fixture.py --capture ../mqtt_observer/meshcore_packets.jsonl \
        --decoder ../mqtt_observer --lines 9193

Expected decodes come from ../mqtt_observer/meshcore_decode.py. That decoder
is an independent implementation: it finds the payload from the MQTT
payload_len field instead of parsing the path, so agreement with the Rust
parser is meaningful. Never feed Rust output back into this file.

Writes, under crates/meshcore-proto/tests/fixtures/:
    capture.jsonl   one line per received packet, with the Python decode
    channels.json   the channel list used (hashtag names only)
    summary.json    counts the Rust test asserts against
"""

import argparse
import json
import os
import sys
from collections import Counter

FIXTURES = "crates/meshcore-proto/tests/fixtures"
ADVERT_FIELDS = ("pubkey", "adv_timestamp", "flags", "lat", "lon", "name")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--capture", required=True)
    ap.add_argument("--decoder", required=True,
                    help="directory containing meshcore_decode.py")
    ap.add_argument("--channels",
                    help="channel list JSON (default: <decoder>/channels.json)")
    ap.add_argument("--lines", type=int,
                    help="read only the first N lines; the capture may still "
                         "be growing, so pin this for a reproducible fixture")
    ap.add_argument("--out", default=FIXTURES)
    args = ap.parse_args()

    sys.path.insert(0, args.decoder)
    from meshcore_decode import (  # pyright: ignore[reportMissingImports]
        decode_advert, decrypt_channel_msg, load_channels)

    chan_path = args.channels or os.path.join(args.decoder, "channels.json")
    with open(chan_path) as fh:
        entries = json.load(fh)
    # Hashtag names are derivable by anyone; an explicit key is a real secret
    # and must not end up committed alongside the tests.
    if any("key" in e for e in entries):
        sys.exit(f"{chan_path} has explicit channel keys; refusing to copy "
                 f"secrets into fixtures")
    channels = load_channels(chan_path)

    os.makedirs(args.out, exist_ok=True)
    stats = Counter()
    unique = set()
    with open(args.capture) as src, \
            open(os.path.join(args.out, "capture.jsonl"), "w") as dst:
        for n, line in enumerate(src):
            if args.lines is not None and n >= args.lines:
                break
            pkt = json.loads(line)
            if pkt.get("type") != "PACKET":
                continue
            ptype = int(pkt["packet_type"])
            pl = int(pkt["payload_len"])
            rec = {
                "hash": pkt["hash"],
                "len": int(pkt["len"]),
                "packet_type": ptype,
                "route": pkt["route"],
                "payload_len": pl,
                "raw": pkt["raw"],
            }
            stats["packets"] += 1

            raw = bytes.fromhex(pkt["raw"])
            payload = raw[-pl:] if 0 < pl <= len(raw) else raw
            if ptype == 4:
                info = decode_advert(pkt["raw"], pl)
                rec["advert"] = info and {k: info[k] for k in ADVERT_FIELDS}
                stats["adverts" if info else "adverts_malformed"] += 1
            elif ptype == 5:
                msg = decrypt_channel_msg(payload, channels)
                rec["grp"] = msg
                if msg:
                    stats["grp_decrypted"] += 1
                    unique.add((msg["channel"], msg["sender_timestamp"],
                                msg["text"]))
                else:
                    stats["grp_undecrypted"] += 1

            dst.write(json.dumps(rec, separators=(",", ":"),
                                 ensure_ascii=False) + "\n")

    with open(os.path.join(args.out, "channels.json"), "w") as fh:
        json.dump(entries, fh, indent=2)
        fh.write("\n")

    stats["unique_messages"] = len(unique)
    summary = dict(sorted(stats.items()))
    with open(os.path.join(args.out, "summary.json"), "w") as fh:
        json.dump(summary, fh, indent=2)
        fh.write("\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
