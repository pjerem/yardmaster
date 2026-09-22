//! Daemon lifecycle: single-instance guard, storage, IPC serving, cleanup.
//!
//! [`run`] is the blocking entry point behind `yard daemon run`. Exactly one
//! daemon owns a state directory at a time, guarded by `daemon.lock`
//! (containing the owner's pid). A second `run` against a live daemon is a
//! quiet no-op; a lock left behind by a dead process is reclaimed.
//!
//! Shutdown paths — a `shutdown` IPC request, SIGTERM, or SIGINT — all flip
//! the same watch channel; `server::serve` returns, a `daemon_stopped` event
//! is appended, and the socket + lock files are removed.

use std::fs;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{broadcast, watch};
use yard_core::config::Config;
use yard_core::ipc::{DaemonInfo, Method, StatusReport, WorkItemStatus};
use yard_core::paths;
use yard_core::storage::Storage;

use crate::server;

/// Runs the daemon for `state_dir` in the foreground until shutdown.
///
/// Returns `Ok(())` without doing anything when another live daemon already
/// holds the lock (double start is a no-op by design: the CLI auto-spawns
/// optimistically and must not fail when it loses the race).
pub fn run(state_dir: PathBuf) -> anyhow::Result<()> {
    // The daemon guards its own config: a broken config must fail loudly
    // here too, not only in the CLI that spawned us.
    Config::load(None).context("loading configuration")?;

    fs::create_dir_all(&state_dir)
        .with_context(|| format!("creating state directory {}", state_dir.display()))?;

    let lock = paths::lock_path(&state_dir);
    match acquire_lock(&lock)? {
        LockState::AlreadyRunning(pid) => {
            tracing::info!(pid, "daemon already running; nothing to do");
            return Ok(());
        }
        LockState::Acquired => {}
    }
    let _lock_guard = RemoveOnDrop(lock);

    // A previous daemon that died without cleanup leaves a dangling socket
    // file; `bind` would refuse it.
    let socket = paths::socket_path(&state_dir);
    remove_if_exists(&socket)?;
    let _socket_guard = RemoveOnDrop(socket.clone());

    let storage = Storage::open(&state_dir).context("opening storage")?;
    let started_at = now_rfc3339();
    storage
        .append_event(
            None,
            "daemon",
            "daemon_started",
            &json!({ "pid": std::process::id(), "version": env!("CARGO_PKG_VERSION") }),
        )
        .context("recording daemon_started event")?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (events_tx, _) = broadcast::channel(64);
    let handler = Arc::new(DaemonHandler {
        storage: Mutex::new(storage),
        state_dir: state_dir.clone(),
        started_at,
        shutdown: shutdown_tx.clone(),
    });

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    let served = runtime.block_on(async {
        let listener =
            UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;

        // SIGTERM/SIGINT funnel into the same shutdown path as the IPC
        // `shutdown` request.
        let mut sigterm = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
        let mut sigint = signal(SignalKind::interrupt()).context("installing SIGINT handler")?;
        let signal_shutdown = shutdown_tx.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = sigterm.recv() => tracing::info!("SIGTERM received; shutting down"),
                _ = sigint.recv() => tracing::info!("SIGINT received; shutting down"),
            }
            let _ = signal_shutdown.send(true);
        });

        tracing::info!(socket = %socket.display(), pid = std::process::id(), "daemon listening");
        server::serve(
            listener,
            Arc::clone(&handler) as Arc<dyn server::Handler>,
            events_tx,
            shutdown_rx,
        )
        .await
    });

    // Best effort even when serve() failed: the stop marker matters most for
    // the audit trail of clean shutdowns, and errors here must not mask the
    // serve error.
    match handler.storage.lock() {
        Ok(storage) => {
            if let Err(err) = storage.append_event(
                None,
                "daemon",
                "daemon_stopped",
                &json!({ "pid": std::process::id() }),
            ) {
                tracing::warn!(error = %err, "failed to record daemon_stopped event");
            }
        }
        Err(_) => tracing::warn!("storage mutex poisoned; daemon_stopped event not recorded"),
    }

    tracing::info!("daemon stopped");
    served // socket + lock removed by the guards
}

/// Outcome of trying to take the single-instance lock.
enum LockState {
    Acquired,
    AlreadyRunning(u32),
}

