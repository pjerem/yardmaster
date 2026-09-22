//! SQLite storage: work-item projection + append-only event log.
//!
//! The event log is append-only *by construction*: this module exposes no
//! UPDATE or DELETE path for `events`. Migrations are keyed on
//! `PRAGMA user_version`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;
use thiserror::Error;

/// Database file name inside the state directory (see `paths::db_path`).
const DB_FILE: &str = "yardmaster.db";

/// One SQL batch per schema version; `user_version` = number of applied
/// migrations. Append new batches, never edit shipped ones.
const MIGRATIONS: &[&str] = &[
    // v1: work-item projection, append-only event log, gate queue.
    "CREATE TABLE work_items (
        id INTEGER PRIMARY KEY,
        provider TEXT NOT NULL,
        ticket_key TEXT NOT NULL,
        repo TEXT NOT NULL,
        state TEXT NOT NULL,
        branch TEXT,
        worktree_path TEXT,
        pr_ref TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        UNIQUE(provider, ticket_key, repo)
    );
    CREATE TABLE events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        work_item_id INTEGER REFERENCES work_items(id),
        ts TEXT NOT NULL,
        actor TEXT NOT NULL,
        kind TEXT NOT NULL,
        payload TEXT NOT NULL
    );
    CREATE INDEX idx_events_work_item ON events(work_item_id);
    CREATE TABLE gate_requests (
        id INTEGER PRIMARY KEY,
        work_item_id INTEGER NOT NULL REFERENCES work_items(id),
        action_kind TEXT NOT NULL,
        payload TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'pending',
        created_at TEXT NOT NULL,
        resolved_at TEXT,
        resolved_by TEXT
    );
    CREATE INDEX idx_gate_requests_pending ON gate_requests(work_item_id, status);",
    // v2: agent session handle per work item (M2 agent supervision). The
    // session id is the ProcessRunner directory name under the daemon's
    // sessions root; NULL = no agent started yet.
    "ALTER TABLE work_items ADD COLUMN agent_session TEXT;",
];

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("failed to create state directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database schema version {found} is newer than supported version {supported}")]
    UnsupportedVersion { found: i64, supported: i64 },
    #[error("work item {0} not found")]
    WorkItemNotFound(i64),
    #[error("gate request {0} not found")]
    GateRequestNotFound(i64),
    #[error("gate request {id} is not pending (status: {status}); double-resolve rejected")]
    GateRequestNotPending { id: i64, status: String },
}

/// Mirrors the `work_items` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItemRow {
    pub id: i64,
    pub provider: String,
    pub ticket_key: String,
    pub repo: String,
    pub state: String,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub pr_ref: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Mirrors the `events` table; `payload` is the parsed JSON column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    pub id: i64,
    pub work_item_id: Option<i64>,
    pub ts: String,
    pub actor: String,
    pub kind: String,
    pub payload: Value,
}

/// Mirrors the `gate_requests` table; `payload` is the parsed JSON column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateRequestRow {
    pub id: i64,
    pub work_item_id: i64,
    pub action_kind: String,
    pub payload: Value,
    pub status: String,
    pub created_at: String,
    pub resolved_at: Option<String>,
    pub resolved_by: Option<String>,
}

pub struct Storage {
    conn: Connection,
}

