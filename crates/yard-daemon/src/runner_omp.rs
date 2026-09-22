//! Generic child-process agent supervision (issue #9).
//!
//! [`ProcessRunner`] drives any agent backend that can be started as a plain
//! command line — no tmux, no PTY, no backend-specific flags baked in. The
//! concrete argv templates arrive via [`BackendSpec`] (wired from config in
//! #13); `{placeholders}` inside argv elements are substituted per session:
//!
//! | placeholder           | value                                          |
//! |-----------------------|------------------------------------------------|
//! | `{worktree}`          | [`AgentTask::worktree`]                        |
//! | `{instructions_file}` | `<session_dir>/instructions.md`                |
//! | `{ticket_key}`        | ticket key string (e.g. `owner/repo#7`)        |
//! | `{session_dir}`       | the session directory                          |
//! | `{message_file}`      | resume only: path of the follow-up message     |
//!
//! Unknown `{...}` tokens are left untouched (they may be meaningful to the
//! backend itself).
//!
//! ```text
//! # HYPOTHETICAL, ILLUSTRATIVE ONLY — real omp templates are configuration
//! # (#13), never hardcoded here, and this syntax is unverified:
//! # start_cmd  = ["omp", "--cwd", "{worktree}", "--instructions", "{instructions_file}"]
//! # resume_cmd = ["omp", "resume", "--cwd", "{worktree}", "--message", "{message_file}"]
//! ```
//!
//! ### Session layout (everything under `<sessions_root>/<id>/`)
//!
//! Written by the runner:
//! - `instructions.md` — orchestrator-composed instructions.
//! - `task.json` — the serialized [`AgentTask`]; lets a freshly constructed
//!   runner recover worktree/ticket context for `resume` after a restart.
//! - `transcript.log` — backend stdout+stderr, append-only across resumes.
//! - `pid` — pid of the most recently spawned backend process.
//! - `exit_code` — written by the waiter thread when that process exits:
//!   decimal exit code, or `signal:<n>` when killed by a signal.
//! - `messages/<n>.md` — follow-up messages fed through `resume` (n = 1, 2, …).
//!
//! ### Backend contract (files the backend may write into `{session_dir}`)
//! - `result.json` — `{"success": bool, "summary": string}`: authoritative
//!   outcome, preferred over exit-code inference.
//! - `usage.json` — `{"input": u64, "output": u64}` token usage; absent ⇒
//!   usage reports zeros.
//! - `question` — plain-text prompt. Its *presence* means the backend waits
//!   for human input, even if the process has exited (non-interactive
//!   backends may exit and expect to be re-spawned via `resume`). `resume`
//!   consumes (deletes) it.
//!
//! ### Supervision design
//! Each spawn puts the backend in its own process group (daemon signals never
//! hit it) with `transcript.log` as stdout+stderr and the task worktree as
//! cwd. A detached *waiter thread* per spawn blocks in `wait()` — reaping the
//! child so no zombie lingers — then records the outcome atomically in
//! `exit_code`. `status()` never blocks: it derives the answer from
//! `question` → `exit_code`/`kill(pid, 0)` → `result.json`.
//!
//! ### What survives a `ProcessRunner` drop or a daemon restart
//! All session state lives on disk, so `status`/`usage`/`transcript`/
//! `resume`/`cancel` work from a recreated runner. Waiter threads are
//! detached: dropping the runner within the same process changes nothing.
//! If the *daemon process* itself dies, running backends keep running (own
//! process group) and are eventually reaped by init — but nobody writes
//! `exit_code` for them. Once such a session exits, `status` reports the
//! backend's own `result.json` if present, otherwise `Failed` with an
//! explicit "outcome unrecorded" message. A recycled pid could briefly be
//! misread as `Running` in that window — accepted for a laptop tool.
//! `cancel` may also overshoot a recycled pid after a daemon restart; same
//! trade-off. Resume assumes the previous spawn has exited (it resets `pid`
//! and `exit_code`); overlapping backend processes are the backend's problem.

use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use yard_core::adapters::{
    AdapterError, AdapterResult, AgentRunner, AgentSessionId, AgentStatus, AgentTask, TokenUsage,
};

