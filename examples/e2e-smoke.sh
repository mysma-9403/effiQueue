#!/usr/bin/env bash
# End-to-end smoke test: spins up RabbitMQ in Docker, publishes a backlog, runs
# effiQueue (threshold mode), and asserts it scales UP to max on the backlog and
# DOWN to zero once the queue is drained. Requires Docker + curl.
set -euo pipefail

IMG=rabbitmq:3-management
CT=eq-e2e-rabbit
API="http://localhost:15672/api"
Q=messages_e2e
METRICS="http://127.0.0.1:9110/metrics"
CFG="$(dirname "$0")/config.e2e.toml"
BIN=./target/debug/effiqueue

EQ_PID=""
cleanup() {
  [ -n "$EQ_PID" ] && kill "$EQ_PID" 2>/dev/null || true
  docker rm -f "$CT" >/dev/null 2>&1 || true
}
trap cleanup EXIT

workers() { curl -s "$METRICS" | awk '/^effiqueue_workers\{/{print $2; exit}'; }

echo "==> starting RabbitMQ ($IMG)"
docker rm -f "$CT" >/dev/null 2>&1 || true
docker run -d --rm --name "$CT" -p 5672:5672 -p 15672:15672 "$IMG" >/dev/null

echo "==> waiting for the management API"
for _ in $(seq 1 60); do
  curl -sf -u guest:guest "$API/overview" >/dev/null 2>&1 && break
  sleep 2
done

echo "==> declaring queue '$Q' and publishing 60 messages"
curl -sf -u guest:guest -H 'content-type: application/json' \
  -XPUT "$API/queues/%2f/$Q" -d '{"durable":true}' >/dev/null
for i in $(seq 1 60); do
  curl -sf -u guest:guest -H 'content-type: application/json' \
    -XPOST "$API/exchanges/%2f/amq.default/publish" \
    -d "{\"properties\":{},\"routing_key\":\"$Q\",\"payload\":\"job-$i\",\"payload_encoding\":\"string\"}" >/dev/null
done

echo "==> building effiQueue"
cargo build -q

echo "==> starting effiQueue (threshold mode)"
RUST_LOG=info "$BIN" --config "$CFG" &
EQ_PID=$!

echo "==> expecting scale-up to max_workers (3)"
up=0
for _ in $(seq 1 30); do
  w="$(workers || echo 0)"; echo "   workers=$w"
  [ "${w:-0}" -ge 3 ] && { up=1; break; }
  sleep 2
done
[ "$up" = 1 ] || { echo "FAIL: did not scale up"; exit 1; }

echo "==> purging the queue -> expecting scale-down to 0"
curl -sf -u guest:guest -XDELETE "$API/queues/%2f/$Q/contents" >/dev/null
down=0
for _ in $(seq 1 40); do
  w="$(workers || echo 1)"; echo "   workers=$w"
  [ "${w:-1}" -eq 0 ] && { down=1; break; }
  sleep 2
done
[ "$down" = 1 ] || { echo "FAIL: did not scale down"; exit 1; }

echo "==> PASS: scaled up on backlog, drained to zero on empty queue"
