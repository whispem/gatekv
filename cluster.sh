#!/bin/sh
# Runs a 3-node cluster on 127.0.0.1:7001-7003 with data under ./data; Ctrl-C stops it.
set -eu
cargo build --release
set -- 127.0.0.1:7001 127.0.0.1:7002 127.0.0.1:7003
pids=""
for id in 0 1 2; do
    target/release/gatekv "$id" "data/$id" "$@" &
    pids="$pids $!"
done
trap 'kill $pids 2>/dev/null' INT TERM
wait