const INSTRUCTIONS_FILE: &str = "instructions.md";
const TASK_FILE: &str = "task.json";
const TRANSCRIPT_FILE: &str = "transcript.log";
const PID_FILE: &str = "pid";
const EXIT_FILE: &str = "exit_code";
const RESULT_FILE: &str = "result.json";
const USAGE_FILE: &str = "usage.json";
const QUESTION_FILE: &str = "question";
const MESSAGES_DIR: &str = "messages";

/// How long `cancel` waits after SIGTERM before escalating to SIGKILL.
const SIGTERM_GRACE: Duration = Duration::from_secs(5);
/// How long `cancel` waits after SIGKILL before giving up.
const SIGKILL_GRACE: Duration = Duration::from_secs(5);
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Command-line templates for one agent backend. See the module docs for the
/// placeholder table.
#[derive(Debug, Clone)]
pub struct BackendSpec {
    /// Argv template used by [`AgentRunner::start`]. Must be non-empty.
    pub start_cmd: Vec<String>,
    /// Argv template used by [`AgentRunner::resume`]. `None` means the
    /// backend cannot be resumed; `resume` then fails permanently.
    pub resume_cmd: Option<Vec<String>>,
}

/// Backend-agnostic [`AgentRunner`] built on plain child processes.
pub struct ProcessRunner {
    spec: BackendSpec,
    sessions_root: PathBuf,
}

/// Backend-authored outcome (`result.json`).
#[derive(Deserialize)]
struct ResultFile {
    success: bool,
    #[serde(default)]
    summary: String,
}

impl ProcessRunner {
    pub fn new(spec: BackendSpec, sessions_root: PathBuf) -> Self {
        Self {
            spec,
            sessions_root,
        }
    }

    /// Joins the id onto `sessions_root`, refusing ids that could escape it.
    /// Ids normally come from our own `start`, but they round-trip through
    /// storage, so a cheap guard beats a path-traversal surprise.
    fn session_dir(&self, id: &AgentSessionId) -> AdapterResult<PathBuf> {
        if id.0.is_empty() || id.0.contains(['/', '\\']) || id.0.contains("..") {
            return Err(AdapterError::Permanent(format!(
                "malformed session id {:?}",
                id.0
            )));
        }
        Ok(self.sessions_root.join(&id.0))
    }

    fn existing_session_dir(&self, id: &AgentSessionId) -> AdapterResult<PathBuf> {
        let dir = self.session_dir(id)?;
        if !dir.is_dir() {
            return Err(AdapterError::Permanent(format!(
                "unknown session {:?}",
                id.0
            )));
        }
        Ok(dir)
    }

    /// Creates a fresh uniquely-named session directory. `create_dir` (not
    /// `create_dir_all`) is the collision check: an existing dir means the
    /// short id collided and we retry with a new one.
    fn allocate_session(&self) -> AdapterResult<(String, PathBuf)> {
        fs::create_dir_all(&self.sessions_root).map_err(|e| {
            AdapterError::Transient(format!(
                "creating sessions root {}: {e}",
                self.sessions_root.display()
            ))
        })?;
        for _ in 0..16 {
            let id = random_session_id();
            let dir = self.sessions_root.join(&id);
            match fs::create_dir(&dir) {
                Ok(()) => return Ok((id, dir)),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(AdapterError::Transient(format!(
                        "creating session dir {}: {e}",
                        dir.display()
                    )));
                }
            }
        }
        Err(AdapterError::Transient(
            "could not allocate a unique session id".into(),
        ))
    }

    /// Spawns `argv` detached into its own process group, transcript-attached,
    /// records `pid`, and leaves a waiter thread behind to reap it and record
    /// `exit_code`.
    fn spawn_backend(&self, session_dir: &Path, argv: &[String], cwd: &Path) -> AdapterResult<()> {
        use std::os::unix::process::CommandExt as _;

        let Some((program, args)) = argv.split_first() else {
            return Err(AdapterError::Permanent(
                "backend command template is empty".into(),
            ));
        };
        let transcript_path = session_dir.join(TRANSCRIPT_FILE);
        let transcript = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&transcript_path)
            .map_err(|e| {
                AdapterError::Transient(format!(
                    "opening transcript {}: {e}",
                    transcript_path.display()
                ))
            })?;
        let transcript_err = transcript
            .try_clone()
            .map_err(|e| AdapterError::Transient(format!("cloning transcript handle: {e}")))?;

        let child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(transcript)
            .stderr(transcript_err)
            // Own process group: daemon-directed signals (SIGINT from a
            // terminal, SIGTERM at shutdown) must not tear agents down.
            .process_group(0)
            .spawn()
            .map_err(|e| AdapterError::Permanent(format!("spawning backend {program:?}: {e}")))?;

        let pid = child.id();
        atomic_write(&session_dir.join(PID_FILE), &pid.to_string())
            .map_err(|e| AdapterError::Transient(format!("recording pid: {e}")))?;
        tracing::info!(pid, session_dir = %session_dir.display(), "spawned agent backend");

        let exit_path = session_dir.join(EXIT_FILE);
        std::thread::spawn(move || waiter(child, exit_path));
        Ok(())
    }

    fn load_task(dir: &Path) -> AdapterResult<AgentTask> {
        let raw = read_optional(&dir.join(TASK_FILE))?.ok_or_else(|| {
            AdapterError::Permanent(format!("session {} has no task.json", dir.display()))
        })?;
        serde_json::from_str(&raw)
            .map_err(|e| AdapterError::Permanent(format!("corrupt task.json: {e}")))
    }
}

