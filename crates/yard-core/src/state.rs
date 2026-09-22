//! Workflow state machine (SPEC §Workflow) and the assignee hard invariant
//! (SPEC §principle 6): both live in core, never in providers.
//!
//! States are persisted by [`crate::storage`] as the snake_case strings
//! produced by [`WorkflowState::as_str`]; serde uses the same strings.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::adapters::Ticket;
use crate::storage::{Storage, StorageError};

/// Workflow state of a work item. See SPEC §Workflow state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowState {
    /// Known ticket, no work started.
    Tracked,
    /// Reserved: agent-assisted ticket refinement (phase 2).
    Groomed,
    /// Scheduled for an agent.
    Queued,
    /// Agent working in the worktree.
    Developing,
    /// PR draft awaiting the 🔴 create/edit gate.
    PrPending,
    /// PR exists; watching CI.
    CiWatch,
    /// CI green; waiting on reviewers.
    AwaitingReview,
    /// Agent addressing CI failures or review feedback.
    Addressing,
    /// Merge policy satisfied; waiting for the human keypress.
    Mergeable,
    /// 🔴 merge gate in flight.
    Merging,
    /// Merged on the forge; tracker wrap-up pending.
    Merged,
    /// Terminal.
    Done,
    /// Parked by a hard invariant or the operator; human unblocks.
    Blocked,
    /// Agent gave up (retry cap, budget); human takes over.
    Escalated,
}

impl WorkflowState {
    /// Every state, in graph order. Handy for exhaustive checks.
    pub const ALL: [WorkflowState; 14] = [
        WorkflowState::Tracked,
        WorkflowState::Groomed,
        WorkflowState::Queued,
        WorkflowState::Developing,
        WorkflowState::PrPending,
        WorkflowState::CiWatch,
        WorkflowState::AwaitingReview,
        WorkflowState::Addressing,
        WorkflowState::Mergeable,
        WorkflowState::Merging,
        WorkflowState::Merged,
        WorkflowState::Done,
        WorkflowState::Blocked,
        WorkflowState::Escalated,
    ];

    /// The exact string persisted in `work_items.state`.
    pub const fn as_str(self) -> &'static str {
        match self {
            WorkflowState::Tracked => "tracked",
            WorkflowState::Groomed => "groomed",
            WorkflowState::Queued => "queued",
            WorkflowState::Developing => "developing",
            WorkflowState::PrPending => "pr_pending",
            WorkflowState::CiWatch => "ci_watch",
            WorkflowState::AwaitingReview => "awaiting_review",
            WorkflowState::Addressing => "addressing",
            WorkflowState::Mergeable => "mergeable",
            WorkflowState::Merging => "merging",
            WorkflowState::Merged => "merged",
            WorkflowState::Done => "done",
            WorkflowState::Blocked => "blocked",
            WorkflowState::Escalated => "escalated",
        }
    }

    /// Terminal states never transition into `Blocked`/`Escalated`; `Merged`
    /// is "terminal-ish": its only remaining move is the tracker wrap-up to
    /// `Done`.
    pub const fn is_terminal(self) -> bool {
        matches!(self, WorkflowState::Merged | WorkflowState::Done)
    }
}

impl fmt::Display for WorkflowState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown workflow state: {0:?}")]
pub struct ParseWorkflowStateError(String);

impl FromStr for WorkflowState {
    type Err = ParseWorkflowStateError;

    fn from_str(s: &str) -> Result<WorkflowState, ParseWorkflowStateError> {
        WorkflowState::ALL
            .into_iter()
            .find(|state| state.as_str() == s)
            .ok_or_else(|| ParseWorkflowStateError(s.to_owned()))
    }
}

