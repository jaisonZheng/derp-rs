#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ADDR="${ADDR:-127.0.0.1:3342}"
DURATION="${DURATION:-15s}"
SERVERS="${SERVERS:-rust go}"
SCENARIOS="${SCENARIOS:-idle:100 idle:1000 idle:5000 active:100 active:1000 slow:100 slow:1000}"

cd "$ROOT"
mkdir -p work
ulimit -n 65535 2>/dev/null || true

if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  cargo build --release --locked
  GOBIN="$ROOT/work" go install tailscale.com/cmd/derper@v1.100.0
  (cd bench && go build -o ../work/derp-rss ./rss)
fi

rss_kib() {
  local pid="$1"
  if [[ -r "/proc/${pid}/status" ]]; then
    awk '/^VmRSS:/{print $2}' "/proc/${pid}/status"
  else
    ps -o rss= -p "$pid" | tr -d ' '
  fi
}

average_rss() {
  local pid="$1" samples="${2:-10}" total=0 count=0 value
  for _ in $(seq 1 "$samples"); do
    value="$(rss_kib "$pid")"
    if [[ -n "$value" ]]; then
      total=$((total + value))
      count=$((count + 1))
    fi
    sleep 0.1
  done
  echo $((total / count))
}

wait_ready() {
  local pid="$1" url="$2"
  for _ in $(seq 1 300); do
    if curl -fsS "$url" >/dev/null 2>&1; then
      return 0
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
      return 1
    fi
    sleep 0.1
  done
  return 1
}

run_case() {
  local server_name="$1" mode="$2" clients="$3"
  local tag="${server_name}-${mode}-${clients}"
  local server_log="work/rss-${tag}-server.log"
  local client_log="work/rss-${tag}-client.log"
  local peak_file="work/rss-${tag}-peak"
  local cmd
  case "$server_name" in
    rust)
      cmd=(target/release/derper-rs --addr "$ADDR" --stun-addr off --private-key work/rss-rust.key --shutdown-grace 1ms)
      ;;
    go)
      cmd=(work/derper -a "$ADDR" -stun=false -http-port=-1 -hostname=localhost -c work/rss-go.key)
      ;;
    *)
      echo "unknown server: $server_name" >&2
      return 1
      ;;
  esac

  "${cmd[@]}" >"$server_log" 2>&1 &
  local server_pid=$!
  if ! wait_ready "$server_pid" "http://${ADDR}/derp/probe"; then
    tail -n 30 "$server_log" >&2
    return 1
  fi
  local baseline
  baseline="$(average_rss "$server_pid")"

  work/derp-rss \
    -addr "$ADDR" \
    -clients "$clients" \
    -mode "$mode" \
    -duration "$DURATION" \
    >"$client_log" 2>&1 &
  local client_pid=$!

  (
    local peak=0 value
    while kill -0 "$client_pid" 2>/dev/null; do
      value="$(rss_kib "$server_pid")"
      if [[ -n "$value" && "$value" -gt "$peak" ]]; then
        peak="$value"
      fi
      sleep 0.05
    done
    echo "$peak" >"$peak_file"
  ) &
  local sampler_pid=$!

  local ready=0
  for _ in $(seq 1 3000); do
    if grep -q '^READY ' "$client_log"; then
      ready=1
      break
    fi
    if ! kill -0 "$client_pid" 2>/dev/null; then
      break
    fi
    sleep 0.1
  done
  if [[ "$ready" -ne 1 ]]; then
    cat "$client_log" >&2
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
    return 1
  fi

  local steady
  steady="$(average_rss "$server_pid" 20)"
  wait "$client_pid"
  wait "$sampler_pid"
  local peak
  peak="$(cat "$peak_file")"
  local per_connection
  per_connection="$(
    awk -v steady="$steady" -v baseline="$baseline" -v clients="$clients" \
      'BEGIN { printf "%.1f", (steady-baseline)*1024/clients }'
  )"
  local result
  result="$(tail -n 1 "$client_log")"

  kill "$server_pid" 2>/dev/null || true
  wait "$server_pid" 2>/dev/null || true
  printf '%s,%s,%s,%s,%s,%s,%s,%s\n' \
    "$server_name" "$mode" "$clients" "$baseline" "$steady" "$peak" "$per_connection" "$result"
}

echo 'server,mode,clients,baseline_rss_kib,steady_rss_kib,peak_rss_kib,incremental_bytes_per_connection,client_result'
for scenario in $SCENARIOS; do
  IFS=: read -r mode clients <<<"$scenario"
  for server_name in $SERVERS; do
    run_case "$server_name" "$mode" "$clients"
  done
done