impl AgentRunner for ProcessRunner {
    fn start(&self, task: &AgentTask) -> AdapterResult<AgentSessionId> {
        if self.spec.start_cmd.is_empty() {
            return Err(AdapterError::Permanent(
                "backend start command is empty".into(),
            ));
        }
        let (id, dir) = self.allocate_session()?;

        fs::write(dir.join(INSTRUCTIONS_FILE), &task.instructions)
            .map_err(|e| AdapterError::Transient(format!("writing instructions: {e}")))?;
        let task_json = serde_json::to_string_pretty(task)
            .map_err(|e| AdapterError::Permanent(format!("serializing task: {e}")))?;
        fs::write(dir.join(TASK_FILE), task_json)
            .map_err(|e| AdapterError::Transient(format!("writing task.json: {e}")))?;

        let argv = substitute(&self.spec.start_cmd, &placeholder_vars(&dir, task, None));
        self.spawn_backend(&dir, &argv, &task.worktree)?;
        Ok(AgentSessionId(id))
    }

    fn resume(&self, id: &AgentSessionId, message: &str) -> AdapterResult<()> {
        let Some(resume_cmd) = self.spec.resume_cmd.as_deref() else {
            return Err(AdapterError::Permanent(
                "backend has no resume command configured".into(),
            ));
        };
        let dir = self.existing_session_dir(id)?;
        let task = Self::load_task(&dir)?;

        let messages_dir = dir.join(MESSAGES_DIR);
        fs::create_dir_all(&messages_dir)
            .map_err(|e| AdapterError::Transient(format!("creating messages dir: {e}")))?;
        let n = next_message_number(&messages_dir)?;
        let message_file = messages_dir.join(format!("{n}.md"));
        fs::write(&message_file, message)
            .map_err(|e| AdapterError::Transient(format!("writing message file: {e}")))?;

        // The question (if any) is now answered, and the previous run's exit
        // record must not shadow the resumed run's status.
        remove_if_exists(&dir.join(QUESTION_FILE))?;
        remove_if_exists(&dir.join(EXIT_FILE))?;

        let argv = substitute(
            resume_cmd,
            &placeholder_vars(&dir, &task, Some(&message_file)),
        );
        self.spawn_backend(&dir, &argv, &task.worktree)
    }

