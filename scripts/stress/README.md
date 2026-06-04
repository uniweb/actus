# Stress scripts

These are runbooks, not unit tests. They exist to answer "does the
framework survive sustained adversarial load" — request/sec, p99
latency, memory growth, file-descriptor behavior, graceful-shutdown
under load. Run them when something feels slow, or before promoting a
release.

Nothing in here runs as part of `cargo test`; the scripts intentionally
take 30+ seconds each and are sensitive to the host's CPU / kernel
limits / what else is competing for the port.

## What's here

```
scripts/stress/
├── README.md
├── http-load.sh          ← ab against /health and /api/users
├── graceful-shutdown.sh  ← drives load, SIGTERMs, measures drain
└── ws-fanout/            ← Rust binary: N concurrent WebSockets
    ├── Cargo.toml        (separate workspace; doesn't affect actus)
    └── src/main.rs
```

## Requirements

- **`ab`** (Apache Bench) — built in on macOS at `/usr/sbin/ab`; on Linux
  it's `apt install apache2-utils` / `dnf install httpd-tools`.
- **`cargo`** — for the WS fanout binary. The crate has its own
  `[workspace]` declaration so it stays out of the actus workspace.
- **`curl`** — for readiness probing.

## How to run

```bash
# Spin up examples/basic in one terminal:
cargo run -p actus-basic-example --release

# In another terminal:
./scripts/stress/http-load.sh             # ~60 s, hits /health and /api/users
./scripts/stress/graceful-shutdown.sh     # ~15 s, drives load then SIGTERMs
cd scripts/stress/ws-fanout && cargo run --release -- --connections 2000 --duration 30
```

(`http-load.sh` and `graceful-shutdown.sh` build + start their own copy
of `examples/basic` in the background; they don't need a pre-running
instance.)

## What to look for

### `http-load.sh`
- **Throughput.** A handful of thousand req/s on a single core for
  `/health`; less for `/api/users` (small JSON serialize). Numbers
  here aren't a benchmark to optimize against — they're a smoke check
  that no obvious slowdown has crept in.
- **Latency p99**. Should be in the low-millisecond range under
  modest concurrency on localhost.
- **Failed requests = 0**. Any failure under sustained load points at
  something — connection leak, accept-loop stall, body-parse error.
- **RSS before vs after**. Should be stable. A 10× growth means a
  leak somewhere.

### `graceful-shutdown.sh`
- **Drain finishes in well under the configured deadline** (default 30 s
  in `examples/basic`). The script reports elapsed time from SIGTERM to
  process exit.
- **No requests fail during the drain**. `ab` reports `Failed
  requests` — should be 0 if the drain is correct.

### `ws-fanout/`
- **All connections open.** A `connect timeout` failure suggests the
  accept loop or backlog is too small for the requested concurrency.
- **Roundtrip rate scales with connection count**. Each connection
  ticks at ~10 messages/sec; with N connections you should see roughly
  10·N message round-trips per second.
- **File descriptors don't leak**. After the test, `lsof -p <pid>` on
  the server should drop back near baseline. (The script doesn't do
  this automatically — eyeball it.)

## Interpreting numbers

These tests run against `127.0.0.1` with no network in the picture, so
latency reflects pure framework + kernel overhead. They saturate one or
two cores at most. They are **not** a benchmark against axum / actix /
hyper — those would need careful matched-config setups, ideally on
isolated hardware. The point here is "does Actus behave sanely under
load," not "is Actus fast."

If you're trying to answer the comparative question, use a real
benchmarking harness like
[the techempower fortunes suite](https://github.com/TechEmpower/FrameworkBenchmarks).

## Sample results

Captured on M-series MacBook, `examples/basic` in release mode, all
three scripts on default settings unless noted. Numbers are for shape,
not for cross-machine comparison.

**`http-load.sh`** (20 000 req × 100 concurrency, keep-alive):

| Endpoint                          | req/s   | p99   | RSS after  | Failed |
|-----------------------------------|---------|-------|------------|--------|
| `GET /health`                     | 123 792 | 2 ms  | 10.7 MB    | 0      |
| `GET /api/users` (10 records)     | 110 564 | 2 ms  | 11.7 MB    | 0      |
| `GET /api/users?limit=200`        |  44 337 | 5 ms  | 15.8 MB    | 0      |
| `…limit=200` + brotli/gzip        |  34 024 | 6 ms  | 24.6 MB    | 0      |

The compression path is the slowest of the four — brotli on each
5.6 KiB body costs measurably more CPU than just shipping the bytes,
which is expected. RSS settles in the low tens of MB; no leak under
sustained load.

**`graceful-shutdown.sh`** (10 s window, SIGTERM at t=5s, 100
concurrent):

- 537 223 requests served in 5 s before SIGTERM (~107 k req/s sustained
  alongside the shutdown setup).
- Server exited **6 ms** after SIGTERM. The default 30 s drain deadline
  was nowhere near the limit; hyper finishes the current request on
  each connection and closes cleanly.

**`ws-fanout/`** (5 000 connections × 10 s × 200 ms ticks):

- 5000 / 5000 connections opened, 0 handshake failures.
- 245 000 round-trips delivered (~24 k round-trips/sec).
- Server RSS peaked at 59.5 MB during the run, settled at 41.7 MB after
  cooldown.
- Server FDs returned to baseline (14) after the test — no FD leak.
