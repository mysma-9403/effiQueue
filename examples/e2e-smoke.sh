#!/usr/bin/env bash
# End-to-end smoke test against a real RabbitMQ.
#
#   MODE=threshold  scale UP on a backlog, DOWN to zero once drained
#   MODE=slo        additionally assert that mu is actually MEASURED and that
#                   workers_needed becomes a real number
#
# The slo assertions are the regression guard for the estimator: the previous
# residual estimator never initialised mu on a growing or steady backlog, so
# workers_needed stayed at the -1 sentinel forever and this test would fail.
#
# Requires curl. Starts RabbitMQ in Docker unless EQ_EXTERNAL_RABBIT=1 (CI
# provides it as a service container).
#
# Ports are overridable, because assuming ownership of 5672 collides with any
# broker a developer already has running — and pointing the test at *that* broker
# would have it create and purge queues in a live environment:
#
#   EQ_AMQP_PORT=5673 EQ_MGMT_PORT=15673 EQ_METRICS_PORT=9111 ./examples/e2e-smoke.sh
set -euo pipefail

MODE="${MODE:-threshold}"
IMG=rabbitmq:3-management
CT=eq-e2e-rabbit
AMQP_PORT="${EQ_AMQP_PORT:-5672}"
MGMT_PORT="${EQ_MGMT_PORT:-15672}"
METRICS_PORT="${EQ_METRICS_PORT:-9110}"
API="${EQ_API:-http://localhost:$MGMT_PORT/api}"
Q=messages_e2e
METRICS="http://127.0.0.1:$METRICS_PORT/metrics"
HERE="$(cd "$(dirname "$0")" && pwd)"
CFG="$HERE/config.e2e.$MODE.toml"
BIN="${EQ_BIN:-./target/debug/effiqueue}"
EXTERNAL="${EQ_EXTERNAL_RABBIT:-0}"

[ -f "$CFG" ] || { echo "no config for MODE=$MODE ($CFG)"; exit 2; }

# The shipped configs stay readable as documentation (default ports, no clutter);
# the ports actually used are substituted into a throwaway copy.
RENDERED="$(mktemp -t effiqueue-e2e-XXXXXX).toml"
sed -e "s|localhost:5672|localhost:$AMQP_PORT|g" \
    -e "s|127.0.0.1:9110|127.0.0.1:$METRICS_PORT|g" "$CFG" > "$RENDERED"
# A non-default management port cannot be derived from the AMQP URI, which only
# ever implies 15672.
if [ "$MGMT_PORT" != 15672 ]; then
  echo "management_url = \"http://guest:guest@localhost:$MGMT_PORT\"" >> "$RENDERED"
fi

EQ_PID=""
cleanup() {
  [ -n "$EQ_PID" ] && kill "$EQ_PID" 2>/dev/null || true
  rm -f "$RENDERED"
  [ "$EXTERNAL" = 1 ] || docker rm -f "$CT" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Read one metric value for the single program under test. Matched as a literal
# prefix, not a regex: "{" opens an interval expression in ERE, and the prefix
# also has to exclude longer names (effiqueue_workers vs effiqueue_workers_needed).
metric() { curl -s "$METRICS" | awk -v p="$1{" 'index($0, p) == 1 { print $2; exit }'; }

if [ "$EXTERNAL" != 1 ]; then
  echo "==> starting RabbitMQ ($IMG) on $AMQP_PORT/$MGMT_PORT"
  docker rm -f "$CT" >/dev/null 2>&1 || true
  docker run -d --rm --name "$CT" \
    -p "$AMQP_PORT:5672" -p "$MGMT_PORT:15672" "$IMG" >/dev/null
fi

echo "==> waiting for the management API"
ready=0
for _ in $(seq 1 60); do
  if curl -sf -u guest:guest "$API/overview" >/dev/null 2>&1; then ready=1; break; fi
  sleep 2
done
[ "$ready" = 1 ] || { echo "FAIL: management API never came up"; exit 1; }

echo "==> declaring queue '$Q' and publishing a backlog"
curl -sf -u guest:guest -XDELETE "$API/queues/%2f/$Q" >/dev/null 2>&1 || true
curl -sf -u guest:guest -H 'content-type: application/json' \
  -XPUT "$API/queues/%2f/$Q" -d '{"durable":true}' >/dev/null
for i in $(seq 1 300); do
  curl -sf -u guest:guest -H 'content-type: application/json' \
    -XPOST "$API/exchanges/%2f/amq.default/publish" \
    -d "{\"properties\":{},\"routing_key\":\"$Q\",\"payload\":\"job-$i\",\"payload_encoding\":\"string\"}" \
    >/dev/null
done

echo "==> building effiQueue"
cargo build -q

echo "==> starting effiQueue (mode=$MODE)"
chmod +x "$HERE/e2e-consumer.sh"
EQ_API="$API" EQ_QUEUE="$Q" RUST_LOG="${RUST_LOG:-info}" "$BIN" --config "$RENDERED" &
EQ_PID=$!

echo "==> expecting scale-up"
up=0
for _ in $(seq 1 40); do
  w="$(metric effiqueue_workers || echo 0)"
  echo "   workers=${w:-0} backlog=$(metric effiqueue_backlog) mu=$(metric effiqueue_mu) src=$(metric effiqueue_mu_source)"
  [ "${w:-0}" -ge 2 ] && { up=1; break; }
  sleep 2
done
[ "$up" = 1 ] || { echo "FAIL: did not scale up"; exit 1; }

if [ "$MODE" = slo ]; then
  echo "==> expecting mu to become MEASURED (mu_source != 0)"
  measured=0
  for _ in $(seq 1 45); do
    src="$(metric effiqueue_mu_source || echo 0)"
    mu="$(metric effiqueue_mu || echo 0)"
    needed="$(metric effiqueue_workers_needed || echo -1)"
    echo "   mu_source=$src mu=$mu workers_needed=$needed"
    if [ "${src%%.*}" != 0 ] && [ "${needed%%.*}" -ge 0 ]; then measured=1; break; fi
    sleep 2
  done
  if [ "$measured" != 1 ]; then
    echo "FAIL: mu was never measured; workers_needed stayed at the -1 sentinel."
    echo "      This is exactly the identifiability bug the estimator rewrite fixed."
    exit 1
  fi
  echo "==> mu measured and workers_needed derived"
fi

echo "==> purging the queue -> expecting scale-down to 0"
curl -sf -u guest:guest -XDELETE "$API/queues/%2f/$Q/contents" >/dev/null
down=0
for _ in $(seq 1 45); do
  w="$(metric effiqueue_workers || echo 1)"
  echo "   workers=${w:-1}"
  [ "${w:-1}" -eq 0 ] && { down=1; break; }
  sleep 2
done
[ "$down" = 1 ] || { echo "FAIL: did not scale down to zero"; exit 1; }

echo "==> PASS (mode=$MODE)"
