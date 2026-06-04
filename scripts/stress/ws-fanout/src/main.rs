//! WebSocket fanout stress test for actus.
//!
//! Opens N concurrent WebSocket connections to a `ws::upgrade(...)`-backed
//! endpoint (`examples/basic`'s `/ws/echo` by default), echoes a small
//! message every ~100 ms on each, holds open for D seconds, then closes
//! gracefully and reports throughput.
//!
//! What this exercises:
//!
//! - Hyper's `with_upgrades()` accept loop under many concurrent handshakes.
//! - `ws::upgrade` machinery (101 response → `OnUpgrade` future → task
//!   spawn per connection).
//! - File-descriptor accounting: N concurrent connections = N FDs on
//!   both client and server. Run `lsof -p <server_pid>` before and after
//!   to confirm no FD leak.
//!
//! Usage:
//!     cargo run --release -- [--connections N] [--duration SECS] [--url URL]

use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() -> anyhow::Result<()> {
    let mut connections: usize = 1_000;
    let mut duration_secs: u64 = 30;
    let mut url = "ws://127.0.0.1:3000/ws/echo".to_string();
    let mut tick_ms: u64 = 100;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--connections" => connections = args.next().expect("--connections N").parse()?,
            "--duration" => duration_secs = args.next().expect("--duration SECS").parse()?,
            "--url" => url = args.next().expect("--url URL"),
            "--tick-ms" => tick_ms = args.next().expect("--tick-ms MS").parse()?,
            "--help" | "-h" => {
                eprintln!(
                    "usage: ws-fanout [--connections N] [--duration SECS] [--url URL] [--tick-ms MS]\n\
                     defaults: 1000 connections, 30 s, ws://127.0.0.1:3000/ws/echo, 100 ms"
                );
                return Ok(());
            }
            other => anyhow::bail!("unknown arg: {other}"),
        }
    }

    println!(
        "ws-fanout: {connections} connections × {duration_secs} s @ {tick_ms} ms ticks → {url}"
    );

    let total_sent = Arc::new(AtomicU64::new(0));
    let total_recv = Arc::new(AtomicU64::new(0));
    let total_failed = Arc::new(AtomicU64::new(0));
    let opened = Arc::new(AtomicU64::new(0));

    let start = Instant::now();
    let mut tasks = Vec::with_capacity(connections);

    for i in 0..connections {
        let url = url.clone();
        let total_sent = total_sent.clone();
        let total_recv = total_recv.clone();
        let total_failed = total_failed.clone();
        let opened = opened.clone();

        tasks.push(tokio::spawn(async move {
            let (mut socket, _) = match connect_async(&url).await {
                Ok(r) => r,
                Err(_) => {
                    total_failed.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            };
            opened.fetch_add(1, Ordering::Relaxed);

            let deadline = Instant::now() + Duration::from_secs(duration_secs);
            let mut tick = tokio::time::interval(Duration::from_millis(tick_ms));
            tick.tick().await; // first tick fires immediately; consume it
            let mut sent = 0u64;
            let mut recv = 0u64;

            loop {
                if Instant::now() >= deadline {
                    break;
                }
                tokio::select! {
                    _ = tick.tick() => {
                        let msg = Message::text(format!("c{i}:s{sent}"));
                        if socket.send(msg).await.is_err() { break; }
                        sent += 1;
                    }
                    res = socket.next() => {
                        match res {
                            Some(Ok(Message::Text(_))) | Some(Ok(Message::Binary(_))) => {
                                recv += 1;
                            }
                            Some(Ok(Message::Close(_))) | None => break,
                            Some(Err(_)) => break,
                            _ => {}
                        }
                    }
                }
            }

            let _ = socket.close(None).await;
            total_sent.fetch_add(sent, Ordering::Relaxed);
            total_recv.fetch_add(recv, Ordering::Relaxed);
        }));

        // Print a progress dot every 100 connections so the user knows
        // the dial-out half is making progress.
        if i > 0 && i % 100 == 0 {
            print!(".");
            use std::io::Write;
            std::io::stdout().flush().ok();
        }
    }
    println!();
    println!(
        "spawned {connections} tasks in {:?}; running for {duration_secs} s...",
        start.elapsed()
    );

    for t in tasks {
        let _ = t.await;
    }

    let elapsed = start.elapsed();
    let sent = total_sent.load(Ordering::Relaxed);
    let recv = total_recv.load(Ordering::Relaxed);
    let failed = total_failed.load(Ordering::Relaxed);
    let opened_n = opened.load(Ordering::Relaxed);

    println!();
    println!("=== results ===");
    println!("elapsed:            {:?}", elapsed);
    println!("connections opened: {} / {}", opened_n, connections);
    println!("failed handshakes:  {}", failed);
    println!("messages sent:      {}", sent);
    println!("messages received:  {}", recv);
    println!(
        "round-trips / sec:  {:.0}",
        recv as f64 / elapsed.as_secs_f64()
    );
    println!(
        "round-trips / conn: {:.1}",
        if opened_n > 0 {
            recv as f64 / opened_n as f64
        } else {
            0.0
        }
    );

    if failed > 0 {
        anyhow::bail!("{failed} handshake(s) failed — accept loop or backlog limit?");
    }
    Ok(())
}