    fn status(&self, id: &AgentSessionId) -> AdapterResult<AgentStatus> {
        let dir = self.existing_session_dir(id)?;

        // A question always wins: the backend may have exited on purpose,
        // waiting to be resumed with the answer.
        if let Some(prompt) = read_optional(&dir.join(QUESTION_FILE))? {
            return Ok(AgentStatus::WaitingInput {
                prompt: prompt.trim_end().to_string(),
            });
        }

        // Exit determination: a recorded `exit_code` is authoritative (immune
        // to pid reuse); otherwise probe the recorded pid.
        let mut exit_record = read_optional(&dir.join(EXIT_FILE))?;
        let exited = match &exit_record {
            Some(_) => true,
            None => match read_pid(&dir)? {
                Some(pid) => !pid_alive(pid),
                // No pid recorded: the spawn never completed.
                None => true,
            },
        };
        if !exited {
            return Ok(AgentStatus::Running);
        }
        // The waiter records `exit_code` moments after the child dies; grant a
        // bounded grace window before concluding the record is lost (daemon
        // restarted mid-flight). Keeps status() race-free with a live waiter.
        if exit_record.is_none() && read_pid(&dir)?.is_some() {
            for _ in 0..10 {
                std::thread::sleep(Duration::from_millis(50));
                exit_record = read_optional(&dir.join(EXIT_FILE))?;
                if exit_record.is_some() {
                    break;
                }
            }
        }

        if let Some(raw) = read_optional(&dir.join(RESULT_FILE))? {
            let result: ResultFile = serde_json::from_str(&raw)
                .map_err(|e| AdapterError::Permanent(format!("corrupt result.json: {e}")))?;
            return Ok(AgentStatus::Finished {
                success: result.success,
                summary: result.summary,
            });
        }

        Ok(match exit_record.as_deref().map(str::trim) {
            Some("0") => AgentStatus::Finished {
                success: true,
                summary: "backend exited 0 without result.json".into(),
            },
            Some(code) if code.parse::<i32>().is_ok() => AgentStatus::Failed {
                message: format!("backend exited with code {code} without result.json"),
            },
            Some(sig) if sig.starts_with("signal:") => AgentStatus::Failed {
                message: format!("backend killed by {sig}"),
            },
            Some(other) => AgentStatus::Failed {
                message: format!("backend outcome unrecorded ({other})"),
            },
            None => AgentStatus::Failed {
                message:
                    "backend exited without recorded outcome (supervisor restarted before exit?)"
                        .into(),
            },
        })
    }

    fn cancel(&self, id: &AgentSessionId) -> AdapterResult<()> {
        let dir = self.existing_session_dir(id)?;
        // Nothing spawned or already reaped ⇒ idempotent success.
        let Some(pid) = read_pid(&dir)? else {
            return Ok(());
        };
        if !pid_alive(pid) {
            return Ok(());
        }

        // The backend owns its process group (see spawn_backend), so signal
        // the whole group: shells and their children die together.
        signal_group(pid, libc::SIGTERM);
        let deadline = Instant::now() + SIGTERM_GRACE;
        while pid_alive(pid) && Instant::now() < deadline {
            std::thread::sleep(CANCEL_POLL);
        }
        if pid_alive(pid) {
            tracing::warn!(pid, "backend ignored SIGTERM; escalating to SIGKILL");
            signal_group(pid, libc::SIGKILL);
            let deadline = Instant::now() + SIGKILL_GRACE;
            while pid_alive(pid) && Instant::now() < deadline {
                std::thread::sleep(CANCEL_POLL);
            }
        }
        if pid_alive(pid) {
            return Err(AdapterError::Transient(format!(
                "backend pid {pid} survived SIGKILL (not yet reaped?)"
            )));
        }
        Ok(())
    }

    fn usage(&self, id: &AgentSessionId) -> AdapterResult<TokenUsage> {
        let dir = self.existing_session_dir(id)?;
        match read_optional(&dir.join(USAGE_FILE))? {
            None => Ok(TokenUsage::default()),
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|e| AdapterError::Permanent(format!("corrupt usage.json: {e}"))),
        }
    }

    fn transcript(&self, id: &AgentSessionId) -> AdapterResult<Option<PathBuf>> {
        let path = self.session_dir(id)?.join(TRANSCRIPT_FILE);
        Ok(path.is_file().then_some(path))
    }
}

// ---------------------------------------------------------------------------
// Helpers

