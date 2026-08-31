//! End-to-end WebSocket test: spin up a real Actus server with a `ws::upgrade`
//! handler, connect with a `tokio-tungstenite` client, and round-trip frames.
#![cfg(feature = "websocket")]

use actus::prelude::*;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;

struct WsEcho;

#[controller]
impl WsEcho {
    routes! {
        GET "echo" => echo(),
    }

    pub async fn echo(&self) -> Reply {
        Ok(ws::upgrade(|mut socket| async move {
            while let Some(Ok(msg)) = socket.next().await {
                match msg {
                    Message::Text(_) | Message::Binary(_) => {
                        if socket.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }))
    }
}

struct Health;

#[controller]
impl Health {
    routes! {
        GET "" => ping(),
    }

    pub async fn ping(&self) -> Reply {
        reply!()
    }
}

app_routes! {
    routes {
        "ws"     => WsEcho,
        "health" => Health,
    }
}

#[tokio::test]
async fn websocket_round_trip() {
    // Bind the listener here and hand it to the server — not bind, read the
    // port, drop and re-bind: parallel tests re-bind a freed port in that gap
    // (measured on CI 2026-08-31), and `run_with_shutdown_listener` exists so
    // a test never has to. Already listening when we get here.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        Server::new(init().await.unwrap())
            .run_with_shutdown_listener(listener, async move {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
    });

    let url = format!("ws://127.0.0.1:{port}/ws/echo");
    let (mut socket, response) = tokio_tungstenite::connect_async(url)
        .await
        .expect("websocket handshake");
    assert_eq!(response.status().as_u16(), 101);

    socket.send(Message::text("ping")).await.unwrap();
    assert_eq!(
        socket.next().await.expect("a frame").expect("ok frame"),
        Message::text("ping")
    );

    socket.send(Message::binary(vec![1u8, 2, 3])).await.unwrap();
    assert_eq!(
        socket.next().await.expect("a frame").expect("ok frame"),
        Message::binary(vec![1u8, 2, 3])
    );

    socket.close(None).await.ok();

    let _ = stop_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
}
