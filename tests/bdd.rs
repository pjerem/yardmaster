//! Cucumber BDD suite: milestone M1 (socle — daemon lifecycle, `yard
//! status`, config handling) plus the M2 end-to-end ticket→PR flow. See
//! `tests/features/`.
//!
//! Every scenario drives the real `yard` binary. Isolation: a per-scenario
//! tempdir provides the state dir and config path, exported to each spawned
//! process via `YARDMASTER_STATE_DIR` / `YARDMASTER_CONFIG` — set on the
//! `Command`, never on the test process itself, so scenarios can run
//! concurrently. Scenario teardown stops (then SIGKILLs) any daemon it
//! started, keeping repeated local runs and CI clean.
//!
//! The E2E sandbox is fully offline: a local source repo pushing to a local
//! BARE remote, a wiremock server standing in for both the GitHub issue API
//! and the forge, a `/bin/sh` stub as the agent backend, and a dummy token
//! injected through the `YARDMASTER_SECRET_*` env fallback.

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
    /// Mocked GitHub API (issues + pulls) for the E2E sandbox.
    mock: Option<wiremock::MockServer>,
    /// E2E sandbox variant: `check_command` that always fails.
    check_fails: bool,
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
            mock: None,
            check_fails: false,
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
    /// The dummy provider token rides the `YARDMASTER_SECRET_*` env fallback
    /// (harmless for scenarios without providers).
    fn yard_command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(YARD_BIN);
        command
            .args(args)
            .env("YARDMASTER_STATE_DIR", self.state_dir())
            .env("YARDMASTER_CONFIG", self.config_path())
            .env("YARDMASTER_SECRET_PROVIDERS_GH", "dummy-test-token");
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

    // ----------------------------------------------------------- e2e sandbox

    fn repo_dir(&self) -> PathBuf {
        self.tmp.path().join("repo")
    }

    fn remote_dir(&self) -> PathBuf {
        self.tmp.path().join("remote.git")
    }

    fn agent_script(&self) -> PathBuf {
        self.tmp.path().join("agent.sh")
    }

    /// Local source repo + BARE origin remote + stub agent script + wiremock
    /// GitHub API (issue GET, pulls POST) + a config wiring them together.
    /// Everything offline; the daemon under test never leaves localhost.
    async fn setup_sandbox(&mut self, assignee: &str) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Source repo with one commit on main, pushing to a local bare remote.
        let repo = self.repo_dir();
        let remote = self.remote_dir();
        git(self.tmp.path(), &["init", "-q", "-b", "main", "repo"]);
        git(&repo, &["config", "user.name", "Yard Test"]);
        git(&repo, &["config", "user.email", "yard@test.invalid"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("README.md"), "# demo\n").expect("writing README");
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-q", "-m", "initial commit"]);
        git(
            self.tmp.path(),
            &["init", "-q", "--bare", "-b", "main", "remote.git"],
        );
        git(
            &repo,
            &["remote", "add", "origin", &remote.to_string_lossy()],
        );
        git(&repo, &["push", "-q", "-u", "origin", "main"]);

        // Stub agent: leaves a commit in the worktree (cwd) and reports
        // success through the ProcessRunner backend contract ($1 = session
        // dir, stdout lands in transcript.log).
        std::fs::write(
            self.agent_script(),
            concat!(
                "#!/bin/sh\n",
                "set -e\n",
                "echo \"stub agent starting in $PWD\"\n",
                "echo \"frobnicator\" > agent-work.txt\n",
                "git add agent-work.txt\n",
                "git commit -q -m \"agent: add frobnicator\"\n",
                "printf '{\"success\": true, \"summary\": \"implemented the frobnicator\"}'",
                " > \"$1/result.json\"\n",
            ),
        )
        .expect("writing agent script");

        // Mocked GitHub API: one open issue and a pulls endpoint.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/issues/23"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "number": 23,
                "title": "Add frobnicator",
                "body": "Please add the frobnicator.",
                "state": "open",
                "assignee": { "login": assignee },
                "html_url": "https://github.test/acme/widgets/issues/23",
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/acme/widgets/pulls"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "number": 7,
            })))
            .mount(&server)
            .await;
        self.mock = Some(server);

        self.write_sandbox_config();
    }

    /// (Re)writes the sandbox config; called again when a variant step flips
    /// a knob (e.g. the failing check command).
    fn write_sandbox_config(&self) {
        let mock_uri = self.mock.as_ref().expect("sandbox not set up").uri();
        let check_line = if self.check_fails {
            "check_command = [\"/bin/sh\", \"-c\", \"echo boom >&2; exit 1\"]\n"
        } else {
            ""
        };
        let config = format!(
            "[providers.gh]\n\
             kind = \"github\"\n\
             url = \"{mock_uri}\"\n\
             user = \"testuser\"\n\
             \n\
             [agents.stub]\n\
             start_cmd = [\"/bin/sh\", \"{script}\", \"{{session_dir}}\"]\n\
             \n\
             [repos.demo]\n\
             path = \"{repo}\"\n\
             forge = \"github\"\n\
             base = \"main\"\n\
             provider = \"gh\"\n\
             remote_repo = \"acme/widgets\"\n\
             agent = \"stub\"\n\
             {check_line}",
            script = self.agent_script().display(),
            repo = self.repo_dir().display(),
        );
        std::fs::write(self.config_path(), config).expect("writing sandbox config");
    }

    /// `yard <args> --json`-style helper: runs the command, asserts success,
    /// parses stdout.
    fn yard_json(&self, args: &[&str]) -> serde_json::Value {
        let result = self.run_yard(args);
        assert_eq!(
            result.code,
            Some(0),
            "`yard {}` failed\nstdout: {}\nstderr: {}",
            args.join(" "),
            result.stdout,
            result.stderr
        );
        serde_json::from_str(&result.stdout).expect("JSON output")
    }

    /// State of the single sandbox work item, per `yard status --json`.
    fn item_state(&self) -> String {
        let report = self.yard_json(&["status", "--json"]);
        report["items"][0]["state"]
            .as_str()
            .unwrap_or_else(|| panic!("no work item in status report: {report}"))
            .to_owned()
    }

    /// `(kind, payload)` event rows of work item 1, in insertion order, read
    /// straight from the daemon's WAL-mode sqlite (readers never block).
    fn item_events(&self) -> Vec<(String, serde_json::Value)> {
        let conn = rusqlite::Connection::open(self.state_dir().join("yardmaster.db"))
            .expect("opening event-log db");
        let mut stmt = conn
            .prepare("SELECT kind, payload FROM events WHERE work_item_id = 1 ORDER BY id")
            .expect("preparing events query");
        let rows = stmt
            .query_map([], |row| {
                let kind: String = row.get(0)?;
                let payload: String = row.get(1)?;
                Ok((kind, payload))
            })
            .expect("querying events");
        rows.map(|row| {
            let (kind, payload) = row.expect("event row");
            let payload = serde_json::from_str(&payload).expect("event payload JSON");
            (kind, payload)
        })
        .collect()
    }

    /// POST requests the mocked forge received on its pulls endpoint.
    async fn pr_creations(&self) -> Vec<wiremock::Request> {
        self.mock
            .as_ref()
            .expect("sandbox not set up")
            .received_requests()
            .await
            .expect("wiremock request recording enabled")
            .into_iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/pulls"))
            .collect()
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

/// Runs `git -C <dir> <args>`, panicking with captured output on failure —
/// sandbox construction must never fail silently.
fn git(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("running git");
    assert!(
        out.status.success(),
        "git -C {} {} failed\nstdout: {}\nstderr: {}",
        dir.display(),
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// True when `refs/heads/<branch>` exists in the (bare) repo at `dir`.
fn has_branch(dir: &std::path::Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .output()
        .expect("running git rev-parse")
        .status
        .success()
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

#[given(expr = "an e2e sandbox with the ticket assigned to {string}")]
async fn e2e_sandbox(world: &mut YardWorld, assignee: String) {
    world.setup_sandbox(&assignee).await;
}

#[given("the sandbox check command fails")]
async fn sandbox_failing_check(world: &mut YardWorld) {
    world.check_fails = true;
    world.write_sandbox_config();
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

// ------------------------------------------------------------- e2e thens --

#[then("the daemon reports no work items")]
async fn daemon_no_items(world: &mut YardWorld) {
    let report = world.yard_json(&["status", "--json"]);
    assert_eq!(
        report["items"],
        serde_json::json!([]),
        "expected an empty backlog: {report}"
    );
}

#[then(expr = "within {int} seconds a {string} gate is pending")]
async fn gate_pending_within(world: &mut YardWorld, secs: u64, action: String) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let gates = world.yard_json(&["gates", "--json"]);
        let pending = gates["gates"]
            .as_array()
            .expect("gates array")
            .iter()
            .any(|gate| gate["action"] == action.as_str());
        if pending {
            return;
        }
        // Escalation means the pipeline failed; surface the audit trail
        // instead of a blind timeout.
        let state = world.item_state();
        assert_ne!(
            state,
            "escalated",
            "work item escalated while waiting for a {action} gate; events: {:#?}",
            world.item_events()
        );
        assert!(
            Instant::now() < deadline,
            "no pending {action} gate after {secs}s (item state {state}); events: {:#?}",
            world.item_events()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[then(expr = "within {int} seconds the work item is in state {string}")]
async fn item_state_within(world: &mut YardWorld, secs: u64, expected: String) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let state = world.item_state();
        if state == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "work item stuck in state {state} (wanted {expected}); events: {:#?}",
            world.item_events()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[then(expr = "the work item is in state {string}")]
async fn item_state_now(world: &mut YardWorld, expected: String) {
    let state = world.item_state();
    assert_eq!(state, expected, "events: {:#?}", world.item_events());
}

#[then("the work item worktree exists")]
async fn worktree_exists(world: &mut YardWorld) {
    let conn =
        rusqlite::Connection::open(world.state_dir().join("yardmaster.db")).expect("opening db");
    let path: String = conn
        .query_row(
            "SELECT worktree_path FROM work_items WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .expect("work item 1 with a worktree path");
    let path = PathBuf::from(path);
    assert!(
        path.is_dir(),
        "worktree {} is not a directory",
        path.display()
    );
    assert!(
        path.join("agent-work.txt").is_file(),
        "stub agent's file missing in {}",
        path.display()
    );
}

#[then(expr = "the bare remote has branch {string}")]
async fn remote_has_branch(world: &mut YardWorld, branch: String) {
    assert!(
        has_branch(&world.remote_dir(), &branch),
        "branch {branch} not on the bare remote"
    );
}

#[then(expr = "the bare remote has no branch {string}")]
async fn remote_has_no_branch(world: &mut YardWorld, branch: String) {
    assert!(
        !has_branch(&world.remote_dir(), &branch),
        "branch {branch} unexpectedly pushed to the bare remote"
    );
}

#[then(expr = "the forge received {int} PR creations")]
async fn forge_pr_count(world: &mut YardWorld, expected: usize) {
    let posts = world.pr_creations().await;
    assert_eq!(
        posts.len(),
        expected,
        "unexpected POST /pulls count: {posts:?}"
    );
}

#[then(
    expr = "the forge received exactly one PR creation for head {string} into {string} as a draft"
)]
async fn forge_pr_payload(world: &mut YardWorld, head: String, base: String) {
    let posts = world.pr_creations().await;
    assert_eq!(
        posts.len(),
        1,
        "expected exactly one POST /pulls: {posts:?}"
    );
    let body: serde_json::Value =
        serde_json::from_slice(&posts[0].body).expect("PR creation body is JSON");
    assert_eq!(body["head"], head.as_str(), "payload: {body}");
    assert_eq!(body["base"], base.as_str(), "payload: {body}");
    assert_eq!(body["draft"], true, "payload: {body}");
    assert!(
        body["title"].as_str().is_some_and(|t| !t.is_empty()),
        "payload: {body}"
    );
}

/// Asserts the comma-separated event tokens appear as an ordered
/// subsequence of work item 1's event log. A token is a kind
/// (`branch_pushed`) or `state_changed:<new>` to pin the transition target.
#[then(expr = "the item event log records, in order: {string}")]
async fn events_in_order(world: &mut YardWorld, expected: String) {
    let events = world.item_events();
    let mut cursor = 0usize;
    for token in expected.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let (kind, new_state) = match token.split_once(':') {
            Some((kind, new_state)) => (kind, Some(new_state)),
            None => (token, None),
        };
        let found = events[cursor..].iter().position(|(k, payload)| {
            k == kind && new_state.is_none_or(|state| payload["new"] == state)
        });
        match found {
            Some(offset) => cursor += offset + 1,
            None => {
                panic!("event {token:?} not found (in order) in the item event log:\n{events:#?}")
            }
        }
    }
}

#[tokio::main]
async fn main() {
    YardWorld::run("tests/features").await;
}
