//! Gate engine (SPEC §principle 4): every side effect is classified
//! 🟢 local / 🟠 remote-private / 🔴 public, and 🔴 actions require explicit
//! human approval, every single time.
//!
//! Core gates *around* adapters: adapters only execute already-approved
//! payloads. Requests and verdicts are persisted through [`crate::storage`]
//! together with their audit events.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::storage::{GateRequestRow, Storage, StorageError};

/// Blast radius of a gated action. Ordering is severity: `Local` <
/// `RemotePrivate` < `Public`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateLevel {
    /// 🟢 worktree edits, local commits, tests — automatic.
    Local,
    /// 🟠 push to a feature branch — automatic + notification.
    RemotePrivate,
    /// 🔴 publicly visible — explicit human approval, never remembered.
    Public,
}

impl GateLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            GateLevel::Local => "local",
            GateLevel::RemotePrivate => "remote_private",
            GateLevel::Public => "public",
        }
    }

    /// What the scheduler does with an action at this level.
    pub const fn decision(self) -> Decision {
        match self {
            GateLevel::Local => Decision::Auto,
            GateLevel::RemotePrivate => Decision::AutoNotify,
            GateLevel::Public => Decision::RequireApproval,
        }
    }
}

impl fmt::Display for GateLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Scheduler verdict derived from a [`GateLevel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Auto,
    AutoNotify,
    RequireApproval,
}

/// Side effects that pass through the gate engine. Persisted in
/// `gate_requests.action_kind` as the snake_case strings from
/// [`GatedAction::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatedAction {
    PushBranch,
    ForcePushBranch,
    CreatePr,
    EditPr,
    PostComment,
    ReplyReview,
    Merge,
    TrackerTransition,
}

impl GatedAction {
    pub const ALL: [GatedAction; 8] = [
        GatedAction::PushBranch,
        GatedAction::ForcePushBranch,
        GatedAction::CreatePr,
        GatedAction::EditPr,
        GatedAction::PostComment,
        GatedAction::ReplyReview,
        GatedAction::Merge,
        GatedAction::TrackerTransition,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            GatedAction::PushBranch => "push_branch",
            GatedAction::ForcePushBranch => "force_push_branch",
            GatedAction::CreatePr => "create_pr",
            GatedAction::EditPr => "edit_pr",
            GatedAction::PostComment => "post_comment",
            GatedAction::ReplyReview => "reply_review",
            GatedAction::Merge => "merge",
            GatedAction::TrackerTransition => "tracker_transition",
        }
    }

    /// Default classification per SPEC §principle 4: branch pushes are 🟠,
    /// everything publicly visible is 🔴. (🟢 actions never reach the gate
    /// engine, hence no `Local` default here.)
    pub const fn default_level(self) -> GateLevel {
        match self {
            GatedAction::PushBranch | GatedAction::ForcePushBranch => GateLevel::RemotePrivate,
            GatedAction::CreatePr
            | GatedAction::EditPr
            | GatedAction::PostComment
            | GatedAction::ReplyReview
            | GatedAction::Merge
            | GatedAction::TrackerTransition => GateLevel::Public,
        }
    }
}

impl fmt::Display for GatedAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown gated action: {0:?}")]
pub struct ParseGatedActionError(String);

impl FromStr for GatedAction {
    type Err = ParseGatedActionError;

    fn from_str(s: &str) -> Result<GatedAction, ParseGatedActionError> {
        GatedAction::ALL
            .into_iter()
            .find(|action| action.as_str() == s)
            .ok_or_else(|| ParseGatedActionError(s.to_owned()))
    }
}

/// Per-work-item gate levels: defaults plus overrides passed as data (config
/// parsing happens elsewhere).
///
/// INVARIANT: when `lock_public_gate` is true, no override may downgrade a
/// default-🔴 action below `Public` — such overrides are clamped back to
/// `Public` with a warning. Upgrades are always honored.
#[derive(Debug, Clone, Default)]
pub struct GatePolicy {
    overrides: HashMap<GatedAction, GateLevel>,
}

impl GatePolicy {
    pub fn new(
        lock_public_gate: bool,
        overrides: impl IntoIterator<Item = (GatedAction, GateLevel)>,
    ) -> GatePolicy {
        let overrides = overrides
            .into_iter()
            .map(|(action, level)| {
                if lock_public_gate
                    && action.default_level() == GateLevel::Public
                    && level < GateLevel::Public
                {
                    tracing::warn!(
                        action = action.as_str(),
                        requested = level.as_str(),
                        "lock_public_gate: override may not downgrade a public action; clamped"
                    );
                    (action, GateLevel::Public)
                } else {
                    (action, level)
                }
            })
            .collect();
        GatePolicy { overrides }
    }

    pub fn level(&self, action: GatedAction) -> GateLevel {
        self.overrides
            .get(&action)
            .copied()
            .unwrap_or_else(|| action.default_level())
    }

    pub fn decision(&self, action: GatedAction) -> Decision {
        self.level(action).decision()
    }
}

