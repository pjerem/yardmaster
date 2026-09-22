//! Adapter contracts: everything yardmaster talks to is one of these traits.
//!
//! Design decisions (SPEC.md §Adapter traits):
//! - Traits are **synchronous** and dyn-compatible; implementations may block
//!   (HTTP via `reqwest::blocking`). The daemon calls them through
//!   `tokio::task::spawn_blocking`. Boring beats async-trait gymnastics.
//! - Adapters NEVER decide anything gate-related: core approves an action
//!   first, adapters only execute already-approved payloads (🔴 markers below
//!   document which calls core must gate).
//! - Errors are `AdapterError`: retryable-ness is the one thing the scheduler
//!   needs to know; provider-specific detail stays in the message.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Error surface common to all adapters.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    /// Transient: network, 5xx, rate limit. The scheduler may retry with backoff.
    #[error("transient adapter error: {0}")]
    Transient(String),
    /// Permanent: 4xx, auth, not found, invalid payload. Escalate, do not retry.
    #[error("permanent adapter error: {0}")]
    Permanent(String),
}

pub type AdapterResult<T> = Result<T, AdapterError>;

// ---------------------------------------------------------------------------
// Tickets

/// Identifies a ticket within a configured provider (`provider` = config name).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TicketKey {
    pub provider: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ticket {
    pub key: TicketKey,
    pub title: String,
    pub body: String,
    pub status: String,
    /// Provider-side login of the assignee. `None` = unassigned.
    pub assignee: Option<String>,
    pub url: String,
    /// Tickets this one is blocked by (provider-declared, e.g. Jira "blocks").
    pub blocked_by: Vec<TicketKey>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackerTransition {
    pub id: String,
    pub name: String,
}

pub trait TicketProvider: Send + Sync {
    /// Tickets assigned to the configured user. Core re-checks the assignee
    /// invariant regardless — this is a convenience filter, not the guard.
    fn my_tickets(&self) -> AdapterResult<Vec<Ticket>>;
    fn get(&self, key: &TicketKey) -> AdapterResult<Ticket>;
    fn available_transitions(&self, key: &TicketKey) -> AdapterResult<Vec<TrackerTransition>>;
    /// 🔴 public action — core must hold an approved gate before calling.
    fn apply_transition(
        &self,
        key: &TicketKey,
        transition: &TrackerTransition,
    ) -> AdapterResult<()>;
}

// ---------------------------------------------------------------------------
// Agents

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentSessionId(pub String);

/// Everything a backend needs to start working on a ticket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTask {
    pub work_item_id: i64,
    pub worktree: PathBuf,
    pub ticket: Ticket,
    /// Orchestrator-composed instructions (conventions, scope, definition of done).
    pub instructions: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AgentStatus {
    Running,
    /// Agent asked a question / needs human input to continue.
    WaitingInput {
        prompt: String,
    },
    Finished {
        success: bool,
        summary: String,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
}

pub trait AgentRunner: Send + Sync {
    fn start(&self, task: &AgentTask) -> AdapterResult<AgentSessionId>;
    /// Feed a follow-up message (review feedback, answer to a question).
    fn resume(&self, id: &AgentSessionId, message: &str) -> AdapterResult<()>;
    fn status(&self, id: &AgentSessionId) -> AdapterResult<AgentStatus>;
    fn cancel(&self, id: &AgentSessionId) -> AdapterResult<()>;
    fn usage(&self, id: &AgentSessionId) -> AdapterResult<TokenUsage>;
    /// Path of the session transcript (for `yard logs` / TUI), if available.
    fn transcript(&self, id: &AgentSessionId) -> AdapterResult<Option<PathBuf>>;
}

// ---------------------------------------------------------------------------
// Forge

/// `owner/repo` plus the PR number once it exists.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PrRef {
    pub repo: String,
    pub number: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrDraft {
    pub repo: String,
    pub title: String,
    pub body: String,
    pub base: String,
    pub head: String,
    pub draft: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckRun {
    pub name: String,
    /// queued | in_progress | completed
    pub status: String,
    /// success | failure | cancelled | … (empty while not completed)
    pub conclusion: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Review {
    pub author: String,
    /// APPROVED | CHANGES_REQUESTED | COMMENTED
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrComment {
    pub id: u64,
    pub author: String,
    pub body: String,
    pub is_bot: bool,
    pub created_at: String,
}

/// One consistent snapshot consumed by the merge policy and the feedback loops.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrSnapshot {
    pub pr: PrRef,
    /// open | closed | merged
    pub state: String,
    pub draft: bool,
    pub base: String,
    pub head_sha: String,
    /// None = forge still computing.
    pub mergeable: Option<bool>,
    pub checks: Vec<CheckRun>,
    pub reviews: Vec<Review>,
    pub comments: Vec<PrComment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

pub trait Forge: Send + Sync {
    /// 🔴 public action — core must hold an approved gate before calling.
    fn create_pr(&self, draft: &PrDraft) -> AdapterResult<PrRef>;
    fn pr_snapshot(&self, pr: &PrRef) -> AdapterResult<PrSnapshot>;
    /// 🔴 public action.
    fn post_comment(&self, pr: &PrRef, body: &str) -> AdapterResult<()>;
    /// 🔴 public action.
    fn merge(&self, pr: &PrRef, method: MergeMethod) -> AdapterResult<()>;
}

// ---------------------------------------------------------------------------
// Notifications & secrets

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub title: String,
    pub body: String,
    /// Work item the notification points at, when applicable.
    pub work_item_id: Option<i64>,
}

pub trait Notifier: Send + Sync {
    fn notify(&self, notification: &Notification) -> AdapterResult<()>;
}

/// Opaque secret wrapper: Debug/Display never leak the value.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Secret(value)
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> AdapterResult<Option<Secret>>;
    fn set(&self, key: &str, value: Secret) -> AdapterResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_never_leaks_in_debug() {
        let s = Secret::new("hunter2".into());
        assert_eq!(format!("{s:?}"), "Secret(***)");
        assert_eq!(s.expose(), "hunter2");
    }

    #[test]
    fn ticket_key_round_trips() {
        let k = TicketKey {
            provider: "gh".into(),
            key: "pjerem/yardmaster#23".into(),
        };
        let json = serde_json::to_string(&k).unwrap();
        assert_eq!(serde_json::from_str::<TicketKey>(&json).unwrap(), k);
    }
}
