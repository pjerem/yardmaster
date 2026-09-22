//! Cucumber BDD suite for milestone M1 (socle): daemon lifecycle, `yard
//! status`, and config handling — see `tests/features/`.
//!
//! Every scenario drives the real `yard` binary. Isolation: a per-scenario
//! tempdir provides the state dir and config path, exported to each spawned
//! process via `YARDMASTER_STATE_DIR` / `YARDMASTER_CONFIG` — set on the
//! `Command`, never on the test process itself, so scenarios can run
//! concurrently. Scenario teardown stops (then SIGKILLs) any daemon it
//! started, keeping repeated local runs and CI clean.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use cucumber::gherkin::Step;
use cucumber::{World, given, then, when};
use tempfile::TempDir;

/// Absolute path of the compiled `yard` binary under test.
const YARD_BIN: &str = env!("CARGO_BIN_EXE_yard");

/// Generous ceiling for one CLI invocation; internal timeouts (spawn backoff
/// ~3s, stop wait 5s) are all well below it.
const CMD_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, World)]
#[world(init = Self::new)]
pub struct YardWorld {
    tmp: TempDir,
    /// Outcome of the last `I run "yard …"` step.
    last: Option<CmdResult>,
    /// Daemon pid captured by an earlier step, for cross-step comparisons.
    remembered_pid: Option<u32>,
    /// Foreground `yard daemon run` children spawned by this scenario; reaped
    /// on teardown so no zombies accumulate across the suite.
    daemon_children: Vec<Child>,
}

#[derive(Debug)]
struct CmdResult {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl YardWorld {
    fn new() -> Self {
        Self {
            tmp: TempDir::new().expect("creating scenario tempdir"),
            last: None,
            remembered_pid: None,
            daemon_children: Vec::new(),
        }
    }

    fn state_dir(&self) -> PathBuf {
        self.tmp.path().join("state")
    }

    fn config_path(&self) -> PathBuf {
        self.tmp.path().join("config.toml")
    }

    fn lock_path(&self) -> PathBuf {
        self.state_dir().join("daemon.lock")
    }

    fn socket_path(&self) -> PathBuf {
        self.state_dir().join("daemon.sock")
    }

    /// A `yard` command wired to this scenario's isolated state dir + config.
    fn yard_command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(YARD_BIN);
        command
            .args(args)
            .env("YARDMASTER_STATE_DIR", self.state_dir())
            .env("YARDMASTER_CONFIG", self.config_path());
        command
    }