/// Takes `daemon.lock` with O_EXCL semantics, writing our pid into it.
///
/// On `EEXIST` the owner pid is probed: a live owner means
/// [`LockState::AlreadyRunning`]; a dead owner (or unreadable/garbage lock)
/// is stale — the file is removed and the take retried. The retry loop
/// bounds the remove/re-create race against a concurrent starter.
fn acquire_lock(lock: &Path) -> anyhow::Result<LockState> {
    for _ in 0..5 {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock)
        {
            Ok(mut file) => {
                write!(file, "{}", std::process::id())
                    .with_context(|| format!("writing pid to {}", lock.display()))?;
                return Ok(LockState::Acquired);
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                let owner = fs::read_to_string(lock)
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .filter(|&pid| pid > 0);
                match owner {
                    Some(pid) if pid_alive(pid) => return Ok(LockState::AlreadyRunning(pid)),
                    _ => {
                        tracing::info!(lock = %lock.display(), "removing stale daemon lock");
                        remove_if_exists(lock)?;
                    }
                }
            }
            Err(err) => {
                return Err(err).with_context(|| format!("creating lock file {}", lock.display()));
            }
        }
    }
    bail!(
        "could not acquire {} after repeated attempts (lock churn?)",
        lock.display()
    );
}

/// `kill(pid, 0)`: signal-delivery check without sending a signal.
/// `EPERM` still proves the pid exists (owned by someone else).
fn pid_alive(pid: u32) -> bool {
    // A pid that overflows pid_t is garbage, and the cast would turn e.g.
    // u32::MAX into kill(-1, 0) — "every process I may signal".
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn remove_if_exists(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}

/// Removes a file on drop, so socket and lock disappear on every exit path
/// (including errors) once we own them.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Err(err) = fs::remove_file(&self.0)
            && err.kind() != ErrorKind::NotFound
        {
            tracing::warn!(path = %self.0.display(), error = %err, "cleanup failed");
        }
    }
}

/// IPC request handler backed by [`Storage`].
///
/// `Handler::handle` runs synchronously on the connection task, hence the
/// blocking mutex: every method is a handful of local SQLite reads.
struct DaemonHandler {
    storage: Mutex<Storage>,
    state_dir: PathBuf,
    /// RFC3339 UTC, captured once at boot.
    started_at: String,
    shutdown: watch::Sender<bool>,
}

impl server::Handler for DaemonHandler {
    fn handle(&self, method: &Method) -> Result<Value, String> {
        match method {
            Method::Ping => Ok(json!({})),
            Method::Status => self.status_report().map_err(|err| format!("{err:#}")),
            Method::Shutdown => {
                // serve() returns once it observes `true`; the ok-response
                // for this request is written best-effort before the
                // connection is aborted.
                let _ = self.shutdown.send(true);
                Ok(json!({}))
            }
            // Contract: subscribe is answered by the server itself and never
            // forwarded here.
            Method::Subscribe => Err("subscribe is handled by the server".to_owned()),
        }
    }
}

impl DaemonHandler {
    fn status_report(&self) -> anyhow::Result<Value> {
        let storage = self
            .storage
            .lock()
            .map_err(|_| anyhow::anyhow!("storage mutex poisoned"))?;
        let mut items = Vec::new();
        for row in storage.list_work_items().context("listing work items")? {
            let pending_gates = storage
                .count_pending_gates(row.id)
                .context("counting pending gates")?;
            items.push(WorkItemStatus {
                id: row.id,
                ticket: row.ticket_key,
                repo: row.repo,
                state: row.state,
                agent: None, // agent supervision lands in M2
                pending_gates,
                mergeable: None, // merge-policy evaluation lands in M4
            });
        }
        let report = StatusReport {
            daemon: DaemonInfo {
                version: env!("CARGO_PKG_VERSION").to_owned(),
                pid: std::process::id(),
                started_at: self.started_at.clone(),
                state_dir: self.state_dir.display().to_string(),
            },
            items,
        };
        serde_json::to_value(report).context("serializing status report")
    }
}

/// Current time as an RFC3339 UTC string, seconds precision. Civil-date
/// conversion follows Howard Hinnant's `civil_from_days` algorithm (same
/// construction as yard-core's private storage helper).
fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_shape() {
        let ts = now_rfc3339();
        assert_eq!(ts.len(), 20, "{ts}");
        assert!(ts.ends_with('Z'), "{ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
    }

    #[test]
    fn stale_lock_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("daemon.lock");
        // u32::MAX is far above any real pid ceiling on macOS/Linux.
        fs::write(&lock, u32::MAX.to_string()).unwrap();
        match acquire_lock(&lock).unwrap() {
            LockState::Acquired => {}
            LockState::AlreadyRunning(pid) => panic!("treated stale pid {pid} as live"),
        }
        let owner: u32 = fs::read_to_string(&lock).unwrap().trim().parse().unwrap();
        assert_eq!(owner, std::process::id());
    }

    #[test]
    fn live_lock_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("daemon.lock");
        // Our own pid is definitely alive.
        fs::write(&lock, std::process::id().to_string()).unwrap();
        match acquire_lock(&lock).unwrap() {
            LockState::AlreadyRunning(pid) => assert_eq!(pid, std::process::id()),
            LockState::Acquired => panic!("stole a live lock"),
        }
    }

    #[test]
    fn garbage_lock_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("daemon.lock");
        fs::write(&lock, "not a pid").unwrap();
        assert!(matches!(acquire_lock(&lock).unwrap(), LockState::Acquired));
    }
}
