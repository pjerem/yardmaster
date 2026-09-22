//! Work-item scheduler (issue #13): drives `yard add` from ticket intake to
//! the 🔴 create-pr gate, and executes gate verdicts.
//!
//! ## Ownership & wiring
//!
//! The daemon builds one [`Scheduler`] at boot from the loaded [`Config`]:
//! - one [`GithubProvider`] + [`GithubForge`] per `kind = "github"` provider
//!   entry (same API URL, same token — secret key `providers.<name>`, env
//!   fallback `YARDMASTER_SECRET_PROVIDERS_<NAME>`),
//! - one [`ProcessRunner`] per agent backend, sessions under
//!   `{state_dir}/agents`,
//! - one [`WorktreeManager`] per repo.
//!
//! ## Async bridge
//!
//! The IPC [`crate::server::Handler`] is synchronous and runs on the blocking
//! pool; the scheduler runs as one tokio task owning all mutable scheduling
//! state. They meet over an mpsc channel of [`Command`]s with oneshot
//! replies: the handler `blocking_send`s a command and `blocking_recv`s the
//! reply. Adapter calls (HTTP, git, process spawn) go through
//! `spawn_blocking`; the storage mutex is never held across an await.
//!
//! ## Gate semantics (SPEC §principle 4)
//!
//! - local checks (`check_command`) are 🟢: automatic, failure escalates;
//! - the branch push is 🟠: automatic + `branch_pushed` audit event;
//! - PR creation is 🔴: a [`GateEngine`] request carrying the **full**
//!   [`PrDraft`] payload; the item stays `developing` until a human verdict.
//!   `approve` executes `Forge::create_pr` and moves the item to
//!   `pr_pending`; `reject` escalates.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::Context as _;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use yard_core::adapters::{
    AgentRunner, AgentSessionId, AgentStatus, AgentTask, Forge, PrDraft, SecretStore as _, Ticket,
    TicketKey, TicketProvider,
};
use yard_core::config::{Config, Repo, render_template, slugify};
use yard_core::gate::{GateEngine, GatePolicy, GatedAction};
use yard_core::ipc::{AddResult, ApproveResult, GateInfo, GateList, LogsResult, RejectResult};
use yard_core::state::{WorkflowState, assignee_violation};
use yard_core::storage::{Storage, StorageError, WorkItemRow};

use crate::forge_github::GithubForge;
use crate::provider_github::GithubProvider;
use crate::runner_omp::{BackendSpec, ProcessRunner};
use crate::secrets;
use crate::worktree::{WorktreeManager, WorktreeSpec};

/// Cadence of the developing-items poll.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Transcript lines returned by `logs` when the client names no tail.
const DEFAULT_LOG_TAIL: usize = 50;
/// Max characters of stderr echoed into escalation events.
const STDERR_TAIL_CHARS: usize = 2000;

/// One IPC request routed into the scheduler task. Replies are oneshot;
/// a dropped receiver just discards the answer.
pub enum Command {
    Add {
        ticket: String,
        repo: Option<String>,
        ty: Option<String>,
        reply: oneshot::Sender<Result<AddResult, String>>,
    },
    Gates {
        reply: oneshot::Sender<Result<GateList, String>>,
    },
    Approve {
        gate_id: i64,
        reply: oneshot::Sender<Result<ApproveResult, String>>,
    },
    Reject {
        gate_id: i64,
        reason: Option<String>,
        reply: oneshot::Sender<Result<RejectResult, String>>,
    },
    Logs {
        item: i64,
        tail: Option<usize>,
        reply: oneshot::Sender<Result<LogsResult, String>>,
    },
}

/// Ticket provider + forge built from one `[providers.*]` github entry.
struct ProviderEntry {
    /// Forge-side login; the assignee hard invariant compares against it.
    user: String,
    provider: Arc<GithubProvider>,
    forge: Arc<GithubForge>,
}

pub struct Scheduler {
    storage: Arc<Mutex<Storage>>,
    config: Config,
    policy: GatePolicy,
    providers: HashMap<String, ProviderEntry>,
    /// Keyed by agent backend name (`[agents.*]` or the built-in `omp`).
    runners: HashMap<String, Arc<ProcessRunner>>,
    /// Keyed by repo config name.
    worktrees: HashMap<String, Arc<WorktreeManager>>,
    /// Work items whose `agent_question` event was already appended, so the
    /// poll loop logs one event per question instead of one per tick.
    asked: HashSet<i64>,
}