/// Replaces `{key}` tokens in every argv element. Unknown tokens survive.
fn substitute(template: &[String], vars: &[(&str, String)]) -> Vec<String> {
    template
        .iter()
        .map(|arg| {
            let mut out = arg.clone();
            for (key, value) in vars {
                let token = format!("{{{key}}}");
                if out.contains(&token) {
                    out = out.replace(&token, value);
                }
            }
            out
        })
        .collect()
}

fn placeholder_vars(
    session_dir: &Path,
    task: &AgentTask,
    message_file: Option<&Path>,
) -> Vec<(&'static str, String)> {
    let mut vars = vec![
        ("worktree", task.worktree.display().to_string()),
        (
            "instructions_file",
            session_dir.join(INSTRUCTIONS_FILE).display().to_string(),
        ),
        ("ticket_key", task.ticket.key.key.clone()),
        ("session_dir", session_dir.display().to_string()),
    ];
    if let Some(path) = message_file {
        vars.push(("message_file", path.display().to_string()));
    }
    vars
}

/// Reaps the child and records its outcome. Runs on a detached thread; the
/// `exit_code` write is atomic (tmp + rename) so `status` never sees a
/// half-written record.
fn waiter(mut child: Child, exit_path: PathBuf) {
    use std::os::unix::process::ExitStatusExt as _;

    let record = match child.wait() {
        Ok(status) => match status.code() {
            Some(code) => code.to_string(),
            None => format!("signal:{}", status.signal().unwrap_or(0)),
        },
        Err(e) => {
            tracing::error!(error = %e, "waiting on agent backend failed");
            "unknown".to_string()
        }
    };
    if let Err(e) = atomic_write(&exit_path, &record) {
        tracing::error!(error = %e, path = %exit_path.display(), "recording backend exit failed");
    }
}

fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)
}

fn read_optional(path: &Path) -> AdapterResult<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AdapterError::Transient(format!(
            "reading {}: {e}",
            path.display()
        ))),
    }
}

fn remove_if_exists(path: &Path) -> AdapterResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(AdapterError::Transient(format!(
            "removing {}: {e}",
            path.display()
        ))),
    }
}

fn read_pid(dir: &Path) -> AdapterResult<Option<i32>> {
    match read_optional(&dir.join(PID_FILE))? {
        None => Ok(None),
        Some(raw) => raw
            .trim()
            .parse::<i32>()
            .map(Some)
            .map_err(|e| AdapterError::Permanent(format!("corrupt pid file: {e}"))),
    }
}

/// `kill(pid, 0)` liveness probe. EPERM still means "exists". Note a zombie
/// counts as alive until the waiter reaps it — a short window at most.
fn pid_alive(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Signals the backend's process group (`pgid == pid`, set at spawn).
/// Failure (ESRCH: already gone) is intentionally ignored.
fn signal_group(pid: i32, signal: libc::c_int) {
    unsafe {
        libc::kill(-pid, signal);
    }
}

fn next_message_number(messages_dir: &Path) -> AdapterResult<u32> {
    let entries = fs::read_dir(messages_dir)
        .map_err(|e| AdapterError::Transient(format!("listing {}: {e}", messages_dir.display())))?;
    let mut max = 0u32;
    for entry in entries {
        let entry = entry.map_err(|e| AdapterError::Transient(format!("listing messages: {e}")))?;
        if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str())
            && let Ok(n) = stem.parse::<u32>()
        {
            max = max.max(n);
        }
    }
    Ok(max + 1)
}

