#!/usr/bin/env bash
# Graceful-shutdown stress for actus.
#
# Starts examples/basic, generates sustained HTTP load with `ab`,
# sends SIGTERM mid-stream, and measures how long the server takes to
# exit. Reports total elapsed time and `ab`'s failed-request count.
# A clean drain means: server exits well under the drain deadline,
# zero failed requests (the in-flight ones finished cleanly).

set -euo pipefail
cd "$(dirname "$0")/../.."

PORT="${PORT:-3000}"
BASE="http://127.0.0.1:${PORT}"
DURATION_SECS="${DURATION_SECS:-10}"
CONCURRENCY="${CONCURRENCY:-100}"
SIGNAL_AT_SECS="${SIGNAL_AT_SECS:-5}"

command -v ab >/dev/null || {
  echo "error: 'ab' not found; see scripts/stress/README.md"
  exit 1
}

echo "Building examples/basic (release)..."
cargo build -p actus-basic-example --release --quiet 2>&1 | tail -5

# Free the port.
if lsof -ti :"$PORT" >/dev/null 2>&1; then
  lsof -ti :"$PORT" | xargs -r kill 2>/dev/null || true
  sleep 1
fi

echo "Starting actus-basic-example on :$PORT ..."
RUST_LOG=warn ./target/release/actus-basic-example >/tmp/actus-drain.log 2>&1 &
SERVER_PID=$!
# Pin a hard kill if the test goes sideways.
trap "kill -KILL $SERVER_PID 2>/dev/null || true" EXIT

for i in $(seq 1 50); do
  if curl -fs "${BASE}/health" >/dev/null 2>&1; then break; fi
  sleep 0.1
done

echo ""
echo "Driving load: ab -t ${DURATION_SECS}s -c ${CONCURRENCY} keep-alive ${BASE}/health"

# `ab -t` implicitly caps requests at 50000; on a fast server that's
# done in well under a second. Override with a high `-n` so ab keeps
# pushing for the full duration window — that way the SIGTERM lands
# while requests are still in-flight, which is the case we want to
# exercise.
ab -q -t "$DURATION_SECS" -n 100000000 -c "$CONCURRENCY" -k "${BASE}/health" \
  > /tmp/actus-drain-ab.txt 2>&1 &
AB_PID=$!

# Wait, then SIGTERM the server while load is still going.
sleep "$SIGNAL_AT_SECS"
echo ""
echo "Sending SIGTERM to server (PID $SERVER_PID) at t=${SIGNAL_AT_SECS}s ..."
START_NS=$(perl -MTime::HiRes -e 'printf "%.0f\n", Time::HiRes::time()*1e9')
kill -TERM "$SERVER_PID"

# Wait for the server to actually exit.
wait "$SERVER_PID" 2>/dev/null || true
END_NS=$(perl -MTime::HiRes -e 'printf "%.0f\n", Time::HiRes::time()*1e9')
ELAPSED_MS=$(( (END_NS - START_NS) / 1000000 ))
echo "Server exited ${ELAPSED_MS} ms after SIGTERM."

# Let ab wrap up if it hasn't yet.
wait "$AB_PID" 2>/dev/null || true

echo ""
echo "=== ab summary ==="
# ab takes one of two exit paths:
#   (a) Hits its `-n` cap or `-t` time → prints full summary with
#       "Requests per second", "Failed requests", percentile table.
#   (b) Connection gets reset when the server exits mid-stream → bails
#       out without the summary, but prints "Total of N requests
#       completed" on the way out.
# Show whichever signal is present.
if grep -q "Requests per second" /tmp/actus-drain-ab.txt; then
    grep -E "Requests per second|Time per request|Failed requests|Non-2xx|Complete requests|Percentage|99%" \
        /tmp/actus-drain-ab.txt | head -20
else
    grep -E "Total of [0-9]+ requests completed|apr_socket_recv" /tmp/actus-drain-ab.txt | head -5
fi

echo ""
echo "=== assessment ==="
# Use `|| true` everywhere — `pipefail` plus a grep that finds nothing
# would otherwise kill the script before printing the assessment.
COMPLETED=$(grep -E "Complete requests|Total of [0-9]+" /tmp/actus-drain-ab.txt 2>/dev/null \
    | head -1 | grep -oE '[0-9]+' | head -1 || true)
FAILED=$(grep "Failed requests" /tmp/actus-drain-ab.txt 2>/dev/null \
    | head -1 | awk '{print $NF}' || true)
echo "requests completed before SIGTERM: ${COMPLETED:-?}"
echo "ab 'Failed requests' (mid-stream):  ${FAILED:-(not reported; ab bailed)}"
echo "drain elapsed (SIGTERM → exit):     ${ELAPSED_MS} ms"
echo ""
echo "Notes:"
echo "  * Default drain deadline is 30000 ms (Server::with_drain_deadline)."
echo "    A clean drain finishes well under that, with the elapsed time"
echo "    bounded by the time hyper needs to flush in-flight responses."
echo "  * 'Connection reset by peer' in ab is expected on the few"
echo "    in-flight connections still mid-pipeline when the server exits"
echo "    — graceful_shutdown finishes the *current* request on each"
echo "    connection, then closes. A pipelined next-request from ab"
echo "    after that close is what ab counts as reset."
