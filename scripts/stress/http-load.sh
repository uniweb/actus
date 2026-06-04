#!/usr/bin/env bash
# HTTP load stress test for actus.
#
# Builds examples/basic in release mode, runs it on port 3000, then
# pummels /health and /api/users with `ab` (Apache Bench). Reports
# req/s, latency percentiles, and before-vs-after RSS — so a leak under
# load shows up.
#
# Numbers here are not a benchmark to optimize against; they're a
# smoke check that the framework behaves sanely under sustained load.
# See scripts/stress/README.md for what to look for.

set -euo pipefail
cd "$(dirname "$0")/../.."

REQUESTS="${REQUESTS:-50000}"
CONCURRENCY="${CONCURRENCY:-200}"
PORT="${PORT:-3000}"
BASE="http://127.0.0.1:${PORT}"

command -v ab >/dev/null || {
  echo "error: 'ab' (Apache Bench) not found in PATH"
  echo "  macOS: built in at /usr/sbin/ab"
  echo "  Debian/Ubuntu: apt install apache2-utils"
  echo "  Fedora/RHEL:   dnf install httpd-tools"
  exit 1
}

echo "Building examples/basic (release)..."
cargo build -p actus-basic-example --release --quiet 2>&1 | tail -5

# Make sure the port is free.
if lsof -ti :"$PORT" >/dev/null 2>&1; then
  echo "warn: port $PORT is in use; killing existing listener"
  lsof -ti :"$PORT" | xargs -r kill 2>/dev/null || true
  sleep 1
fi

echo ""
echo "Starting actus-basic-example on :$PORT ..."
RUST_LOG=warn ./target/release/actus-basic-example >/tmp/actus-stress.log 2>&1 &
SERVER_PID=$!
trap "kill $SERVER_PID 2>/dev/null || true" EXIT

# Wait for the port to come up.
for i in $(seq 1 50); do
  if curl -fs "${BASE}/health" >/dev/null 2>&1; then break; fi
  sleep 0.1
done
if ! curl -fs "${BASE}/health" >/dev/null 2>&1; then
  echo "error: server didn't come up on :$PORT"
  exit 1
fi

# Helper: RSS in MB for our PID.
rss_mb() {
  ps -o rss= -p "$SERVER_PID" 2>/dev/null | awk '{printf "%.1f", $1/1024}'
}

echo "Server PID: $SERVER_PID, baseline RSS: $(rss_mb) MB"
echo ""

run_ab() {
  local url="$1" label="$2"
  echo "=== $label ==="
  ab -q -n "$REQUESTS" -c "$CONCURRENCY" -k "$url" \
    | grep -E "Requests per second|Time per request|Failed requests|Transfer rate|Percentage|99%|95%|^Document"
  echo ""
}

run_ab "${BASE}/health"        "GET /health (empty 200, smallest possible)"
echo "  RSS: $(rss_mb) MB"
echo ""

run_ab "${BASE}/api/users"     "GET /api/users (small JSON, 10 records)"
echo "  RSS: $(rss_mb) MB"
echo ""

run_ab "${BASE}/api/users?page=1&limit=200" "GET /api/users?limit=200 (larger JSON, 200 records)"
echo "  RSS: $(rss_mb) MB"
echo ""

# Send the same workload with Accept-Encoding so the compression path is
# exercised — the framework compresses responses ≥ 1 KiB on the
# `compression` feature, which is enabled in examples/basic.
echo "=== GET /api/users?limit=200 with Accept-Encoding: gzip,br ==="
ab -q -n "$REQUESTS" -c "$CONCURRENCY" -k -H "Accept-Encoding: gzip, br" "${BASE}/api/users?page=1&limit=200" \
  | grep -E "Requests per second|Time per request|Failed requests|Transfer rate|Percentage|99%|95%"
echo "  RSS: $(rss_mb) MB"
echo ""

echo "=== Final RSS ==="
echo "  $(rss_mb) MB"
echo ""
echo "Done. Server log at /tmp/actus-stress.log."