/// States a `Blocked`/`Escalated` item may be handed back to: any non-terminal
/// state — the human decides where work resumes (SPEC: "human unblocks or
/// hands work back"). The parked twin stays reachable; self-loops are not.
const RESUME_FROM_BLOCKED: &[WorkflowState] = &[
    WorkflowState::Tracked,
    WorkflowState::Groomed,
    WorkflowState::Queued,
    WorkflowState::Developing,
    WorkflowState::PrPending,
    WorkflowState::CiWatch,
    WorkflowState::AwaitingReview,
    WorkflowState::Addressing,
    WorkflowState::Mergeable,
    WorkflowState::Merging,
    WorkflowState::Escalated,
];

const RESUME_FROM_ESCALATED: &[WorkflowState] = &[
    WorkflowState::Tracked,
    WorkflowState::Groomed,
    WorkflowState::Queued,
    WorkflowState::Developing,
    WorkflowState::PrPending,
    WorkflowState::CiWatch,
    WorkflowState::AwaitingReview,
    WorkflowState::Addressing,
    WorkflowState::Mergeable,
    WorkflowState::Merging,
    WorkflowState::Blocked,
];

/// The SPEC §Workflow transition graph. Every non-terminal state may fall to
/// `Blocked`/`Escalated`; `Addressing` is the convergence point of the
/// PR-gate/CI-red/review-feedback fallbacks and loops back through
/// `Developing` (re-work → 🔴 push/edit gate) or straight to `Mergeable`
/// (reply-only resolution).
pub fn legal_transitions(from: WorkflowState) -> &'static [WorkflowState] {
    use WorkflowState::{
        Addressing, AwaitingReview, Blocked, CiWatch, Developing, Done, Escalated, Groomed,
        Mergeable, Merged, Merging, PrPending, Queued, Tracked,
    };
    match from {
        Tracked => &[Groomed, Queued, Blocked, Escalated],
        Groomed => &[Queued, Blocked, Escalated],
        Queued => &[Developing, Blocked, Escalated],
        Developing => &[PrPending, Blocked, Escalated],
        PrPending => &[CiWatch, Addressing, Blocked, Escalated],
        CiWatch => &[AwaitingReview, Addressing, Blocked, Escalated],
        AwaitingReview => &[Mergeable, Addressing, Blocked, Escalated],
        Addressing => &[Developing, Mergeable, Blocked, Escalated],
        Mergeable => &[Merging, Blocked, Escalated],
        Merging => &[Merged, Blocked, Escalated],
        Merged => &[Done],
        Done => &[],
        Blocked => RESUME_FROM_BLOCKED,
        Escalated => RESUME_FROM_ESCALATED,
    }
}

/// True iff `from → to` is an edge of the workflow graph. Self-loops are
/// never legal.
pub fn can_transition(from: WorkflowState, to: WorkflowState) -> bool {
    legal_transitions(from).contains(&to)
}

/// SPEC §principle 6 — never touch someone else's ticket.
///
/// `None` when the ticket is assigned to `me`; otherwise a human-readable
/// reason (unassigned, or assigned to someone else).
pub fn assignee_violation(ticket: &Ticket, me: &str) -> Option<String> {
    match ticket.assignee.as_deref() {
        Some(assignee) if assignee == me => None,
        Some(other) => Some(format!(
            "ticket {} is assigned to {other:?}, not {me:?}",
            ticket.key.key
        )),
        None => Some(format!("ticket {} is unassigned", ticket.key.key)),
    }
}

