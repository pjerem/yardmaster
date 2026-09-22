//! JSON-lines IPC server over a unix socket.
//!
//! # Contract for the wave-2 integrator (daemon lifecycle)
//!
//! Exact signatures:
//!
//! ```ignore
//! pub trait Handler: Send + Sync + 'static {
//!     fn handle(&self, method: &yard_core::ipc::Method) -> Result<serde_json::Value, String>;
//! }
//!
//! pub async fn serve(
//!     listener: tokio::net::UnixListener,
//!     handler: std::sync::Arc<dyn Handler>,
//!     events: tokio::sync::broadcast::Sender<yard_core::ipc::EventMsg>,
//!     shutdown: tokio::sync::watch::Receiver<bool>,
//! ) -> anyhow::Result<()>;
//! ```
//!
//! Semantics:
//! - The caller binds the listener (at `yard_core::paths::socket_path(state_dir)`)
//!   and owns socket-file cleanup.
//! - On connect the server writes one hello line
//!   (`{"hello":{"version":…,"proto":1,"pid":…}}`), then answers one response
//!   line per request line.
//! - `subscribe` is handled by the server itself (never forwarded to the
//!   `Handler`): it answers `ok:true` with `{"subscribed":true}` and from then
//!   on forwards every `EventMsg` published on `events` as an event line on
//!   that connection. Publish daemon events via `events.send(…)`.
//! - `ping`/`status`/`shutdown` go through `Handler::handle`, whose
//!   `Ok(data)`/`Err(msg)` becomes the `ok:true`/`ok:false` response.
//!   `Handler::handle` runs synchronously on the connection task — keep it
//!   fast and non-blocking.
//! - **Shutdown**: `serve` does NOT stop on a `shutdown` request. The handler
//!   answers `ok:true` and, as a side effect, flips the `watch::Sender<bool>`
//!   to `true`; `serve` returns when its `watch::Receiver` observes `true`
//!   (or the sender is dropped), aborting all connection tasks so it never
//!   lingers on live connections.
//! - A malformed JSON line gets an `ok:false` response (request `id` if it can
//!   be salvaged from the line, else `0`) and the connection stays usable.

use std::sync::Arc;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream, unix::OwnedWriteHalf};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinSet;
use yard_core::ipc::{EventMsg, Hello, HelloFrame, Method, PROTO_VERSION, Request, Response};

pub trait Handler: Send + Sync + 'static {
    /// Handle one request; `Ok(data)` / `Err(msg)` map to the wire
    /// `ok:true` / `ok:false` response.
    fn handle(&self, method: &Method) -> Result<serde_json::Value, String>;
}

/// Serve IPC connections until `shutdown` becomes `true` (or its sender is
/// dropped). See the module docs for the full contract.
pub async fn serve(
    listener: UnixListener,
    handler: Arc<dyn Handler>,
    events: broadcast::Sender<EventMsg>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut connections: JoinSet<()> = JoinSet::new();

    if !*shutdown.borrow_and_update() {
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _addr)) => {
                        let handler = Arc::clone(&handler);
                        let events = events.clone();
                        connections.spawn(async move {
                            if let Err(err) = handle_connection(stream, handler, events).await {
                                tracing::debug!(error = %err, "ipc connection ended with error");
                            }
                        });
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "ipc accept failed");
                    }
                },
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow_and_update() {
                        break;
                    }
                }
                // Reap finished connection tasks so the set doesn't grow.
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
    }

    tracing::info!("ipc server shutting down");
    // Abort live connections so serve() never lingers on slow clients.
    connections.shutdown().await;
    Ok(())
}

async fn handle_connection(
    stream: UnixStream,
    handler: Arc<dyn Handler>,
    events: broadcast::Sender<EventMsg>,
) -> anyhow::Result<()> {
    let (read_half, mut writer) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    let hello = HelloFrame {
        hello: Hello {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            proto: PROTO_VERSION,
            pid: std::process::id(),
        },
    };
    let hello_buf = serde_json::to_vec(&hello).context("serializing hello")?;
    write_json_line(&mut writer, hello_buf)
        .await
        .context("writing hello")?;

    let mut subscription: Option<broadcast::Receiver<EventMsg>> = None;
    loop {
        let line = match subscription.as_mut() {
            Some(rx) => {
                tokio::select! {
                    line = lines.next_line() => line,
                    event = rx.recv() => {
                        match event {
                            Ok(msg) => {
                                let buf = serde_json::to_vec(&msg)
                                    .context("serializing ipc event")?;
                                write_json_line(&mut writer, buf).await?;
                            }
                            Err(broadcast::error::RecvError::Lagged(missed)) => {
                                tracing::warn!(missed, "ipc subscriber lagged; events dropped");
                            }
                            Err(broadcast::error::RecvError::Closed) => subscription = None,
                        }
                        continue;
                    }
                }
            }
            None => lines.next_line().await,
        };

        let Some(line) = line.context("reading request line")? else {
            return Ok(()); // client hung up
        };
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => match request.method {
                Method::Subscribe => {
                    subscription = Some(events.subscribe());
                    Response::ok(request.id, serde_json::json!({"subscribed": true}))
                }
                method => match handler.handle(&method) {
                    Ok(data) => Response::ok(request.id, data),
                    Err(error) => Response::err(request.id, error),
                },
            },
            Err(err) => Response::err(salvage_id(&line), format!("invalid request: {err}")),
        };
        let buf = serde_json::to_vec(&response).context("serializing ipc response")?;
        write_json_line(&mut writer, buf).await?;
    }
}

/// Best-effort request id extraction from a line that failed to parse as a
/// `Request`, so the error response can still be correlated.
fn salvage_id(line: &str) -> u64 {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("id")?.as_u64())
        .unwrap_or(0)
}