/// Typed façade over the persisted gate queue. Requests and verdicts commit
/// atomically with their audit events (`gate_requested`, `gate_approved`,
/// `gate_rejected`).
pub struct GateEngine<'a> {
    storage: &'a mut Storage,
}

impl<'a> GateEngine<'a> {
    pub fn new(storage: &'a mut Storage) -> GateEngine<'a> {
        GateEngine { storage }
    }

    /// Queues `action` for approval. `payload` is the exact side effect to
    /// execute once approved (e.g. full PR body). Returns the gate request id.
    pub fn request(
        &mut self,
        work_item_id: i64,
        action: GatedAction,
        payload: &Value,
    ) -> Result<i64, StorageError> {
        self.storage
            .create_gate_request(work_item_id, action.as_str(), payload)
    }

    /// Approves a pending request. Double-resolve is an error.
    pub fn approve(&mut self, id: i64, actor: &str) -> Result<(), StorageError> {
        self.storage
            .resolve_gate_request(id, "approved", actor, None)
    }

    /// Rejects a pending request with a reason. Double-resolve is an error.
    pub fn reject(&mut self, id: i64, actor: &str, reason: &str) -> Result<(), StorageError> {
        self.storage
            .resolve_gate_request(id, "rejected", actor, Some(reason))
    }

    /// Pending requests, oldest first; `Some(id)` filters to one work item.
    pub fn pending(&self, work_item_id: Option<i64>) -> Result<Vec<GateRequestRow>, StorageError> {
        self.storage.pending_gate_requests(work_item_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn open_temp() -> (tempfile::TempDir, Storage) {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(dir.path()).expect("open");
        (dir, storage)
    }

    fn work_item(storage: &Storage, key: &str) -> i64 {
        storage
            .create_work_item("github", key, "app", "developing")
            .expect("create work item")
    }

    #[test]
    fn default_classification_and_decisions() {
        let policy = GatePolicy::default();
        for action in [GatedAction::PushBranch, GatedAction::ForcePushBranch] {
            assert_eq!(policy.level(action), GateLevel::RemotePrivate);
            assert_eq!(policy.decision(action), Decision::AutoNotify);
        }
        for action in [
            GatedAction::CreatePr,
            GatedAction::EditPr,
            GatedAction::PostComment,
            GatedAction::ReplyReview,
            GatedAction::Merge,
            GatedAction::TrackerTransition,
        ] {
            assert_eq!(policy.level(action), GateLevel::Public);
            assert_eq!(policy.decision(action), Decision::RequireApproval);
        }
        assert_eq!(GateLevel::Local.decision(), Decision::Auto);
    }

    #[test]
    fn action_strings_round_trip() {
        for action in GatedAction::ALL {
            assert_eq!(action.as_str().parse::<GatedAction>(), Ok(action));
            let value = serde_json::to_value(action).unwrap();
            assert_eq!(value, serde_json::Value::String(action.as_str().to_owned()));
            assert_eq!(
                serde_json::from_value::<GatedAction>(value).unwrap(),
                action
            );
        }
        assert!("CreatePr".parse::<GatedAction>().is_err());
    }

    #[test]
    fn unlocked_policy_honors_public_downgrade() {
        let policy = GatePolicy::new(false, [(GatedAction::Merge, GateLevel::RemotePrivate)]);
        assert_eq!(policy.level(GatedAction::Merge), GateLevel::RemotePrivate);
        assert_eq!(policy.decision(GatedAction::Merge), Decision::AutoNotify);
        // Untouched actions keep their defaults.
        assert_eq!(policy.level(GatedAction::CreatePr), GateLevel::Public);
    }

    #[test]
    fn locked_policy_clamps_every_public_downgrade() {
        let policy = GatePolicy::new(
            true,
            [
                (GatedAction::Merge, GateLevel::Local),
                (GatedAction::CreatePr, GateLevel::RemotePrivate),
                (GatedAction::PostComment, GateLevel::Public), // no-op override
                (GatedAction::PushBranch, GateLevel::Local),   // 🟠 default: downgrade allowed
                (GatedAction::ForcePushBranch, GateLevel::Public), // upgrade always allowed
            ],
        );
        assert_eq!(policy.level(GatedAction::Merge), GateLevel::Public);
        assert_eq!(
            policy.decision(GatedAction::Merge),
            Decision::RequireApproval
        );
        assert_eq!(policy.level(GatedAction::CreatePr), GateLevel::Public);
        assert_eq!(policy.level(GatedAction::PostComment), GateLevel::Public);
        assert_eq!(policy.level(GatedAction::PushBranch), GateLevel::Local);
        assert_eq!(
            policy.level(GatedAction::ForcePushBranch),
            GateLevel::Public
        );
    }

    #[test]
    fn request_persists_payload_and_event() {
        let (_dir, mut storage) = open_temp();
        let item = work_item(&storage, "acme/app#1");
        let payload = json!({"title": "feat: x", "body": "long body"});
        let gate_id = GateEngine::new(&mut storage)
            .request(item, GatedAction::CreatePr, &payload)
            .unwrap();

        let pending = storage.pending_gate_requests(Some(item)).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, gate_id);
        assert_eq!(pending[0].action_kind, "create_pr");
        assert_eq!(pending[0].payload, payload);
        assert_eq!(pending[0].status, "pending");
        assert_eq!(pending[0].resolved_by, None);

        let events = storage.events(Some(item)).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "gate_requested");
        assert_eq!(
            events[0].payload,
            json!({"gate_id": gate_id, "action": "create_pr"})
        );
    }

    #[test]
    fn request_for_missing_work_item_fails() {
        let (_dir, mut storage) = open_temp();
        assert!(matches!(
            GateEngine::new(&mut storage).request(42, GatedAction::Merge, &json!({})),
            Err(StorageError::WorkItemNotFound(42))
        ));
        assert!(storage.events(None).unwrap().is_empty());
    }

    #[test]
    fn approve_lifecycle_records_verdict_and_event() {
        let (_dir, mut storage) = open_temp();
        let item = work_item(&storage, "acme/app#1");
        let mut engine = GateEngine::new(&mut storage);
        let gate_id = engine
            .request(item, GatedAction::Merge, &json!({"method": "squash"}))
            .unwrap();
        engine.approve(gate_id, "jeremy").unwrap();
        assert!(engine.pending(None).unwrap().is_empty());

        let row = storage.get_gate_request(gate_id).unwrap();
        assert_eq!(row.status, "approved");
        assert_eq!(row.resolved_by.as_deref(), Some("jeremy"));
        assert!(row.resolved_at.is_some());

        let events = storage.events(Some(item)).unwrap();
        let approved = events
            .iter()
            .find(|e| e.kind == "gate_approved")
            .expect("event");
        assert_eq!(approved.actor, "jeremy");
        assert_eq!(
            approved.payload,
            json!({"gate_id": gate_id, "action": "merge"})
        );
    }

    #[test]
    fn reject_records_reason() {
        let (_dir, mut storage) = open_temp();
        let item = work_item(&storage, "acme/app#1");
        let mut engine = GateEngine::new(&mut storage);
        let gate_id = engine
            .request(item, GatedAction::PostComment, &json!({"body": "hi"}))
            .unwrap();
        engine.reject(gate_id, "jeremy", "tone is off").unwrap();

        let row = storage.get_gate_request(gate_id).unwrap();
        assert_eq!(row.status, "rejected");
        let events = storage.events(Some(item)).unwrap();
        let rejected = events
            .iter()
            .find(|e| e.kind == "gate_rejected")
            .expect("event");
        assert_eq!(
            rejected.payload,
            json!({"gate_id": gate_id, "action": "post_comment", "reason": "tone is off"})
        );
    }

    #[test]
    fn double_resolve_is_rejected() {
        let (_dir, mut storage) = open_temp();
        let item = work_item(&storage, "acme/app#1");
        let mut engine = GateEngine::new(&mut storage);
        let gate_id = engine
            .request(item, GatedAction::Merge, &json!({}))
            .unwrap();
        engine.approve(gate_id, "jeremy").unwrap();

        for result in [
            engine.approve(gate_id, "jeremy"),
            engine.reject(gate_id, "jeremy", "no"),
        ] {
            match result {
                Err(StorageError::GateRequestNotPending { id, ref status }) => {
                    assert_eq!(id, gate_id);
                    assert_eq!(status, "approved");
                }
                other => panic!("expected GateRequestNotPending, got {other:?}"),
            }
        }
        assert!(matches!(
            engine.approve(999, "jeremy"),
            Err(StorageError::GateRequestNotFound(999))
        ));
        // Exactly one resolution event survived.
        let events = storage.events(Some(item)).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind.starts_with("gate_"))
                .count(),
            2
        );
    }

    #[test]
    fn pending_filters_by_work_item() {
        let (_dir, mut storage) = open_temp();
        let a = work_item(&storage, "acme/app#1");
        let b = work_item(&storage, "acme/app#2");
        let mut engine = GateEngine::new(&mut storage);
        let first = engine
            .request(a, GatedAction::CreatePr, &json!({}))
            .unwrap();
        let second = engine.request(b, GatedAction::Merge, &json!({})).unwrap();
        engine
            .request(a, GatedAction::PostComment, &json!({}))
            .unwrap();
        engine.reject(first, "jeremy", "redo").unwrap();

        let all: Vec<i64> = engine.pending(None).unwrap().iter().map(|r| r.id).collect();
        assert_eq!(all.len(), 2);
        let for_a = engine.pending(Some(a)).unwrap();
        assert_eq!(for_a.len(), 1);
        assert_eq!(for_a[0].action_kind, "post_comment");
        let for_b = engine.pending(Some(b)).unwrap();
        assert_eq!(for_b.len(), 1);
        assert_eq!(for_b[0].id, second);
    }
}