impl Scheduler {
    /// Builds all adapters from the configuration. Providers that cannot be
    /// driven (non-github kind, missing `user`, missing token) are skipped
    /// with a warning: the daemon must boot for `status` even half-configured;
    /// `add` reports the precise gap on use.
    ///
    /// Call from a synchronous context: blocking HTTP clients are created
    /// here.
    pub fn new(
        config: Config,
        state_dir: &Path,
        storage: Arc<Mutex<Storage>>,
    ) -> anyhow::Result<Scheduler> {
        let secret_store = secrets::default_chain(state_dir);
        let mut providers = HashMap::new();
        for (name, cfg) in &config.providers {
            if cfg.kind != "github" {
                tracing::warn!(provider = %name, kind = %cfg.kind, "provider kind not driven yet; skipped");
                continue;
            }
            let Some(user) = cfg.user.clone() else {
                tracing::warn!(provider = %name, "github provider has no `user`; skipped");
                continue;
            };
            let token = match secret_store.get(&format!("providers.{name}")) {
                Ok(Some(token)) => token,
                Ok(None) => {
                    tracing::warn!(
                        provider = %name,
                        "no token found (secret key providers.{name}); skipped"
                    );
                    continue;
                }
                Err(err) => {
                    tracing::warn!(provider = %name, error = %err, "secret store failed; skipped");
                    continue;
                }
            };
            // `my_tickets` scans the remote repos wired to this provider.
            let remote_repos: Vec<String> = config
                .repos
                .values()
                .filter(|repo| repo.provider.as_deref() == Some(name.as_str()))
                .filter_map(|repo| repo.remote_repo.clone())
                .collect();
            let api_url = cfg.api_url();
            let forge = GithubForge::new(api_url.clone(), token.clone())
                .with_context(|| format!("building forge for provider '{name}'"))?;
            let provider =
                GithubProvider::new(name.clone(), api_url, token, user.clone(), remote_repos);
            providers.insert(
                name.clone(),
                ProviderEntry {
                    user,
                    provider: Arc::new(provider),
                    forge: Arc::new(forge),
                },
            );
        }

        let sessions_root = state_dir.join("agents");
        let backend_names: BTreeSet<&str> = config
            .repos
            .values()
            .map(Repo::agent_name)
            .chain(config.agents.keys().map(String::as_str))
            .collect();
        let mut runners = HashMap::new();
        for name in backend_names {
            // Config validation guarantees every referenced name resolves.
            let Some(agent) = config.agent(name) else {
                continue;
            };
            runners.insert(
                name.to_owned(),
                Arc::new(ProcessRunner::new(
                    BackendSpec {
                        start_cmd: agent.start_cmd,
                        resume_cmd: agent.resume_cmd,
                    },
                    sessions_root.clone(),
                )),
            );
        }

        let worktrees = config
            .repos
            .iter()
            .map(|(name, repo)| {
                (
                    name.clone(),
                    Arc::new(WorktreeManager::new(WorktreeSpec {
                        repo_path: repo.path.clone(),
                        worktrees_root: repo.worktrees_root(),
                        setup_hook: repo.setup_hook.clone(),
                        teardown_hook: repo.teardown_hook.clone(),
                    })),
                )
            })
            .collect();

        let policy = GatePolicy::new(config.profile.lock_public_gate, []);
        Ok(Scheduler {
            storage,
            config,
            policy,
            providers,
            runners,
            worktrees,
            asked: HashSet::new(),
        })
    }

