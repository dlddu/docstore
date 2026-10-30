#!/usr/bin/env bash
# Usage: scripts/smoke.sh <image>
set -euo pipefail
IMAGE="$1"
NAME=docstore-smoke
VOL=docstore-smoke-data
PORT=19200
BASE="http://127.0.0.1:${PORT}"

cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; docker volume rm "$VOL" >/dev/null 2>&1 || true; }
trap cleanup EXIT
cleanup
docker volume create "$VOL" >/dev/null
# The image runs as nonroot; give the volume to that uid like an EFS access point would.
docker run --rm -v "$VOL:/data" busybox chown 65532:65532 /data

start() {
  docker run -d --name "$NAME" -p "${PORT}:9200" -v "$VOL:/data" "$IMAGE" >/dev/null
  for _ in $(seq 1 50); do
    curl -fsS "$BASE/healthz" >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  docker logs "$NAME"; echo "server did not become healthy"; exit 1
}

expect() { # expect <expected-status> <method> <path> [json-body]
  local want="$1" method="$2" path="$3" body="${4:-}"
  local args=(-sS -o /tmp/smoke-body -w '%{http_code}' -X "$method" "$BASE$path")
  [ -n "$body" ] && args+=(-H 'Content-Type: application/json' -d "$body")
  local got; got=$(curl "${args[@]}")
  echo "$method $path -> $got $(head -c 300 /tmp/smoke-body)"
  [ "$got" = "$want" ] || { echo "expected $want"; docker logs "$NAME"; exit 1; }
}

start
expect 201 PUT  /smoke/_doc/one '{"title":"hello smoke","tags":["ci"]}'
expect 200 PUT  /smoke/_doc/one '{"title":"hello again smoke","tags":["ci"]}'
expect 201 POST /smoke/_doc     '{"title":"auto id"}'
expect 200 POST /smoke/_search  '{"size":10,"query":{"simple_query_string":{"query":"smoke"}}}'
grep -q '"_id":"one"' /tmp/smoke-body

docker restart "$NAME" >/dev/null
for _ in $(seq 1 50); do curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break; sleep 0.2; done

expect 200 POST /_search '{"size":10,"query":{"simple_query_string":{"query":"again"}}}'
grep -q '"value":1' /tmp/smoke-body
expect 200 DELETE /smoke/_doc/one
expect 404 DELETE /smoke/_doc/one
expect 200 GET /smoke/_count
grep -q '"count":1' /tmp/smoke-body
echo "smoke test passed"
