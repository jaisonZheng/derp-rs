#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RUST_ADDR="127.0.0.1:3340"
GO_ADDR="127.0.0.1:3341"

cd "$ROOT"
cargo build --release --locked
GOBIN="$ROOT/work" go install tailscale.com/cmd/derper@v1.100.0
(cd bench && go build -o ../work/derp-bench .)

run_one() {
  local name="$1" addr="$2"
  shift 2
  "$@" >"work/${name}.log" 2>&1 &
  local pid=$!
  trap 'kill "$pid" 2>/dev/null || true' RETURN
  local ready=0
  for _ in $(seq 1 100); do
    if curl -fsS "http://${addr}/derp/probe" >/dev/null 2>&1; then
      ready=1
      break
    fi
    sleep 0.05
  done
  if [[ "$ready" -ne 1 ]]; then
    tail -n 20 "work/${name}.log"
    return 1
  fi
  for trial in $(seq 1 5); do
    local result
    result="$(work/derp-bench -addr "$addr" -clients 16 -rounds 1000 -batch 16 -size 1200)"
    printf '%s trial %s\t%s\n' "$name" "$trial" "$result"
  done
  local rss
  rss="$(ps -o rss= -p "$pid" | tr -d ' ')"
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  printf '%s rss_kib\t%s\n' "$name" "$rss"
  trap - RETURN
}

run_one rust "$RUST_ADDR" target/release/derper-rs --addr "$RUST_ADDR" --stun-addr off --private-key work/bench-rust.key --shutdown-grace 1s
run_one go "$GO_ADDR" work/derper -a "$GO_ADDR" -stun=false -http-port=-1 -hostname=localhost -c work/bench-go.key