    /// Scheduler task body: serves [`Command`]s and polls developing items
    /// every [`POLL_INTERVAL`]. Returns when the command channel closes
    /// (daemon shutdown).
    pub async fn run(mut self, mut rx: mpsc::Receiver<Command>) {
        let mut poll = tokio::time::interval(POLL_INTERVAL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                command = rx.recv() => match command {
                    Some(command) => self.dispatch(command).await,
                    None => return,
                },
                _ = poll.tick() => self.tick().await,
            }
        }
    }

    async fn dispatch(&mut self, command: Command) {
        match command {
            Command::Add {
                ticket,
                repo,
                ty,
                reply,
            } => {
                let _ = reply.send(self.add(&ticket, repo.as_deref(), ty.as_deref()).await);
            }
            Command::Gates { reply } => {
                let _ = reply.send(self.gates());
            }
            Command::Approve { gate_id, reply } => {
                let _ = reply.send(self.approve(gate_id).await);
            }
            Command::Reject {
                gate_id,
                reason,
                reply,
            } => {
                let _ = reply.send(self.reject(gate_id, reason.as_deref()));
            }
            Command::Logs { item, tail, reply } => {
                let _ = reply.send(self.logs(item, tail.unwrap_or(DEFAULT_LOG_TAIL)));
            }
        }
    }

    /// Storage guard; a poisoned mutex is recovered (SQLite transactions keep
    /// the file consistent regardless of a panicked peer).
    fn storage(&self) -> MutexGuard<'_, Storage> {
        self.storage.lock().unwrap_or_else(|e| e.into_inner())
    }

    // -------------------------------------------------------------- add ----

    /// `yard add`: resolve ticket + repo, enforce the assignee invariant,
    /// then work item → branch → worktree → agent, leaving the item
    /// `developing` under the poll loop's watch.
    async fn add(
        &mut self,
        ticket_ref: &str,
        repo: Option<&str>,
        ty: Option<&str>,
    ) -> Result<AddResult, String> {
        let (provider_name, ticket_key) = resolve_ticket_ref(&self.config, ticket_ref)?;
        let repo_name = resolve_repo_name(&self.config, repo)?;
        let repo_cfg = self.config.repos[&repo_name].clone();
        if let Some(expected) = &repo_cfg.provider {
            if *expected != provider_name {
                return Err(format!(
                    "ticket provider '{provider_name}' does not match repo '{repo_name}' \
                     (configured provider '{expected}')"
                ));
            }
        } else {
            return Err(format!(
                "repo '{repo_name}' has no provider configured; set `provider` in [repos.{repo_name}]"
            ));
        }
        let entry = self.providers.get(&provider_name).ok_or_else(|| {
            format!(
                "provider '{provider_name}' is not usable: it needs kind = \"github\", a `user`, \
                 and a token under secret key providers.{provider_name} \
                 (env YARDMASTER_SECRET_PROVIDERS_{})",
                provider_name.to_uppercase().replace(['.', '-'], "_")
            )
        })?;

        // Fetch the ticket and enforce SPEC §principle 6 before anything is
        // created: someone else's ticket leaves zero local trace.
        let provider = Arc::clone(&entry.provider);
        let key = TicketKey {
            provider: provider_name.clone(),
            key: ticket_key,
        };
        let fetch_key = key.clone();
        let ticket = blocking(move || provider.get(&fetch_key))
            .await?
            .map_err(|e| format!("fetching ticket {}: {e}", key.key))?;
        if let Some(reason) = assignee_violation(&ticket, &entry.user) {
            return Err(format!(
                "refusing to add: {reason} — yardmaster never touches someone else's ticket"
            ));
        }

        // Work item: tracked → queued.
        let item_id = {
            let storage = self.storage();
            let item_id = storage
                .create_work_item(
                    &provider_name,
                    &ticket.key.key,
                    &repo_name,
                    WorkflowState::Tracked.as_str(),
                )
                .map_err(|e| friendly_create_error(e, &ticket.key.key, &repo_name))?;
            storage
                .append_event(
                    Some(item_id),
                    "system",
                    "ticket_added",
                    &json!({
                        "provider": provider_name,
                        "ticket": ticket.key.key,
                        "title": ticket.title,
                        "url": ticket.url,
                    }),
                )
                .map_err(|e| e.to_string())?;
            item_id
        };
        self.update_state(item_id, WorkflowState::Queued)?;

        // Branch + worktree.
        let branch = branch_for(&repo_cfg, ty, &ticket.key.key, &ticket.title)?;
        self.storage()
            .set_work_item_branch(item_id, &branch)
            .map_err(|e| e.to_string())?;
        let manager = Arc::clone(&self.worktrees[&repo_name]);
        let (wt_branch, wt_base) = (branch.clone(), repo_cfg.base.clone());
        let worktree = match blocking(move || manager.create(&wt_branch, &wt_base)).await? {
            Ok(path) => path,
            Err(err) => {
                let msg = format!("creating worktree for branch {branch}: {err}");
                self.escalate(item_id, "worktree_failed", json!({ "error": msg }));
                return Err(msg);
            }
        };
        {
            let storage = self.storage();
            storage
                .set_work_item_worktree(item_id, &worktree.to_string_lossy())
                .map_err(|e| e.to_string())?;
            storage
                .append_event(
                    Some(item_id),
                    "system",
                    "worktree_created",
                    &json!({ "path": worktree.to_string_lossy(), "branch": branch }),
                )
                .map_err(|e| e.to_string())?;
        }

        // Agent start.
        let backend = repo_cfg.agent_name().to_owned();
        let runner = Arc::clone(&self.runners[&backend]);
        let task = AgentTask {
            work_item_id: item_id,
            worktree: worktree.clone(),
            instructions: compose_instructions(&ticket, &repo_name, &repo_cfg, &branch),
            ticket,
        };
        let session = match blocking(move || runner.start(&task)).await? {
            Ok(session) => session,
            Err(err) => {
                let msg = format!("starting agent backend '{backend}': {err}");
                self.escalate(item_id, "agent_start_failed", json!({ "error": msg }));
                return Err(msg);
            }
        };
        {
            let storage = self.storage();
            storage
                .set_work_item_agent_session(item_id, &session.0)
                .map_err(|e| e.to_string())?;
            storage
                .append_event(
                    Some(item_id),
                    "system",
                    "agent_started",
                    &json!({ "backend": backend, "session": session.0 }),
                )
                .map_err(|e| e.to_string())?;
        }
        self.update_state(item_id, WorkflowState::Developing)?;

        Ok(AddResult {
            id: item_id,
            ticket: key.key,
            repo: repo_name,
            branch,
            worktree: worktree.to_string_lossy().into_owned(),
            state: WorkflowState::Developing.as_str().to_owned(),
        })
    }

    // ------------------------------------------------------------- poll ----

    /// One poll pass: every `developing` item without a pending gate gets a
    /// backend status probe; finished agents flow into checks → push → gate.
    async fn tick(&mut self) {
        let developing: Vec<WorkItemRow> = {
            let storage = self.storage();
            match storage.list_work_items() {
                Ok(items) => items
                    .into_iter()
                    .filter(|item| item.state == WorkflowState::Developing.as_str())
                    .collect(),
                Err(err) => {
                    tracing::warn!(error = %err, "poll: listing work items failed");
                    return;
                }
            }
        };
        for item in developing {
            // A pending gate means the pipeline already ran to its 🔴 stop.
            match self.storage().count_pending_gates(item.id) {
                Ok(0) => {}
                Ok(_) => continue,
                Err(err) => {
                    tracing::warn!(item = item.id, error = %err, "poll: gate count failed");
                    continue;
                }
            }
            let session = match self.storage().work_item_agent_session(item.id) {
                Ok(Some(session)) => session,
                Ok(None) => continue, // no agent yet (mid-add)
                Err(err) => {
                    tracing::warn!(item = item.id, error = %err, "poll: session lookup failed");
                    continue;
                }
            };
            let Some(repo_cfg) = self.config.repos.get(&item.repo).cloned() else {
                tracing::warn!(item = item.id, repo = %item.repo, "poll: repo no longer configured");
                continue;
            };
            let Some(runner) = self.runners.get(repo_cfg.agent_name()).cloned() else {
                continue;
            };
            let session_id = AgentSessionId(session);
            let probe = {
                let session_id = session_id.clone();
                blocking(move || runner.status(&session_id)).await
            };
            match probe {
                Ok(Ok(AgentStatus::Running)) => {
                    self.asked.remove(&item.id);
                }
                Ok(Ok(AgentStatus::WaitingInput { prompt })) => {
                    if self.asked.insert(item.id) {
                        let event = json!({ "prompt": prompt, "session": session_id.0 });
                        let recorded = self.storage().append_event(
                            Some(item.id),
                            "system",
                            "agent_question",
                            &event,
                        );
                        if let Err(err) = recorded {
                            tracing::warn!(item = item.id, error = %err, "recording agent_question failed");
                            self.asked.remove(&item.id);
                        }
                    }
                }
                Ok(Ok(AgentStatus::Finished {
                    success: true,
                    summary,
                })) => {
                    self.asked.remove(&item.id);
                    self.finish_item(&item, &repo_cfg, &summary).await;
                }
                Ok(Ok(AgentStatus::Finished {
                    success: false,
                    summary,
                })) => {
                    self.asked.remove(&item.id);
                    self.escalate(item.id, "agent_failed", json!({ "summary": summary }));
                }
                Ok(Ok(AgentStatus::Failed { message })) => {
                    self.asked.remove(&item.id);
                    self.escalate(item.id, "agent_failed", json!({ "message": message }));
                }
                Ok(Err(err)) => {
                    // Transient by construction of status(); retry next tick.
                    tracing::warn!(item = item.id, error = %err, "poll: agent status failed");
                }
                Err(err) => tracing::warn!(item = item.id, %err, "poll: status task failed"),
            }
        }
    }

    /// Agent succeeded: 🟢 local checks, 🟠 push, then the 🔴 create-pr gate.
    /// Any failure escalates with an audit event; the item stays `developing`
    /// while the gate is pending.
    async fn finish_item(&mut self, item: &WorkItemRow, repo_cfg: &Repo, summary: &str) {
        let (Some(branch), Some(worktree)) = (item.branch.clone(), item.worktree_path.clone())
        else {
            self.escalate(
                item.id,
                "pipeline_failed",
                json!({ "error": "work item has no branch/worktree recorded" }),
            );
            return;
        };
        let worktree = PathBuf::from(worktree);

        // 🟢 local checks.
        if !repo_cfg.check_command.is_empty() {
            let argv = repo_cfg.check_command.clone();
            let cwd = worktree.clone();
            let outcome = blocking(move || run_argv(&argv, &cwd)).await;
            match outcome {
                Ok(Ok(output)) if output.status.success() => {
                    let event = json!({ "command": repo_cfg.check_command });
                    if let Err(err) =
                        self.storage()
                            .append_event(Some(item.id), "system", "check_passed", &event)
                    {
                        tracing::warn!(item = item.id, error = %err, "recording check_passed failed");
                    }
                }
                Ok(Ok(output)) => {
                    self.escalate(
                        item.id,
                        "check_failed",
                        json!({
                            "command": repo_cfg.check_command,
                            "exit_code": output.status.code(),
                            "stderr_tail": tail_chars(
                                &String::from_utf8_lossy(&output.stderr),
                                STDERR_TAIL_CHARS,
                            ),
                        }),
                    );
                    return;
                }
                Ok(Err(err)) => {
                    self.escalate(
                        item.id,
                        "check_failed",
                        json!({ "command": repo_cfg.check_command, "error": err.to_string() }),
                    );
                    return;
                }
                Err(err) => {
                    self.escalate(item.id, "check_failed", json!({ "error": err }));
                    return;
                }
            }
        }

        // 🟠 push: automatic + notification (the audit event; OS notifications
        // land in M5). GatePolicy cannot downgrade this below RemotePrivate
        // in practice — per-item overrides are not wired yet.
        debug_assert_eq!(
            self.policy.level(GatedAction::PushBranch),
            yard_core::gate::GateLevel::RemotePrivate
        );
        let push_argv: Vec<String> = ["git", "push", "-u", "origin", branch.as_str()]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let cwd = worktree.clone();
        let outcome = blocking(move || run_argv(&push_argv, &cwd)).await;
        match outcome {
            Ok(Ok(output)) if output.status.success() => {
                let event = json!({ "branch": branch, "remote": "origin" });
                if let Err(err) =
                    self.storage()
                        .append_event(Some(item.id), "system", "branch_pushed", &event)
                {
                    tracing::warn!(item = item.id, error = %err, "recording branch_pushed failed");
                }
            }
            Ok(Ok(output)) => {
                self.escalate(
                    item.id,
                    "push_failed",
                    json!({
                        "branch": branch,
                        "exit_code": output.status.code(),
                        "stderr_tail": tail_chars(
                            &String::from_utf8_lossy(&output.stderr),
                            STDERR_TAIL_CHARS,
                        ),
                    }),
                );
                return;
            }
            Ok(Err(err)) => {
                self.escalate(
                    item.id,
                    "push_failed",
                    json!({ "branch": branch, "error": err.to_string() }),
                );
                return;
            }
            Err(err) => {
                self.escalate(item.id, "push_failed", json!({ "error": err }));
                return;
            }
        }

        // 🔴 create-pr gate. The remote is the source of truth for the ticket
        // title (SPEC §principle 5), so re-fetch instead of caching it.
        let ticket = match self.fetch_ticket(&item.provider, &item.ticket_key).await {
            Ok(ticket) => ticket,
            Err(err) => {
                self.escalate(item.id, "pr_draft_failed", json!({ "error": err }));
                return;
            }
        };
        let draft = match pr_draft_for(repo_cfg, &ticket, &branch, summary) {
            Ok(draft) => draft,
            Err(err) => {
                self.escalate(item.id, "pr_draft_failed", json!({ "error": err }));
                return;
            }
        };
        let payload = match serde_json::to_value(&draft) {
            Ok(payload) => payload,
            Err(err) => {
                self.escalate(
                    item.id,
                    "pr_draft_failed",
                    json!({ "error": err.to_string() }),
                );
                return;
            }
        };
        let requested = {
            let mut storage = self.storage();
            GateEngine::new(&mut storage).request(item.id, GatedAction::CreatePr, &payload)
        };
        match requested {
            // Item intentionally stays `developing`: PrPending begins when
            // the approved PR actually exists.
            Ok(gate_id) => {
                tracing::info!(item = item.id, gate_id, "create-pr gate pending approval");
            }
            Err(err) => {
                tracing::warn!(item = item.id, error = %err, "creating gate request failed");
            }
        }
    }

    // ------------------------------------------------------------ gates ----

    fn gates(&self) -> Result<GateList, String> {
        let storage = self.storage();
        let rows = storage
            .pending_gate_requests(None)
            .map_err(|e| e.to_string())?;
        let mut gates = Vec::with_capacity(rows.len());
        for row in rows {
            let item = storage
                .get_work_item(row.work_item_id)
                .map_err(|e| e.to_string())?;
            gates.push(GateInfo {
                id: row.id,
                work_item_id: row.work_item_id,
                ticket: item.ticket_key,
                repo: item.repo,
                action: row.action_kind,
                created_at: row.created_at,
                payload: row.payload,
            });
        }
        Ok(GateList { gates })
    }

    /// Approve: audit verdict first, then execute the gated action.
    /// A failing execution after approval escalates — the human said yes,
    /// the forge said no, a human must look.
    async fn approve(&mut self, gate_id: i64) -> Result<ApproveResult, String> {
        let gate = self
            .storage()
            .get_gate_request(gate_id)
            .map_err(|e| e.to_string())?;
        let action: GatedAction = gate
            .action_kind
            .parse()
            .map_err(|_| format!("gate {gate_id} has unknown action {:?}", gate.action_kind))?;
        if action != GatedAction::CreatePr {
            return Err(format!("no executor for gated action '{action}' yet"));
        }
        let draft: PrDraft = serde_json::from_value(gate.payload.clone())
            .map_err(|e| format!("gate {gate_id} payload is not a PR draft: {e}"))?;
        let item = self
            .storage()
            .get_work_item(gate.work_item_id)
            .map_err(|e| e.to_string())?;
        let forge = self.forge_for_repo(&item.repo)?;

        {
            let mut storage = self.storage();
            GateEngine::new(&mut storage)
                .approve(gate_id, "human")
                .map_err(|e| e.to_string())?;
        }
        let exec_draft = draft.clone();
        let created = blocking(move || forge.create_pr(&exec_draft)).await?;
        let pr = match created {
            Ok(pr) => pr,
            Err(err) => {
                let msg = format!("creating PR for {}: {err}", draft.repo);
                self.escalate(item.id, "pr_create_failed", json!({ "error": msg }));
                return Err(msg);
            }
        };
        let pr_ref = format!("{}#{}", pr.repo, pr.number);
        self.storage()
            .set_work_item_pr_ref(item.id, &pr_ref)
            .map_err(|e| e.to_string())?;
        self.update_state(item.id, WorkflowState::PrPending)?;
        Ok(ApproveResult {
            gate_id,
            work_item_id: item.id,
            action: action.as_str().to_owned(),
            pr: Some(pr_ref),
            state: WorkflowState::PrPending.as_str().to_owned(),
        })
    }

    fn reject(&mut self, gate_id: i64, reason: Option<&str>) -> Result<RejectResult, String> {
        let gate = self
            .storage()
            .get_gate_request(gate_id)
            .map_err(|e| e.to_string())?;
        {
            let mut storage = self.storage();
            GateEngine::new(&mut storage)
                .reject(gate_id, "human", reason.unwrap_or("rejected"))
                .map_err(|e| e.to_string())?;
        }
        self.update_state(gate.work_item_id, WorkflowState::Escalated)?;
        Ok(RejectResult {
            gate_id,
            work_item_id: gate.work_item_id,
            state: WorkflowState::Escalated.as_str().to_owned(),
        })
    }

    // ------------------------------------------------------------- logs ----

    fn logs(&self, item_id: i64, tail: usize) -> Result<LogsResult, String> {
        let (repo_name, session) = {
            let storage = self.storage();
            let item = storage.get_work_item(item_id).map_err(|e| e.to_string())?;
            let session = storage
                .work_item_agent_session(item_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("work item {item_id} has no agent session"))?;
            (item.repo, session)
        };
        let backend = self
            .config
            .repos
            .get(&repo_name)
            .map(Repo::agent_name)
            .unwrap_or("omp");
        let runner = self
            .runners
            .get(backend)
            .ok_or_else(|| format!("agent backend '{backend}' is not configured"))?;
        let path = runner
            .transcript(&AgentSessionId(session))
            .map_err(|e| e.to_string())?;
        let lines = match &path {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| format!("reading transcript {}: {e}", path.display()))?;
                tail_lines(&text, tail)
            }
            None => Vec::new(),
        };
        Ok(LogsResult {
            transcript_path: path.map(|p| p.display().to_string()),
            lines,
        })
    }

    // ---------------------------------------------------------- helpers ----

    async fn fetch_ticket(&self, provider_name: &str, ticket_key: &str) -> Result<Ticket, String> {
        let entry = self
            .providers
            .get(provider_name)
            .ok_or_else(|| format!("provider '{provider_name}' is not usable"))?;
        let provider = Arc::clone(&entry.provider);
        let key = TicketKey {
            provider: provider_name.to_owned(),
            key: ticket_key.to_owned(),
        };
        blocking(move || provider.get(&key))
            .await?
            .map_err(|e| format!("fetching ticket {ticket_key}: {e}"))
    }

    fn forge_for_repo(&self, repo_name: &str) -> Result<Arc<GithubForge>, String> {
        let repo_cfg = self
            .config
            .repos
            .get(repo_name)
            .ok_or_else(|| format!("repo '{repo_name}' is not configured"))?;
        let provider = repo_cfg
            .provider
            .as_deref()
            .ok_or_else(|| format!("repo '{repo_name}' has no provider configured"))?;
        self.providers
            .get(provider)
            .map(|entry| Arc::clone(&entry.forge))
            .ok_or_else(|| format!("provider '{provider}' is not usable"))
    }

    fn update_state(&self, item_id: i64, state: WorkflowState) -> Result<(), String> {
        self.storage()
            .update_work_item_state(item_id, state.as_str())
            .map_err(|e| e.to_string())
    }

    /// Appends an audit event and parks the item in `escalated`. Failures are
    /// logged, never propagated: escalation is already the error path.
    fn escalate(&self, item_id: i64, kind: &str, payload: Value) {
        let mut storage = self.storage();
        if let Err(err) = storage.append_event(Some(item_id), "system", kind, &payload) {
            tracing::warn!(item = item_id, kind, error = %err, "recording escalation event failed");
        }
        if let Err(err) = storage.update_work_item_state(item_id, WorkflowState::Escalated.as_str())
        {
            tracing::warn!(item = item_id, error = %err, "escalating work item failed");
        }
        tracing::warn!(item = item_id, kind, "work item escalated");
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested below)

