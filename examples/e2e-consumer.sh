#!/usr/bin/env sh
# Test-only worker used by e2e-smoke.sh.
#
# Pulls one message at a time through the management API and acknowledges it,
# with a small fixed delay so each worker has a bounded, roughly known
# throughput. That is what lets the smoke test assert effiQueue actually
# *measures* mu rather than merely guessing it.
#
# Not a template for real workers — a real consumer holds an AMQP connection.
set -u

API="${EQ_API:-http://localhost:15672/api}"
QUEUE="${EQ_QUEUE:-messages_e2e}"
AUTH="${EQ_AUTH:-guest:guest}"
WORK_DELAY="${EQ_WORK_DELAY:-0.2}"

# Exit cleanly on SIGTERM so the graceful-drain path is exercised too.
trap 'exit 0' TERM INT

while :; do
  body=$(curl -s -u "$AUTH" -H 'content-type: application/json' \
    -X POST "$API/queues/%2f/$QUEUE/get" \
    -d '{"count":1,"ackmode":"ack_requeue_false","encoding":"auto"}' 2>/dev/null)
  case "$body" in
    *payload*) sleep "$WORK_DELAY" ;;  # got work; simulate processing it
    *)         sleep 0.5 ;;            # queue empty; idle politely
  esac
done