/// Short random hex id. `RandomState` is randomly seeded per process and
/// perturbed per instantiation; mixed with time + pid it is unique enough,
/// and `allocate_session` retries on the (astronomically unlikely) collision.
fn random_session_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let mut hasher = RandomState::new().build_hasher();
    if let Ok(elapsed) = SystemTime::now().duration_since(UNIX_EPOCH) {
        hasher.write_u128(elapsed.as_nanos());
    }
    hasher.write_u32(std::process::id());
    let mut id = format!("{:016x}", hasher.finish());
    id.truncate(12);
    id
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use yard_core::adapters::{Ticket, TicketKey};

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        path
    }

    fn stub_task(root: &Path) -> AgentTask {
        let worktree = root.join("wt");
        fs::create_dir_all(&worktree).unwrap();
        AgentTask {
            work_item_id: 1,
            worktree,
            ticket: Ticket {
                key: TicketKey {
                    provider: "github".into(),
                    key: "acme/app#7".into(),
                },
                title: "add feature".into(),
                body: "details".into(),
                status: "open".into(),
                assignee: Some("me".into()),
                url: "https://example.test/7".into(),
                blocked_by: vec![],
            },
            instructions: "do the thing".into(),
        }
    }

    /// Scripts receive `$1 = {session_dir}` (start) and `$2 = {message_file}`
    /// (resume).
    fn runner(root: &Path, start: &Path, resume: Option<&Path>) -> ProcessRunner {
        let spec = BackendSpec {
            start_cmd: vec![
                "/bin/sh".into(),
                start.display().to_string(),
                "{session_dir}".into(),
            ],
            resume_cmd: resume.map(|r| {
                vec![
                    "/bin/sh".into(),
                    r.display().to_string(),
                    "{session_dir}".into(),
                    "{message_file}".into(),
                ]
            }),
        };
        ProcessRunner::new(spec, root.join("sessions"))
    }

    fn wait_until<T>(mut probe: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(value) = probe() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out waiting for condition");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_settled(runner: &ProcessRunner, id: &AgentSessionId) -> AgentStatus {
        wait_until(|| match runner.status(id).unwrap() {
            AgentStatus::Running => None,
            settled => Some(settled),
        })
    }

    #[test]
    fn happy_path_finishes_with_result_usage_and_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_script(
            tmp.path(),
            "ok.sh",
            r#"echo hello from stub
printf '{"success":true,"summary":"done"}' > "$1/result.json"
printf '{"input":10,"output":20}' > "$1/usage.json""#,
        );
        let runner = runner(tmp.path(), &script, None);
        let task = stub_task(tmp.path());
        let id = runner.start(&task).unwrap();

        let session_dir = tmp.path().join("sessions").join(&id.0);
        assert_eq!(
            fs::read_to_string(session_dir.join("instructions.md")).unwrap(),
            task.instructions
        );
        let stored: AgentTask =
            serde_json::from_str(&fs::read_to_string(session_dir.join("task.json")).unwrap())
                .unwrap();
        assert_eq!(stored.worktree, task.worktree);

        assert_eq!(
            wait_settled(&runner, &id),
            AgentStatus::Finished {
                success: true,
                summary: "done".into()
            }
        );
        assert_eq!(
            runner.usage(&id).unwrap(),
            TokenUsage {
                input: 10,
                output: 20
            }
        );

        let transcript = runner.transcript(&id).unwrap().expect("transcript path");
        assert!(
            fs::read_to_string(transcript)
                .unwrap()
                .contains("hello from stub")
        );
    }

    #[test]
    fn nonzero_exit_without_result_is_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_script(tmp.path(), "fail.sh", "exit 3");
        let runner = runner(tmp.path(), &script, None);
        let id = runner.start(&stub_task(tmp.path())).unwrap();

        match wait_settled(&runner, &id) {
            AgentStatus::Failed { message } => assert!(message.contains('3'), "{message}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        // No backend usage.json ⇒ zeros, not an error.
        assert_eq!(runner.usage(&id).unwrap(), TokenUsage::default());
    }

    #[test]
    fn question_file_yields_waiting_input_and_resume_answers_it() {
        let tmp = tempfile::tempdir().unwrap();
        // Exits right after asking: the question must win over the exit.
        let start = write_script(
            tmp.path(),
            "ask.sh",
            r#"printf 'Which DB?' > "$1/question""#,
        );
        let resume = write_script(
            tmp.path(),
            "answer.sh",
            r#"cat "$2" > "$1/answer.txt"
printf '{"success":true,"summary":"answered"}' > "$1/result.json""#,
        );
        let runner = runner(tmp.path(), &start, Some(&resume));
        let id = runner.start(&stub_task(tmp.path())).unwrap();

        let prompt = wait_until(|| match runner.status(&id).unwrap() {
            AgentStatus::WaitingInput { prompt } => Some(prompt),
            _ => None,
        });
        assert_eq!(prompt, "Which DB?");

        runner.resume(&id, "use postgres").unwrap();
        let session_dir = tmp.path().join("sessions").join(&id.0);
        assert_eq!(
            fs::read_to_string(session_dir.join("messages/1.md")).unwrap(),
            "use postgres"
        );
        assert!(
            !session_dir.join("question").exists(),
            "resume consumes the question"
        );

        assert_eq!(
            wait_settled(&runner, &id),
            AgentStatus::Finished {
                success: true,
                summary: "answered".into()
            }
        );
        // {message_file} really pointed the backend at the message.
        assert_eq!(
            fs::read_to_string(session_dir.join("answer.txt")).unwrap(),
            "use postgres"
        );
    }

    #[test]
    fn cancel_kills_sleeping_backend_and_waiter_reaps_it() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_script(tmp.path(), "sleep.sh", "sleep 30");
        let runner = runner(tmp.path(), &script, None);
        let id = runner.start(&stub_task(tmp.path())).unwrap();

        assert_eq!(runner.status(&id).unwrap(), AgentStatus::Running);
        runner.cancel(&id).unwrap();

        match wait_settled(&runner, &id) {
            AgentStatus::Failed { message } => assert!(message.contains("signal"), "{message}"),
            other => panic!("expected Failed after cancel, got {other:?}"),
        }
        // Reaped, not a zombie: kill(pid, 0) reports ESRCH (a zombie would
        // still count as alive).
        let session_dir = tmp.path().join("sessions").join(&id.0);
        let pid: i32 = fs::read_to_string(session_dir.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        wait_until(|| (!pid_alive(pid)).then_some(()));
        // Idempotent: cancelling a dead session is fine.
        runner.cancel(&id).unwrap();
    }

    #[test]
    fn resume_without_resume_cmd_is_permanent() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_script(tmp.path(), "noop.sh", ":");
        let runner = runner(tmp.path(), &script, None);
        let id = runner.start(&stub_task(tmp.path())).unwrap();

        let err = runner.resume(&id, "more work").unwrap_err();
        assert!(matches!(err, AdapterError::Permanent(_)), "{err:?}");
    }

    #[test]
    fn placeholders_substitute_in_argv() {
        let tmp = tempfile::tempdir().unwrap();
        let task = stub_task(tmp.path());
        let session_dir = tmp.path().join("sessions/abc123");
        let message_file = session_dir.join("messages/2.md");

        let template = vec![
            "run".to_string(),
            "--wt={worktree}".to_string(),
            "{instructions_file}".to_string(),
            "key={ticket_key} in {session_dir}".to_string(),
            "{message_file}".to_string(),
            "{unknown}".to_string(),
        ];
        let argv = substitute(
            &template,
            &placeholder_vars(&session_dir, &task, Some(&message_file)),
        );
        assert_eq!(
            argv,
            vec![
                "run".to_string(),
                format!("--wt={}", task.worktree.display()),
                session_dir.join("instructions.md").display().to_string(),
                format!("key=acme/app#7 in {}", session_dir.display()),
                message_file.display().to_string(),
                "{unknown}".to_string(),
            ]
        );
    }

    #[test]
    fn sessions_survive_runner_drop_and_recreate() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_script(
            tmp.path(),
            "ok.sh",
            r#"printf '{"success":true,"summary":"done"}' > "$1/result.json"
printf '{"input":1,"output":2}' > "$1/usage.json""#,
        );
        let first = runner(tmp.path(), &script, None);
        let id = first.start(&stub_task(tmp.path())).unwrap();
        drop(first);

        // A brand-new runner over the same sessions_root sees everything:
        // the detached waiter thread keeps recording the exit regardless.
        let second = runner(tmp.path(), &script, None);
        assert_eq!(
            wait_settled(&second, &id),
            AgentStatus::Finished {
                success: true,
                summary: "done".into()
            }
        );
        assert_eq!(
            second.usage(&id).unwrap(),
            TokenUsage {
                input: 1,
                output: 2
            }
        );
        assert!(second.transcript(&id).unwrap().is_some());

        let unknown = AgentSessionId("deadbeef0000".into());
        assert!(matches!(
            second.status(&unknown).unwrap_err(),
            AdapterError::Permanent(_)
        ));
    }
}
