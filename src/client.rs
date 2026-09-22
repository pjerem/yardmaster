//! Blocking JSON-lines IPC client for CLI commands.
//!
//! Speaks the exact `yard_core::ipc` wire protocol over a std `UnixStream`
//! with per-call read timeouts: quick queries fail fast on a wedged daemon,
//! while scheduler-driving commands (`add`, `approve`, …) get room for the
//! provider/forge/git round trips they trigger.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::Value;
use yard_core::ipc::{HelloFrame, Method, PROTO_VERSION, Request, Response};
use yard_core::paths;

/// Per-read/write socket timeout for quick methods (local SQLite reads) and
/// the connection handshake; anything slower means a wedged daemon.
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// Read timeout for scheduler-driving methods: ticket fetch, worktree
/// creation, agent spawn, or PR creation happen before the reply.
pub const SLOW_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    /// Daemon pid, taken from the hello greeting.
    pub daemon_pid: u32,
}

impl Client {
    /// Connects to `{state_dir}/daemon.sock`, reads the hello greeting, and
    /// verifies the protocol version.
    pub fn connect(state_dir: &Path) -> anyhow::Result<Client> {
        let socket = paths::socket_path(state_dir);
        let stream = UnixStream::connect(&socket)
            .with_context(|| format!("connecting to {}", socket.display()))?;
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .context("setting read timeout")?;
        stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .context("setting write timeout")?;
        let writer = stream.try_clone().context("cloning socket handle")?;
        let mut reader = BufReader::new(stream);

        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .context("reading server hello")?;
        if n == 0 {
            bail!("daemon closed the connection before greeting");
        }
        let frame: HelloFrame = serde_json::from_str(&line).context("parsing server hello")?;
        if frame.hello.proto != PROTO_VERSION {
            bail!(
                "daemon (pid {}) speaks protocol v{}; this client expects v{PROTO_VERSION} — \
                 restart the daemon after upgrading",
                frame.hello.pid,
                frame.hello.proto
            );
        }
        Ok(Client {
            reader,
            writer,
            next_id: 1,
            daemon_pid: frame.hello.pid,
        })
    }

    /// Sends one parameterless request with the quick timeout.
    pub fn request(&mut self, method: Method) -> anyhow::Result<Value> {
        self.call(method, None, IO_TIMEOUT)
    }

    /// Sends one request and reads its response line, waiting up to
    /// `timeout` for the reply.
    pub fn call(
        &mut self,
        method: Method,
        params: Option<Value>,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request = Request { id, method, params };
        let mut buf = serde_json::to_vec(&request).context("serializing request")?;
        buf.push(b'\n');
        self.writer
            .write_all(&buf)
            .with_context(|| format!("sending {method} request"))?;

        self.reader
            .get_ref()
            .set_read_timeout(Some(timeout))
            .context("setting read timeout")?;
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .with_context(|| format!("reading {method} response"))?;
        if n == 0 {
            bail!("daemon closed the connection before answering {method}");
        }
        let response: Response = serde_json::from_str(&line).context("parsing daemon response")?;
        if response.id != id {
            bail!("daemon answered request {} instead of {id}", response.id);
        }
        response
            .result
            .map_err(|msg| anyhow::anyhow!("daemon refused {method}: {msg}"))
    }
}