/// Enforces the assignee invariant on one work item; designed to run on every
/// sync, so it is idempotent: an item already `Blocked` (or terminal) is left
/// untouched and no duplicate events are appended.
///
/// On a fresh violation it appends an `assignee_violation` event and moves the
/// item to [`WorkflowState::Blocked`]. Returns whether this call blocked the
/// item.
pub fn enforce_assignee(
    storage: &mut Storage,
    work_item_id: i64,
    ticket: &Ticket,
    me: &str,
) -> Result<bool, StorageError> {
    let Some(reason) = assignee_violation(ticket, me) else {
        return Ok(false);
    };
    let item = storage.get_work_item(work_item_id)?;
    // Unparseable state strings fall through to blocking: fail safe.
    if let Ok(state) = WorkflowState::from_str(&item.state)
        && (state == WorkflowState::Blocked || state.is_terminal())
    {
        return Ok(false);
    }
    let payload = json!({
        "provider": ticket.key.provider,
        "ticket": ticket.key.key,
        "assignee": ticket.assignee,
        "me": me,
        "reason": reason,
    });
    storage.append_event(Some(work_item_id), "system", "assignee_violation", &payload)?;
    storage.update_work_item_state(work_item_id, WorkflowState::Blocked.as_str())?;
    tracing::warn!(
        work_item_id,
        reason,
        "assignee invariant violated; work item blocked"
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::TicketKey;

    fn ticket(assignee: Option<&str>) -> Ticket {
        Ticket {
            key: TicketKey {
                provider: "github".into(),
                key: "acme/app#7".into(),
            },
            title: "t".into(),
            body: String::new(),
            status: "open".into(),
            assignee: assignee.map(str::to_owned),
            url: "https://example.invalid".into(),
            blocked_by: Vec::new(),
        }
    }

    fn open_temp() -> (tempfile::TempDir, Storage) {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(dir.path()).expect("open");
        (dir, storage)
    }

    #[test]
    fn happy_path_is_legal() {
        use WorkflowState::*;
        let chain = [
            Tracked,
            Queued,
            Developing,
            PrPending,
            CiWatch,
            AwaitingReview,
            Mergeable,
            Merging,
            Merged,
            Done,
        ];
        for pair in chain.windows(2) {
            assert!(
                can_transition(pair[0], pair[1]),
                "{} -> {} must be legal",
                pair[0],
                pair[1]
            );
        }
        assert!(can_transition(Tracked, Groomed));
        assert!(can_transition(Groomed, Queued));
    }

    #[test]
    fn feedback_loops_are_legal() {
        use WorkflowState::*;
        for (from, to) in [
            (PrPending, Addressing),
            (CiWatch, Addressing),
            (AwaitingReview, Addressing),
            (Addressing, Developing),
            (Addressing, Mergeable),
        ] {
            assert!(can_transition(from, to), "{from} -> {to} must be legal");
        }
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        use WorkflowState::*;
        for (from, to) in [
            (Tracked, Developing),
            (Queued, Merged),
            (Developing, Merging),
            (Merged, Blocked),
            (Merged, Escalated),
            (Done, Tracked),
            (Done, Blocked),
            (Blocked, Done),
            (Escalated, Merged),
        ] {
            assert!(!can_transition(from, to), "{from} -> {to} must be illegal");
        }
        for state in WorkflowState::ALL {
            assert!(
                !can_transition(state, state),
                "{state} self-loop must be illegal"
            );
        }
    }

    #[test]
    fn blocked_and_escalated_reachable_from_any_non_terminal() {
        for from in WorkflowState::ALL {
            if from.is_terminal() {
                assert!(!can_transition(from, WorkflowState::Blocked));
                assert!(!can_transition(from, WorkflowState::Escalated));
                continue;
            }
            if from != WorkflowState::Blocked {
                assert!(
                    can_transition(from, WorkflowState::Blocked),
                    "{from} -> blocked"
                );
            }
            if from != WorkflowState::Escalated {
                assert!(
                    can_transition(from, WorkflowState::Escalated),
                    "{from} -> escalated"
                );
            }
        }
    }

    #[test]
    fn snake_case_round_trips_str_and_serde() {
        for state in WorkflowState::ALL {
            assert_eq!(state.as_str().parse::<WorkflowState>(), Ok(state));
            let value = serde_json::to_value(state).unwrap();
            assert_eq!(value, serde_json::Value::String(state.as_str().to_owned()));
            assert_eq!(
                serde_json::from_value::<WorkflowState>(value).unwrap(),
                state
            );
        }
        assert!("PrPending".parse::<WorkflowState>().is_err());
        assert!("".parse::<WorkflowState>().is_err());
    }

    #[test]
    fn storage_round_trips_state_strings() {
        let (_dir, mut storage) = open_temp();
        let id = storage
            .create_work_item(
                "github",
                "acme/app#7",
                "app",
                WorkflowState::Tracked.as_str(),
            )
            .unwrap();
        storage
            .update_work_item_state(id, WorkflowState::Developing.as_str())
            .unwrap();
        let row = storage.get_work_item(id).unwrap();
        assert_eq!(
            row.state.parse::<WorkflowState>(),
            Ok(WorkflowState::Developing)
        );
    }

    #[test]
    fn assignee_violation_cases() {
        assert_eq!(assignee_violation(&ticket(Some("me")), "me"), None);
        assert!(
            assignee_violation(&ticket(None), "me")
                .unwrap()
                .contains("unassigned")
        );
        assert!(
            assignee_violation(&ticket(Some("alice")), "me")
                .unwrap()
                .contains("alice")
        );
    }

    #[test]
    fn enforce_assignee_blocks_reassigned_item_once() {
        let (_dir, mut storage) = open_temp();
        let id = storage
            .create_work_item(
                "github",
                "acme/app#7",
                "app",
                WorkflowState::Developing.as_str(),
            )
            .unwrap();

        // Assigned to me: no-op on every sync.
        assert!(!enforce_assignee(&mut storage, id, &ticket(Some("me")), "me").unwrap());
        assert!(storage.events(Some(id)).unwrap().is_empty());

        // Reassigned mid-flight: block + audit trail.
        assert!(enforce_assignee(&mut storage, id, &ticket(Some("alice")), "me").unwrap());
        let row = storage.get_work_item(id).unwrap();
        assert_eq!(row.state, WorkflowState::Blocked.as_str());
        let events = storage.events(Some(id)).unwrap();
        let violation = events
            .iter()
            .find(|e| e.kind == "assignee_violation")
            .expect("assignee_violation event");
        assert_eq!(violation.payload["assignee"], "alice");
        assert_eq!(violation.payload["me"], "me");
        assert!(events.iter().any(|e| e.kind == "state_changed"));

        // Next sync: already blocked, idempotent — no duplicate events.
        let count = events.len();
        assert!(!enforce_assignee(&mut storage, id, &ticket(Some("alice")), "me").unwrap());
        assert_eq!(storage.events(Some(id)).unwrap().len(), count);
    }

    #[test]
    fn enforce_assignee_blocks_unassigned_but_skips_terminal() {
        let (_dir, mut storage) = open_temp();
        let unassigned = storage
            .create_work_item(
                "github",
                "acme/app#1",
                "app",
                WorkflowState::Queued.as_str(),
            )
            .unwrap();
        assert!(enforce_assignee(&mut storage, unassigned, &ticket(None), "me").unwrap());
        assert_eq!(
            storage.get_work_item(unassigned).unwrap().state,
            WorkflowState::Blocked.as_str()
        );

        let done = storage
            .create_work_item("github", "acme/app#2", "app", WorkflowState::Done.as_str())
            .unwrap();
        assert!(!enforce_assignee(&mut storage, done, &ticket(Some("alice")), "me").unwrap());
        assert_eq!(
            storage.get_work_item(done).unwrap().state,
            WorkflowState::Done.as_str()
        );
        assert!(storage.events(Some(done)).unwrap().is_empty());
    }

    #[test]
    fn enforce_assignee_missing_item_is_not_found() {
        let (_dir, mut storage) = open_temp();
        assert!(matches!(
            enforce_assignee(&mut storage, 999, &ticket(None), "me"),
            Err(StorageError::WorkItemNotFound(999))
        ));
    }
}
