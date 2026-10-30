#!/usr/bin/env bash
# Drives docstore over HTTP the way homelab-k3s-mcp does, including a restart
# in the middle to prove documents survive on the data volume.
#
# Usage:
#   scripts/smoke.sh <image>
#       Local: runs the image in docker with a named volume.
#   BASE=<url> RESTART='<cmd>' [LOGS='<cmd>'] scripts/smoke.sh
#       Against a server that is already running (CI: kind e2e in test.yml).
#       RESTART must restart the server while keeping its data volume;
#       LOGS prints server logs when an expectation fails.
set -euo pipefail

if [ -z "${BASE:-}" ]; then
  IMAGE="${1:?usage: scripts/smoke.sh <image>  |  BASE=<url> RESTART=<cmd> scripts/smoke.sh}"
  NAME=docstore-smoke
  VOL=docstore-smoke-data
  PORT=19200
  BASE="http://127.0.0.1:${PORT}"
  RESTART="docker restart $NAME >/dev/null"
  LOGS="docker logs $NAME"

  cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; docker volume rm "$VOL" >/dev/null 2>&1 || true; }
  trap cleanup EXIT
  cleanup
  docker volume create "$VOL" >/dev/null
  # The image runs as nonroot; give the volume to that uid like an EFS access point would.
  docker run --rm -v "$VOL:/data" busybox chown 65532:65532 /data
  docker run -d --name "$NAME" -p "${PORT}:9200" -v "$VOL:/data" "$IMAGE" >/dev/null
else
  : "${RESTART:?RESTART is required when BASE is set}"
  LOGS="${LOGS:-true}"
fi

logs() { bash -c "$LOGS" || true; }

wait_healthy() {
  for _ in $(seq 1 120); do
    curl -fsS "$BASE/healthz" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  logs; echo "server did not become healthy"; exit 1
}

expect() { # expect <expected-status> <method> <path> [json-body]
  local want="$1" method="$2" path="$3" body="${4:-}"
  local args=(-sS -o /tmp/smoke-body -w '%{http_code}' -X "$method" "$BASE$path")
  [ -n "$body" ] && args+=(-H 'Content-Type: application/json' -d "$body")
  local got; got=$(curl "${args[@]}")
  echo "$method $path -> $got $(head -c 300 /tmp/smoke-body)"
  [ "$got" = "$want" ] || { echo "expected $want"; logs; exit 1; }
}

wait_healthy
expect 201 PUT  /smoke/_doc/one '{"title":"hello smoke","tags":["ci"]}'
expect 200 PUT  /smoke/_doc/one '{"title":"hello again smoke","tags":["ci"]}'
expect 201 POST /smoke/_doc     '{"title":"auto id"}'
expect 200 POST /smoke/_search  '{"size":10,"query":{"simple_query_string":{"query":"smoke"}}}'
grep -q '"_id":"one"' /tmp/smoke-body

echo "restarting server"
bash -c "$RESTART"
wait_healthy

expect 200 POST /_search '{"size":10,"query":{"simple_query_string":{"query":"again"}}}'
grep -q '"value":1' /tmp/smoke-body
expect 200 DELETE /smoke/_doc/one
expect 404 DELETE /smoke/_doc/one
expect 200 GET /smoke/_count
grep -q '"count":1' /tmp/smoke-body
echo "smoke test passed"