impl Storage {
    /// Creates `state_dir` if needed, opens `{state_dir}/yardmaster.db` in
    /// WAL mode with foreign keys enforced, and applies pending migrations.
    pub fn open(state_dir: &Path) -> Result<Storage, StorageError> {
        fs::create_dir_all(state_dir).map_err(|source| StorageError::CreateDir {
            path: state_dir.to_path_buf(),
            source,
        })?;
        let mut conn = Connection::open(state_dir.join(DB_FILE))?;
        // `PRAGMA journal_mode` returns the resulting mode as a row, so it
        // must go through query_row rather than pragma_update.
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            tracing::warn!(mode, "sqlite refused WAL journal mode");
        }
        conn.pragma_update(None, "foreign_keys", true)?;
        migrate(&mut conn)?;
        Ok(Storage { conn })
    }

    /// Appends one event. Returns the event id. `work_item_id = None` records
    /// a daemon-level event not tied to any work item.
    pub fn append_event(
        &self,
        work_item_id: Option<i64>,
        actor: &str,
        kind: &str,
        payload: &Value,
    ) -> Result<i64, StorageError> {
        Ok(insert_event(
            &self.conn,
            work_item_id,
            actor,
            kind,
            payload,
            &now_rfc3339(),
        )?)
    }

    /// Events in insertion order; `Some(id)` filters to one work item.
    pub fn events(&self, work_item_id: Option<i64>) -> Result<Vec<EventRow>, StorageError> {
        const BASE: &str = "SELECT id, work_item_id, ts, actor, kind, payload FROM events";
        let rows = match work_item_id {
            Some(id) => {
                let mut stmt = self
                    .conn
                    .prepare(&format!("{BASE} WHERE work_item_id = ?1 ORDER BY id"))?;
                let rows = stmt.query_map([id], event_from_row)?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
            None => {
                let mut stmt = self.conn.prepare(&format!("{BASE} ORDER BY id"))?;
                let rows = stmt.query_map([], event_from_row)?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
        };
        Ok(rows)
    }

    pub fn list_work_items(&self) -> Result<Vec<WorkItemRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, provider, ticket_key, repo, state, branch, worktree_path, pr_ref,
                    created_at, updated_at
             FROM work_items ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row: &Row| {
            Ok(WorkItemRow {
                id: row.get(0)?,
                provider: row.get(1)?,
                ticket_key: row.get(2)?,
                repo: row.get(3)?,
                state: row.get(4)?,
                branch: row.get(5)?,
                worktree_path: row.get(6)?,
                pr_ref: row.get(7)?,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn count_pending_gates(&self, work_item_id: i64) -> Result<u32, StorageError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM gate_requests WHERE work_item_id = ?1 AND status = 'pending'",
            [work_item_id],
            |r| r.get(0),
        )?)
    }

    /// Inserts a new work item. `(provider, ticket_key, repo)` is UNIQUE;
    /// duplicates surface as a sqlite constraint error.
    pub fn create_work_item(
        &self,
        provider: &str,
        ticket_key: &str,
        repo: &str,
        state: &str,
    ) -> Result<i64, StorageError> {
        let now = now_rfc3339();
        self.conn.execute(
            "INSERT INTO work_items (provider, ticket_key, repo, state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![provider, ticket_key, repo, state, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Updates a work item's state and appends a `state_changed` event
    /// (payload `{"old", "new"}`) in the same transaction.
    pub fn update_work_item_state(&mut self, id: i64, new_state: &str) -> Result<(), StorageError> {
        let tx = self.conn.transaction()?;
        let old: Option<String> = tx
            .query_row("SELECT state FROM work_items WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        let now = now_rfc3339();
        tx.execute(
            "UPDATE work_items SET state = ?1, updated_at = ?2 WHERE id = ?3",
            params![new_state, now, id],
        )?;
        let payload = serde_json::json!({ "old": old, "new": new_state });
        // Existence is enforced by the events FK inside the transaction: for a
        // missing item the insert hits a FOREIGN KEY constraint failure, the
        // whole transaction rolls back when `tx` drops, and we surface the
        // typed not-found error instead.
        if let Err(e) = insert_event(&tx, Some(id), "system", "state_changed", &payload, &now) {
            return Err(match old {
                None => StorageError::WorkItemNotFound(id),
                Some(_) => e.into(),
            });
        }
        tx.commit()?;
        Ok(())
    }

    /// Fetches one work item by id.
    pub fn get_work_item(&self, id: i64) -> Result<WorkItemRow, StorageError> {
        self.conn
            .query_row(
                "SELECT id, provider, ticket_key, repo, state, branch, worktree_path, pr_ref,
                        created_at, updated_at
                 FROM work_items WHERE id = ?1",
                [id],
                work_item_from_row,
            )
            .optional()?
            .ok_or(StorageError::WorkItemNotFound(id))
    }

    pub fn set_work_item_branch(&self, id: i64, branch: &str) -> Result<(), StorageError> {
        self.set_work_item_field(id, "branch", branch)
    }

    pub fn set_work_item_worktree(&self, id: i64, worktree_path: &str) -> Result<(), StorageError> {
        self.set_work_item_field(id, "worktree_path", worktree_path)
    }

    pub fn set_work_item_pr_ref(&self, id: i64, pr_ref: &str) -> Result<(), StorageError> {
        self.set_work_item_field(id, "pr_ref", pr_ref)
    }

    /// Records the agent session driving a work item (see `[agents.*]` /
    /// ProcessRunner). Overwritten on resume-after-restart.
    pub fn set_work_item_agent_session(&self, id: i64, session: &str) -> Result<(), StorageError> {
        self.set_work_item_field(id, "agent_session", session)
    }

    /// Agent session recorded for a work item; `None` = no agent started.
    pub fn work_item_agent_session(&self, id: i64) -> Result<Option<String>, StorageError> {
        self.conn
            .query_row(
                "SELECT agent_session FROM work_items WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(StorageError::WorkItemNotFound(id))
    }

    /// `column` comes only from the fixed set above — never caller input.
    fn set_work_item_field(&self, id: i64, column: &str, value: &str) -> Result<(), StorageError> {
        let changed = self.conn.execute(
            &format!("UPDATE work_items SET {column} = ?1, updated_at = ?2 WHERE id = ?3"),
            params![value, now_rfc3339(), id],
        )?;
        if changed == 0 {
            return Err(StorageError::WorkItemNotFound(id));
        }
        Ok(())
    }

    /// Inserts a pending gate request and appends a `gate_requested` event
    /// (payload `{"gate_id", "action"}`) in the same transaction. Returns the
    /// gate request id.
    pub fn create_gate_request(
        &mut self,
        work_item_id: i64,
        action_kind: &str,
        payload: &Value,
    ) -> Result<i64, StorageError> {
        let tx = self.conn.transaction()?;
        let exists: Option<i64> = tx
            .query_row(
                "SELECT id FROM work_items WHERE id = ?1",
                [work_item_id],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(StorageError::WorkItemNotFound(work_item_id));
        }
        let now = now_rfc3339();
        tx.execute(
            "INSERT INTO gate_requests (work_item_id, action_kind, payload, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![work_item_id, action_kind, payload.to_string(), now],
        )?;
        let gate_id = tx.last_insert_rowid();
        let event = serde_json::json!({ "gate_id": gate_id, "action": action_kind });
        insert_event(
            &tx,
            Some(work_item_id),
            "system",
            "gate_requested",
            &event,
            &now,
        )?;
        tx.commit()?;
        Ok(gate_id)
    }

    /// Resolves a pending gate request (`verdict` = `"approved"` or
    /// `"rejected"`) and appends the matching `gate_<verdict>` event (payload
    /// `{"gate_id", "action"[, "reason"]}`) in the same transaction. The
    /// UPDATE is guarded on `status = 'pending'`: resolving twice (or a
    /// missing id) is an error.
    pub fn resolve_gate_request(
        &mut self,
        id: i64,
        verdict: &str,
        actor: &str,
        reason: Option<&str>,
    ) -> Result<(), StorageError> {
        let tx = self.conn.transaction()?;
        let row: Option<(i64, String, String)> = tx
            .query_row(
                "SELECT work_item_id, action_kind, status FROM gate_requests WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((work_item_id, action_kind, status)) = row else {
            return Err(StorageError::GateRequestNotFound(id));
        };
        let now = now_rfc3339();
        let changed = tx.execute(
            "UPDATE gate_requests SET status = ?1, resolved_at = ?2, resolved_by = ?3
             WHERE id = ?4 AND status = 'pending'",
            params![verdict, now, actor, id],
        )?;
        if changed == 0 {
            return Err(StorageError::GateRequestNotPending { id, status });
        }
        let mut event = serde_json::json!({ "gate_id": id, "action": action_kind });
        if let Some(reason) = reason {
            event["reason"] = reason.into();
        }
        insert_event(
            &tx,
            Some(work_item_id),
            actor,
            &format!("gate_{verdict}"),
            &event,
            &now,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Fetches one gate request by id.
    pub fn get_gate_request(&self, id: i64) -> Result<GateRequestRow, StorageError> {
        self.conn
            .query_row(
                "SELECT id, work_item_id, action_kind, payload, status, created_at,
                        resolved_at, resolved_by
                 FROM gate_requests WHERE id = ?1",
                [id],
                gate_request_from_row,
            )
            .optional()?
            .ok_or(StorageError::GateRequestNotFound(id))
    }

    /// Pending gate requests in creation order; `Some(id)` filters to one
    /// work item.
    pub fn pending_gate_requests(
        &self,
        work_item_id: Option<i64>,
    ) -> Result<Vec<GateRequestRow>, StorageError> {
        const BASE: &str = "SELECT id, work_item_id, action_kind, payload, status, created_at,
                                   resolved_at, resolved_by
                            FROM gate_requests WHERE status = 'pending'";
        let rows = match work_item_id {
            Some(id) => {
                let mut stmt = self
                    .conn
                    .prepare(&format!("{BASE} AND work_item_id = ?1 ORDER BY id"))?;
                let rows = stmt.query_map([id], gate_request_from_row)?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
            None => {
                let mut stmt = self.conn.prepare(&format!("{BASE} ORDER BY id"))?;
                let rows = stmt.query_map([], gate_request_from_row)?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
        };
        Ok(rows)
    }
}

fn migrate(conn: &mut Connection) -> Result<(), StorageError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let supported = MIGRATIONS.len() as i64;
    if version > supported {
        return Err(StorageError::UnsupportedVersion {
            found: version,
            supported,
        });
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let target = (i + 1) as i64;
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", target)?;
        tx.commit()?;
        tracing::debug!(version = target, "applied sqlite migration");
    }
    Ok(())
}

fn insert_event(
    conn: &Connection,
    work_item_id: Option<i64>,
    actor: &str,
    kind: &str,
    payload: &Value,
    ts: &str,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO events (work_item_id, ts, actor, kind, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![work_item_id, ts, actor, kind, payload.to_string()],
    )?;
    Ok(conn.last_insert_rowid())
}

fn event_from_row(row: &Row) -> rusqlite::Result<EventRow> {
    let raw: String = row.get(5)?;
    let payload = serde_json::from_str(&raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(EventRow {
        id: row.get(0)?,
        work_item_id: row.get(1)?,
        ts: row.get(2)?,
        actor: row.get(3)?,
        kind: row.get(4)?,
        payload,
    })
}

fn work_item_from_row(row: &Row) -> rusqlite::Result<WorkItemRow> {
    Ok(WorkItemRow {
        id: row.get(0)?,
        provider: row.get(1)?,
        ticket_key: row.get(2)?,
        repo: row.get(3)?,
        state: row.get(4)?,
        branch: row.get(5)?,
        worktree_path: row.get(6)?,
        pr_ref: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn gate_request_from_row(row: &Row) -> rusqlite::Result<GateRequestRow> {
    let raw: String = row.get(3)?;
    let payload = serde_json::from_str(&raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(GateRequestRow {
        id: row.get(0)?,
        work_item_id: row.get(1)?,
        action_kind: row.get(2)?,
        payload,
        status: row.get(4)?,
        created_at: row.get(5)?,
        resolved_at: row.get(6)?,
        resolved_by: row.get(7)?,
    })
}

/// Current time as an RFC3339 UTC string, seconds precision.
fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the unix epoch")
        .as_secs();
    rfc3339_from_unix(secs as i64)
}

/// Unix seconds → `YYYY-MM-DDTHH:MM:SSZ`. Civil-date conversion follows
/// Howard Hinnant's `civil_from_days` algorithm.
fn rfc3339_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, min, s) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
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

    #[test]
    fn open_creates_db_at_current_version() {
        let (dir, storage) = open_temp();
        assert!(dir.path().join(DB_FILE).exists());
        let version: i64 = storage
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        let fk: i64 = storage
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }

    #[test]
    fn v1_database_migrates_and_gains_agent_session() {
        let dir = tempfile::tempdir().unwrap();
        {
            // Hand-build a v1 database exactly as the shipped v1 batch did.
            let mut conn = Connection::open(dir.path().join(DB_FILE)).unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(MIGRATIONS[0]).unwrap();
            tx.pragma_update(None, "user_version", 1).unwrap();
            tx.commit().unwrap();
        }
        let storage = Storage::open(dir.path()).unwrap();
        let id = storage
            .create_work_item("gh", "acme/app#1", "backend", "tracked")
            .unwrap();
        assert_eq!(storage.work_item_agent_session(id).unwrap(), None);
        storage.set_work_item_agent_session(id, "abc123").unwrap();
        assert_eq!(
            storage.work_item_agent_session(id).unwrap().as_deref(),
            Some("abc123")
        );
    }

    #[test]
    fn agent_session_round_trips_and_missing_item_errors() {
        let (_dir, storage) = open_temp();
        let id = storage
            .create_work_item("gh", "acme/app#2", "backend", "tracked")
            .unwrap();
        assert_eq!(storage.work_item_agent_session(id).unwrap(), None);
        storage.set_work_item_agent_session(id, "s-1").unwrap();
        assert_eq!(
            storage.work_item_agent_session(id).unwrap().as_deref(),
            Some("s-1")
        );
        assert!(matches!(
            storage.work_item_agent_session(id + 1),
            Err(StorageError::WorkItemNotFound(_))
        ));
        assert!(matches!(
            storage.set_work_item_agent_session(id + 1, "x"),
            Err(StorageError::WorkItemNotFound(_))
        ));
    }

    #[test]
    fn open_rejects_future_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        {
            let storage = Storage::open(dir.path()).unwrap();
            storage
                .conn
                .pragma_update(None, "user_version", 99)
                .unwrap();
        }
        assert!(matches!(
            Storage::open(dir.path()),
            Err(StorageError::UnsupportedVersion { found: 99, .. })
        ));
    }

    #[test]
    fn rows_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let id = {
            let storage = Storage::open(dir.path()).unwrap();
            let id = storage
                .create_work_item("jira", "ZEE-1", "backend", "new")
                .unwrap();
            storage
                .append_event(Some(id), "daemon", "created", &json!({"via": "test"}))
                .unwrap();
            id
        }; // dropped: connection closed, crash-survival proxy
        let storage = Storage::open(dir.path()).unwrap();
        let items = storage.list_work_items().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, id);
        assert_eq!(items[0].ticket_key, "ZEE-1");
        assert_eq!(storage.events(Some(id)).unwrap().len(), 1);
    }

    #[test]
    fn append_event_round_trips_and_filters() {
        let (_dir, storage) = open_temp();
        let a = storage
            .create_work_item("jira", "ZEE-1", "backend", "new")
            .unwrap();
        let b = storage
            .create_work_item("jira", "ZEE-2", "backend", "new")
            .unwrap();
        let payload = json!({"n": 1, "nested": {"ok": true}});
        storage
            .append_event(Some(a), "agent", "started", &payload)
            .unwrap();
        storage
            .append_event(Some(b), "agent", "started", &json!({"n": 2}))
            .unwrap();
        storage
            .append_event(None, "daemon", "boot", &json!({}))
            .unwrap();

        let for_a = storage.events(Some(a)).unwrap();
        assert_eq!(for_a.len(), 1);
        assert_eq!(for_a[0].work_item_id, Some(a));
        assert_eq!(for_a[0].actor, "agent");
        assert_eq!(for_a[0].kind, "started");
        assert_eq!(for_a[0].payload, payload);
        assert!(for_a[0].ts.ends_with('Z'));

        let all = storage.events(None).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[2].work_item_id, None);
    }

    #[test]
    fn append_event_enforces_work_item_fk() {
        let (_dir, storage) = open_temp();
        assert!(
            storage
                .append_event(Some(999), "agent", "started", &json!({}))
                .is_err()
        );
        assert!(storage.events(None).unwrap().is_empty());
    }

    #[test]
    fn state_change_writes_exactly_one_event_atomically() {
        let (_dir, mut storage) = open_temp();
        let id = storage
            .create_work_item("jira", "ZEE-1", "backend", "new")
            .unwrap();
        storage.update_work_item_state(id, "in_progress").unwrap();

        let items = storage.list_work_items().unwrap();
        assert_eq!(items[0].state, "in_progress");
        let events = storage.events(Some(id)).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "state_changed");
        assert_eq!(
            events[0].payload,
            json!({"old": "new", "new": "in_progress"})
        );
    }

    #[test]
    fn state_change_rolls_back_on_constraint_failure() {
        let (_dir, mut storage) = open_temp();
        let id = storage
            .create_work_item("jira", "ZEE-1", "backend", "new")
            .unwrap();
        // Missing item: the events FK fails inside the transaction; nothing
        // (no event row, no state mutation) may persist.
        assert!(matches!(
            storage.update_work_item_state(id + 1, "in_progress"),
            Err(StorageError::WorkItemNotFound(_))
        ));
        assert!(storage.events(None).unwrap().is_empty());
        assert_eq!(storage.list_work_items().unwrap()[0].state, "new");
    }

    #[test]
    fn work_item_identity_is_unique() {
        let (_dir, storage) = open_temp();
        storage
            .create_work_item("jira", "ZEE-1", "backend", "new")
            .unwrap();
        assert!(
            storage
                .create_work_item("jira", "ZEE-1", "backend", "new")
                .is_err()
        );
        // Same ticket in another repo is a distinct work item.
        storage
            .create_work_item("jira", "ZEE-1", "frontend", "new")
            .unwrap();
        assert_eq!(storage.list_work_items().unwrap().len(), 2);
    }

    #[test]
    fn count_pending_gates_counts_only_pending() {
        let (_dir, storage) = open_temp();
        let id = storage
            .create_work_item("jira", "ZEE-1", "backend", "new")
            .unwrap();
        // Gate creation is out of scope for M1's public API; seed directly.
        for status in ["pending", "pending", "approved"] {
            storage
                .conn
                .execute(
                    "INSERT INTO gate_requests (work_item_id, action_kind, payload, status, created_at)
                     VALUES (?1, 'publish_pr', '{}', ?2, ?3)",
                    params![id, status, now_rfc3339()],
                )
                .unwrap();
        }
        assert_eq!(storage.count_pending_gates(id).unwrap(), 2);
        assert_eq!(storage.count_pending_gates(id + 1).unwrap(), 0);
    }

    #[test]
    fn field_setters_update_row_and_reject_missing_item() {
        let (_dir, storage) = open_temp();
        let id = storage
            .create_work_item("github", "acme/app#1", "app", "developing")
            .unwrap();
        storage.set_work_item_branch(id, "feat/app-1").unwrap();
        storage.set_work_item_worktree(id, "/tmp/wt/app-1").unwrap();
        storage.set_work_item_pr_ref(id, "acme/app#12").unwrap();
        let row = storage.get_work_item(id).unwrap();
        assert_eq!(row.branch.as_deref(), Some("feat/app-1"));
        assert_eq!(row.worktree_path.as_deref(), Some("/tmp/wt/app-1"));
        assert_eq!(row.pr_ref.as_deref(), Some("acme/app#12"));

        assert!(matches!(
            storage.set_work_item_branch(999, "x"),
            Err(StorageError::WorkItemNotFound(999))
        ));
        assert!(matches!(
            storage.get_work_item(999),
            Err(StorageError::WorkItemNotFound(999))
        ));
    }

    #[test]
    fn rfc3339_formatting() {
        assert_eq!(rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_from_unix(1_700_000_000), "2023-11-14T22:13:20Z");
        // Leap day.
        assert_eq!(rfc3339_from_unix(1_582_934_400), "2020-02-29T00:00:00Z");
        assert!(now_rfc3339().len() == 20);
    }
}
