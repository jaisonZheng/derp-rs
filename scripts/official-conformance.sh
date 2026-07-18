#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DERP_PORT="${DERP_PORT:-33340}"
STUN_PORT="${STUN_PORT:-33478}"
DERP_URL="${DERP_URL:-http://127.0.0.1:${DERP_PORT}}"
DERP_STUN_ADDR="${DERP_STUN_ADDR:-127.0.0.1:${STUN_PORT}}"
DERP_MESH_PSK="${DERP_MESH_PSK:-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef}"
WORK_DIR="${ROOT}/work/official-conformance"
KEY_FILE="${WORK_DIR}/derper.key"
MESH_FILE="${WORK_DIR}/mesh.key"
LOG_FILE="${WORK_DIR}/server.log"

mkdir -p "${WORK_DIR}"
printf '%s\n' "${DERP_MESH_PSK}" > "${MESH_FILE}"

cargo build --manifest-path "${ROOT}/Cargo.toml" --release --locked
"${ROOT}/target/release/derper-rs" \
  --addr "127.0.0.1:${DERP_PORT}" \
  --stun-addr "${DERP_STUN_ADDR}" \
  --private-key "${KEY_FILE}" \
  --mesh-psk-file "${MESH_FILE}" \
  --shutdown-grace 10ms \
  >"${LOG_FILE}" 2>&1 &
SERVER_PID=$!

cleanup() {
  kill -TERM "${SERVER_PID}" 2>/dev/null || true
  wait "${SERVER_PID}" 2>/dev/null || true
}
trap cleanup EXIT

for _ in $(seq 1 100); do
  if curl --fail --silent "${DERP_URL}/derp/probe" >/dev/null; then
    break
  fi
  if ! kill -0 "${SERVER_PID}" 2>/dev/null; then
    sed -n '1,200p' "${LOG_FILE}" >&2
    exit 1
  fi
  sleep 0.05
done
curl --fail --silent "${DERP_URL}/derp/probe" >/dev/null

(
  cd "${ROOT}/bench"
  DERP_URL="${DERP_URL}" \
  DERP_STUN_ADDR="${DERP_STUN_ADDR}" \
  DERP_MESH_PSK="${DERP_MESH_PSK}" \
    go test ./conformance -count=1 -v
)