/// Runs a closure on the blocking pool; the join error (panic/cancel) is
/// flattened into the command-level error string.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("blocking task failed: {e}"))
}

/// `<provider>:<key>` → (provider, key); a lone key is accepted when exactly
/// one provider is configured.
fn resolve_ticket_ref(config: &Config, ticket: &str) -> Result<(String, String), String> {
    if let Some((prefix, rest)) = ticket.split_once(':')
        && config.providers.contains_key(prefix)
    {
        return Ok((prefix.to_owned(), rest.to_owned()));
    }
    let mut names = config.providers.keys();
    match (names.next(), names.next()) {
        (Some(only), None) => Ok((only.clone(), ticket.to_owned())),
        _ => Err(format!(
            "ticket ref {ticket:?}: expected <provider>:<key> naming a configured provider \
             (configured: {})",
            config
                .providers
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// `--repo` name validated against config; a lone configured repo is the
/// default.
fn resolve_repo_name(config: &Config, explicit: Option<&str>) -> Result<String, String> {
    if let Some(name) = explicit {
        if config.repos.contains_key(name) {
            return Ok(name.to_owned());
        }
        return Err(format!("repo '{name}' is not configured"));
    }
    let mut names = config.repos.keys();
    match (names.next(), names.next()) {
        (Some(only), None) => Ok(only.clone()),
        (None, _) => Err("no repos configured; add a [repos.*] section".to_owned()),
        _ => Err(format!(
            "several repos configured; pass --repo (one of: {})",
            config.repos.keys().cloned().collect::<Vec<_>>().join(", ")
        )),
    }
}

/// Renders the repo's branch template: `{type}` from `--type` (default
/// `feature`), `{ticket}` = key with `/` and `#` folded to `-`, `{slug}` =
/// slugified title.
fn branch_for(
    repo: &Repo,
    ty: Option<&str>,
    ticket_key: &str,
    title: &str,
) -> Result<String, String> {
    let sanitized = ticket_key.replace(['/', '#'], "-");
    let slug = slugify(title);
    let vars = BTreeMap::from([
        ("type", ty.unwrap_or("feature")),
        ("ticket", sanitized.as_str()),
        ("slug", slug.as_str()),
    ]);
    render_template(&repo.branch_template, &vars).map_err(|e| e.to_string())
}

/// The full PR draft carried through the 🔴 gate. `Closes #N` is only valid
/// when the PR lands in the ticket's own repository.
fn pr_draft_for(
    repo: &Repo,
    ticket: &Ticket,
    branch: &str,
    summary: &str,
) -> Result<PrDraft, String> {
    let remote_repo = repo.remote_repo.clone().ok_or_else(|| {
        "repo has no remote_repo configured; set `remote_repo = \"owner/name\"`".to_owned()
    })?;
    let mut body = String::new();
    if let Some(number) = ticket.key.key.strip_prefix(&format!("{remote_repo}#")) {
        body.push_str(&format!("Closes #{number}.\n\n"));
    }
    if summary.is_empty() {
        body.push_str("(no agent summary)");
    } else {
        body.push_str(summary);
    }
    Ok(PrDraft {
        repo: remote_repo,
        title: format!("{}: {}", ticket.key.key, ticket.title),
        body,
        base: repo.base.clone(),
        head: branch.to_owned(),
        draft: true,
    })
}

/// Orchestrator-composed instructions handed to the agent backend.
fn compose_instructions(ticket: &Ticket, repo_name: &str, repo: &Repo, branch: &str) -> String {
    let mut out = format!(
        "# {key}: {title}\n\n{body}\n\n## Context\n\n\
         - Ticket: {url}\n\
         - Repository: {repo_name} (base branch `{base}`)\n\
         - You are working in a dedicated git worktree on branch `{branch}`.\n",
        key = ticket.key.key,
        title = ticket.title,
        body = ticket.body.trim(),
        url = ticket.url,
        base = repo.base,
    );
    if !repo.check_command.is_empty() {
        out.push_str(&format!(
            "- Local checks run after you finish: `{}`. Make them pass.\n",
            repo.check_command.join(" ")
        ));
    }
    out.push_str(
        "\n## Rules\n\n\
         - Commit your work locally with clear messages.\n\
         - Commit locally; NEVER push, open pull requests, or perform any \
         other remote action — the orchestrator handles everything remote.\n",
    );
    out
}

/// Duplicate `(provider, ticket, repo)` inserts surface as a UNIQUE
/// constraint failure; everything else passes through.
fn friendly_create_error(err: StorageError, ticket: &str, repo: &str) -> String {
    let msg = err.to_string();
    if msg.contains("UNIQUE constraint") {
        format!("ticket {ticket} is already tracked for repo '{repo}'")
    } else {
        msg
    }
}

fn run_argv(argv: &[String], cwd: &Path) -> std::io::Result<std::process::Output> {
    let (program, args) = argv.split_first().expect("run_argv called with empty argv");
    std::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
}

/// Last `n` lines, oldest first.
fn tail_lines(text: &str, n: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].iter().map(|s| (*s).to_owned()).collect()
}

/// Last `max_chars` characters (on a char boundary).
fn tail_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(max_chars)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml_src: &str) -> Config {
        toml::from_str(toml_src).expect("test config")
    }

    fn ticket(key: &str, title: &str) -> Ticket {
        Ticket {
            key: TicketKey {
                provider: "gh".into(),
                key: key.into(),
            },
            title: title.into(),
            body: "Please add the frobnicator.".into(),
            status: "open".into(),
            assignee: Some("me".into()),
            url: format!("https://example.test/{key}"),
            blocked_by: Vec::new(),
        }
    }

    const TWO_PROVIDERS: &str = r#"
        [providers.gh]
        kind = "github"
        user = "me"
        [providers.hub]
        kind = "github"
        user = "me"
    "#;

    #[test]
    fn ticket_ref_prefix_and_single_provider_fallback() {
        let two = config(TWO_PROVIDERS);
        assert_eq!(
            resolve_ticket_ref(&two, "gh:acme/app#7").unwrap(),
            ("gh".to_owned(), "acme/app#7".to_owned())
        );
        // Ambiguous without a prefix when several providers exist.
        let err = resolve_ticket_ref(&two, "acme/app#7").unwrap_err();
        assert!(err.contains("gh") && err.contains("hub"), "{err}");

        let one = config("[providers.gh]\nkind = \"github\"\nuser = \"me\"");
        assert_eq!(
            resolve_ticket_ref(&one, "acme/app#7").unwrap(),
            ("gh".to_owned(), "acme/app#7".to_owned())
        );
        // A colon inside the key must not be mistaken for an unknown prefix.
        assert_eq!(
            resolve_ticket_ref(&one, "weird:key").unwrap(),
            ("gh".to_owned(), "weird:key".to_owned())
        );
    }

    #[test]
    fn repo_resolution_validates_and_defaults() {
        let cfg = config(
            r#"
            [repos.app]
            path = "/opt/app"
            forge = "github"
            "#,
        );
        assert_eq!(resolve_repo_name(&cfg, None).unwrap(), "app");
        assert_eq!(resolve_repo_name(&cfg, Some("app")).unwrap(), "app");
        assert!(resolve_repo_name(&cfg, Some("ghost")).is_err());
        assert!(resolve_repo_name(&config(""), None).is_err());
    }

    #[test]
    fn branch_rendering_sanitizes_ticket_and_slugs_title() {
        let cfg = config(
            r#"
            [repos.app]
            path = "/opt/app"
            forge = "github"
            "#,
        );
        let branch = branch_for(
            &cfg.repos["app"],
            None,
            "acme/widgets#23",
            "Add frobnicator",
        )
        .unwrap();
        assert_eq!(branch, "feature/acme-widgets-23-add-frobnicator");
        let branch = branch_for(&cfg.repos["app"], Some("fix"), "acme/widgets#23", "Oops").unwrap();
        assert_eq!(branch, "fix/acme-widgets-23-oops");
    }

    #[test]
    fn pr_draft_closes_ticket_only_in_its_own_repo() {
        let cfg = config(
            r#"
            [repos.app]
            path = "/opt/app"
            forge = "github"
            remote_repo = "acme/widgets"
            base = "main"
            "#,
        );
        let repo = &cfg.repos["app"];
        let draft = pr_draft_for(
            repo,
            &ticket("acme/widgets#23", "Add frobnicator"),
            "feature/x",
            "did it",
        )
        .unwrap();
        assert_eq!(draft.repo, "acme/widgets");
        assert_eq!(draft.title, "acme/widgets#23: Add frobnicator");
        assert!(draft.body.starts_with("Closes #23.\n\n"), "{}", draft.body);
        assert!(draft.body.ends_with("did it"), "{}", draft.body);
        assert_eq!(draft.base, "main");
        assert_eq!(draft.head, "feature/x");
        assert!(draft.draft);

        // Cross-repo ticket: no bogus "Closes #N".
        let cross = pr_draft_for(
            repo,
            &ticket("acme/other#23", "Elsewhere"),
            "feature/x",
            "did it",
        )
        .unwrap();
        assert!(!cross.body.contains("Closes"), "{}", cross.body);

        // remote_repo is required.
        let bare = config(
            r#"
            [repos.app]
            path = "/opt/app"
            forge = "github"
            "#,
        );
        assert!(
            pr_draft_for(
                &bare.repos["app"],
                &ticket("acme/widgets#23", "T"),
                "b",
                "s"
            )
            .is_err()
        );
    }

    #[test]
    fn instructions_forbid_remote_actions() {
        let cfg = config(
            r#"
            [repos.app]
            path = "/opt/app"
            forge = "github"
            check_command = ["make", "test"]
            "#,
        );
        let text = compose_instructions(
            &ticket("acme/widgets#23", "Add frobnicator"),
            "app",
            &cfg.repos["app"],
            "feature/x",
        );
        assert!(text.contains("acme/widgets#23"), "{text}");
        assert!(text.contains("branch `feature/x`"), "{text}");
        assert!(text.contains("NEVER push"), "{text}");
        assert!(
            text.contains("the orchestrator handles everything remote"),
            "{text}"
        );
        assert!(text.contains("make test"), "{text}");
    }

    #[test]
    fn tails_keep_the_end() {
        assert_eq!(tail_lines("a\nb\nc\n", 2), vec!["b", "c"]);
        assert_eq!(tail_lines("a\nb", 10), vec!["a", "b"]);
        assert!(tail_lines("", 3).is_empty());
        assert_eq!(tail_chars("héllo", 3), "llo");
        assert_eq!(tail_chars("ab", 10), "ab");
    }
}
