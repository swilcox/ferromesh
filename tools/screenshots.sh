#!/bin/sh
# Renders the README's TUI screenshots from made-up traffic, never a real
# capture: crates/ferromeshd/examples/demo_capture.rs invents the mesh.
#
#     tools/screenshots.sh        # writes docs/screenshots/*.svg
set -eu
cd "$(dirname "$0")/.."
work=$(mktemp -d)
port=7479
trap 'kill "$server" 2>/dev/null; rm -rf "$work"' EXIT

cat > "$work/demo.toml" <<TOML
data_dir = "$work/data"
[api]
listen = "127.0.0.1:$port"
# Never reached: the traffic comes from the import below.
[mqtt]
host = "127.0.0.1"
port = 1
client_id = "demo"
topics = []
[[channel]]
name = "#chat"
[[channel]]
name = "#test"
[[channel]]
name = "#weather"
[[channel]]
name = "#emcomm"
TOML

cargo build -q --release -p ferromeshd -p ferromesh
cargo run -q --release -p ferromeshd --example demo_capture > "$work/demo.jsonl"
./target/release/ferromeshd -c "$work/demo.toml" import "$work/demo.jsonl" --label demo > /dev/null
./target/release/ferromeshd -c "$work/demo.toml" serve > "$work/serve.log" 2>&1 &
server=$!
sleep 1

mkdir -p docs/screenshots
shoot() { # name, keys, size
    FERROMESH_SERVER=127.0.0.1:$port ./target/release/ferromesh tui --snapshot --svg \
        --keys "$2" --size "$3" > "docs/screenshots/$1.svg"
}
shoot messages 1 120x36
shoot rf "4<tab>" 120x36
shoot health 5 120x25