// Takes pre-serialized bytes: yard-daemon deliberately has no direct `serde`
// dependency, so a generic `T: Serialize` bound can't be named here.
async fn write_json_line(writer: &mut OwnedWriteHalf, mut buf: Vec<u8>) -> anyhow::Result<()> {
    buf.push(b'\n');
    writer.write_all(&buf).await.context("writing ipc line")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::path::Path;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader, Lines};
    use tokio::net::unix::OwnedReadHalf;
    use tokio::task::JoinHandle;
    use tokio::time::timeout;

    struct StubHandler;

    impl Handler for StubHandler {
        fn handle(&self, method: &Method) -> Result<Value, String> {
            match method {
                Method::Ping => Ok(json!({"pong": true})),
                Method::Status => Ok(json!({"items": []})),
                Method::Shutdown => Ok(Value::Null),
                Method::Subscribe => Err("subscribe must be intercepted by the server".into()),
            }
        }
    }

    struct TestServer {
        events: broadcast::Sender<EventMsg>,
        shutdown: watch::Sender<bool>,
        serve_task: JoinHandle<anyhow::Result<()>>,
    }

    fn start_server(socket: &Path) -> TestServer {
        let listener = UnixListener::bind(socket).expect("bind test socket");
        let (events, _) = broadcast::channel(16);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let serve_task = tokio::spawn(serve(
            listener,
            Arc::new(StubHandler),
            events.clone(),
            shutdown_rx,
        ));
        TestServer {
            events,
            shutdown,
            serve_task,
        }
    }

    struct Client {
        lines: Lines<BufReader<OwnedReadHalf>>,
        writer: OwnedWriteHalf,
    }

    impl Client {
        async fn connect(socket: &Path) -> Self {
            let stream = UnixStream::connect(socket).await.expect("connect");
            let (read_half, writer) = stream.into_split();
            Client {
                lines: BufReader::new(read_half).lines(),
                writer,
            }
        }

        async fn read_line(&mut self) -> String {
            timeout(Duration::from_secs(5), self.lines.next_line())
                .await
                .expect("read timed out")
                .expect("read failed")
                .expect("connection closed")
        }

        async fn hello(&mut self) -> HelloFrame {
            serde_json::from_str(&self.read_line().await).expect("hello frame")
        }

        async fn send_raw(&mut self, line: &str) {
            self.writer.write_all(line.as_bytes()).await.expect("write");
            self.writer.write_all(b"\n").await.expect("write newline");
        }

        async fn request(&mut self, id: u64, method: &str) -> Response {
            self.send_raw(&format!(r#"{{"id":{id},"method":"{method}"}}"#))
                .await;
            serde_json::from_str(&self.read_line().await).expect("response frame")
        }
    }

    #[tokio::test]
    async fn two_concurrent_clients_get_hello_and_responses() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let server = start_server(&socket);

        let (mut c1, mut c2) = tokio::join!(Client::connect(&socket), Client::connect(&socket));
        let (h1, h2) = tokio::join!(c1.hello(), c2.hello());
        for hello in [&h1.hello, &h2.hello] {
            assert_eq!(hello.proto, PROTO_VERSION);
            assert_eq!(hello.version, env!("CARGO_PKG_VERSION"));
            assert_eq!(hello.pid, std::process::id());
        }

        let (ping, status) = tokio::join!(c1.request(1, "ping"), c2.request(2, "status"));
        assert_eq!(ping, Response::ok(1, json!({"pong": true})));
        assert_eq!(status, Response::ok(2, json!({"items": []})));

        server.shutdown.send(true).unwrap();
        server.serve_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn subscriber_receives_broadcast_events() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let server = start_server(&socket);

        let mut client = Client::connect(&socket).await;
        client.hello().await;
        let resp = client.request(1, "subscribe").await;
        assert_eq!(resp, Response::ok(1, json!({"subscribed": true})));

        let event = EventMsg {
            event: "item_updated".into(),
            payload: json!({"id": 7}),
        };
        server.events.send(event.clone()).unwrap();
        let received: EventMsg = serde_json::from_str(&client.read_line().await).unwrap();
        assert_eq!(received, event);

        // The connection still answers requests while subscribed.
        let ping = client.request(2, "ping").await;
        assert_eq!(ping, Response::ok(2, json!({"pong": true})));

        server.shutdown.send(true).unwrap();
        server.serve_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn malformed_line_yields_error_and_connection_survives() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let server = start_server(&socket);

        let mut client = Client::connect(&socket).await;
        client.hello().await;

        client.send_raw("this is not json").await;
        let resp: Response = serde_json::from_str(&client.read_line().await).unwrap();
        assert_eq!(resp.id, 0);
        assert!(resp.result.is_err(), "malformed line must yield ok:false");

        // Parseable JSON but invalid request: the id is salvaged.
        client.send_raw(r#"{"id":9,"method":"reboot"}"#).await;
        let resp: Response = serde_json::from_str(&client.read_line().await).unwrap();
        assert_eq!(resp.id, 9);
        assert!(resp.result.is_err());

        let ping = client.request(3, "ping").await;
        assert_eq!(ping, Response::ok(3, json!({"pong": true})));

        server.shutdown.send(true).unwrap();
        server.serve_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn shutdown_signal_makes_serve_return() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let server = start_server(&socket);

        // A live (even subscribed) connection must not block shutdown.
        let mut client = Client::connect(&socket).await;
        client.hello().await;
        client.request(1, "subscribe").await;

        server.shutdown.send(true).unwrap();
        timeout(Duration::from_secs(5), server.serve_task)
            .await
            .expect("serve did not return after shutdown")
            .unwrap()
            .unwrap();
    }
}