    /// Runs `yard <args>` to completion, killing it past [`CMD_TIMEOUT`] so a
    /// regression can never hang the suite.
    fn run_yard(&self, args: &[&str]) -> CmdResult {
        let mut child = self
            .yard_command(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawning yard");
        let deadline = Instant::now() + CMD_TIMEOUT;
        while child.try_wait().expect("polling yard").is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let out = child.wait_with_output().expect("reaping timed-out yard");
                panic!(
                    "`yard {}` did not exit within {CMD_TIMEOUT:?}\nstdout: {}\nstderr: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr),
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let out = child.wait_with_output().expect("collecting yard output");
        CmdResult {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn last(&self) -> &CmdResult {
        self.last
            .as_ref()
            .expect("no `I run \"yard …\"` step ran yet")
    }

    /// Daemon pid as recorded in the lock file, when present and sane.
    fn lock_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.lock_path())
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|&pid| pid > 0)
    }
}

impl Drop for YardWorld {
    fn drop(&mut self) {
        // Polite stop first (also exercises cleanup), SIGKILL as a backstop.
        if let Some(pid) = self.lock_pid() {
            let _ = self
                .yard_command(&["daemon", "stop"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if pid_alive(pid) {
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            }
        }
        for child in &mut self.daemon_children {
            let _ = child.kill(); // no-op if already exited
            let _ = child.wait(); // reap
        }
    }
}

/// `kill(pid, 0)`: liveness probe. EPERM would also mean alive, but every
/// process here is ours, so plain success is the signal that matters.
fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    unsafe { libc::kill(pid, 0) == 0 }
}

#[given("a fresh state directory")]
async fn fresh_state_dir(world: &mut YardWorld) {
    // Fresh by construction (per-scenario tempdir); assert it as a guard.
    assert!(
        !world.state_dir().exists(),
        "state dir already exists: {}",
        world.state_dir().display()
    );
}

#[given("a config file containing:")]
async fn config_file(world: &mut YardWorld, step: &Step) {
    let doc = step
        .docstring
        .as_deref()
        .expect("this step requires a docstring")
        .trim();
    std::fs::write(world.config_path(), doc).expect("writing scenario config");
}

#[given(regex = r#"^a daemon started with "yard daemon run"$"#)]
async fn daemon_started(world: &mut YardWorld) {
    let child = world
        .yard_command(&["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning foreground daemon");
    world.daemon_children.push(child);

    // Readiness: lock (with a live pid) + socket both present.
    let deadline = Instant::now() + Duration::from_secs(5);
    let pid = loop {
        if let Some(pid) = world.lock_pid()
            && world.socket_path().exists()
        {
            break pid;
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not come up within 5s"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(pid_alive(pid), "daemon pid {pid} from lock is not alive");
    world.remembered_pid = Some(pid);
}

// ----------------------------------------------------------------- whens --

#[when(regex = r#"^I run "yard ?(.*)"$"#)]
async fn run_yard(world: &mut YardWorld, args: String) {
    let args: Vec<&str> = args.split_whitespace().collect();
    world.last = Some(world.run_yard(&args));
}

#[when("the launcher process has exited")]
async fn launcher_exited(world: &mut YardWorld) {
    // Documentation step: `run_yard` waits for the launcher to exit, so by
    // now it is gone; anything still answering is the detached daemon.
    assert!(
        world.last().code.is_some(),
        "launcher was killed, not exited"
    );
}

// ----------------------------------------------------------------- thens --

#[then("the command succeeds")]
async fn command_succeeds(world: &mut YardWorld) {
    let last = world.last();
    assert_eq!(
        last.code,
        Some(0),
        "expected exit 0\nstdout: {}\nstderr: {}",
        last.stdout,
        last.stderr
    );
}

#[then("the command fails")]
async fn command_fails(world: &mut YardWorld) {
    let last = world.last();
    assert!(
        last.code.is_some_and(|code| code != 0),
        "expected a nonzero exit code, got {:?}\nstdout: {}\nstderr: {}",
        last.code,
        last.stdout,
        last.stderr
    );
}

#[then("the daemon is running")]
async fn daemon_running(world: &mut YardWorld) {
    let pid = world.lock_pid().expect("lock file with a pid");
    assert!(pid_alive(pid), "daemon pid {pid} is not alive");
    world.remembered_pid.get_or_insert(pid);
}

#[then("the daemon pid is unchanged")]
async fn daemon_pid_unchanged(world: &mut YardWorld) {
    let remembered = world
        .remembered_pid
        .expect("no daemon pid recorded earlier");
    let current = world.lock_pid().expect("lock file with a pid");
    assert_eq!(
        current, remembered,
        "a different daemon took over the state dir"
    );
}

#[then("the lock file exists")]
async fn lock_exists(world: &mut YardWorld) {
    assert!(world.lock_path().exists());
}

#[then("the socket file exists")]
async fn socket_exists(world: &mut YardWorld) {
    assert!(world.socket_path().exists());
}

#[then("the lock file is gone")]
async fn lock_gone(world: &mut YardWorld) {
    assert!(!world.lock_path().exists());
}

#[then("the socket file is gone")]
async fn socket_gone(world: &mut YardWorld) {
    assert!(!world.socket_path().exists());
}

#[then("no daemon was started")]
async fn no_daemon(world: &mut YardWorld) {
    assert!(!world.lock_path().exists(), "unexpected daemon lock");
    assert!(!world.socket_path().exists(), "unexpected daemon socket");
}

#[then(expr = "the output mentions {string}")]
async fn output_mentions(world: &mut YardWorld, needle: String) {
    let last = world.last();
    assert!(
        last.stdout.contains(&needle),
        "stdout does not mention {needle:?}\nstdout: {}",
        last.stdout
    );
}

#[then(expr = "the error output mentions {string}")]
async fn error_output_mentions(world: &mut YardWorld, needle: String) {
    let last = world.last();
    assert!(
        last.stderr.contains(&needle),
        "stderr does not mention {needle:?}\nstderr: {}",
        last.stderr
    );
}

#[then("the JSON daemon version matches the crate version")]
async fn json_version(world: &mut YardWorld) {
    let report: serde_json::Value =
        serde_json::from_str(&world.last().stdout).expect("stdout is a JSON report");
    assert_eq!(report["daemon"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(report["daemon"]["pid"].as_u64().is_some_and(|pid| pid > 0));
}

#[then("the JSON report lists no items")]
async fn json_no_items(world: &mut YardWorld) {
    let report: serde_json::Value =
        serde_json::from_str(&world.last().stdout).expect("stdout is a JSON report");
    assert_eq!(report["items"], serde_json::json!([]));
}

#[then(expr = "the event log records {string}")]
async fn event_recorded(world: &mut YardWorld, kind: String) {
    let db = world.state_dir().join("yardmaster.db");
    let conn = rusqlite::Connection::open(&db).expect("opening event-log db");
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE kind = ?1",
            [&kind],
            |row| row.get(0),
        )
        .expect("querying events");
    assert!(count >= 1, "no {kind:?} event in {}", db.display());
}

#[tokio::main]
async fn main() {
    YardWorld::run("tests/features").await;
}
