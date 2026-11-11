//! Local state database (SQLite).
//!
//! One `node` row per file or folder in the vault: where it is locally (`parent_id`, `name`),
//! where it is remotely as of the last sync (`remote_*`), and what work is pending. The worker
//! converges the cloud to the local rows; the scanner folds remote changes into them. Because
//! every pending action is recorded as row state (not as a queue of commands), a crash at any
//! point leaves the database consistent: the next run simply redoes the pending work.
//!
//! Performance rules applied here (see `docs/database.md`):
//! * WAL journal, `synchronous=NORMAL`, `busy_timeout`; separate writer and reader connections.
//! * Short transactions only — never hold one across network I/O.
//! * Batch writes (a whole scan is applied in one transaction) instead of row-by-row commits.
//! * Every hot query is served by an index; work-queue and LRU lookups use partial indexes.
//! * Explicit column lists, `LIMIT` on unbounded reads, keyset pagination for activity.
//! * STRICT tables with `NOT NULL` and `CHECK` constraints; integer primary keys.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::ipc::{Activity, ActivityKind, Issue, IssueAction, IssueKind};
use crate::remote::Kind;

pub type NodeId = i64;
pub const ROOT_ID: NodeId = 1;

/// Pending work bits stored in `node.pending`.
pub mod pending {
    /// Needs creating remotely (mkdir, or upload of a new/modified file).
    pub const PUSH: i64 = 1;
    /// Local location differs from the remote one: move/rename remotely.
    pub const MOVE: i64 = 2;
    /// Deleted locally: trash remotely, then drop the row.
    pub const TRASH: i64 = 4;
}

const SCHEMA_VERSION: i64 = 2;

/// v2: per-file transfer mode, and persisted resumable-upload sessions.
const SCHEMA_V2: &str = r#"
ALTER TABLE node ADD COLUMN upload_mode INTEGER NOT NULL DEFAULT 0 CHECK (upload_mode BETWEEN 0 AND 3);

CREATE TABLE upload_session (
    node_id     INTEGER PRIMARY KEY REFERENCES node(id) ON DELETE CASCADE,
    size        INTEGER NOT NULL CHECK (size >= 0),
    mtime_ms    INTEGER NOT NULL,
    state       TEXT    NOT NULL,
    created_ms  INTEGER NOT NULL,
    updated_ms  INTEGER NOT NULL
) STRICT;
"#;

/// `node.upload_mode` values (see [`crate::ipc::TransferMode`]).
pub mod upload_mode {
    pub const AUTO: i64 = 0;
    pub const PAUSED: i64 = 1;
    pub const TONIGHT: i64 = 2;
    pub const NOW: i64 = 3;
}

/// Which uploads may start right now (pause, overnight window).
#[derive(Debug, Clone, Copy, Default)]
pub struct UploadGate {
    /// Night uploads are on and the window is closed.
    pub night_closed: bool,
    /// With `night_closed`: files at least this large wait (when in auto mode).
    pub night_min_bytes: u64,
}

/// SQL condition (on `node`) that is true for uploads held back by the user or the schedule.
/// Parameters: `?night_closed` (0/1) and `?night_min`.
const HELD: &str = "(kind = 0 AND pending & 1 != 0 AND (
        upload_mode = 1
        OR (upload_mode = 2 AND :night_closed)
        OR (upload_mode = 0 AND :night_closed AND size >= :night_min)))";

#[derive(Debug, Clone)]
pub struct UploadSession {
    pub node_id: NodeId,
    pub size: u64,
    pub mtime_ms: i64,
    pub state: crate::remote::resumable::ResumeState,
}

const SCHEMA_V1: &str = r#"
CREATE TABLE node (
    id               INTEGER PRIMARY KEY,
    parent_id        INTEGER REFERENCES node(id) ON DELETE CASCADE,
    name             TEXT    NOT NULL,
    name_key         TEXT    NOT NULL,
    kind             INTEGER NOT NULL CHECK (kind IN (0, 1)),
    size             INTEGER NOT NULL DEFAULT 0 CHECK (size >= 0),
    mtime_ms         INTEGER NOT NULL DEFAULT 0,
    remote_id        TEXT    UNIQUE,
    remote_parent_id TEXT,
    remote_name      TEXT,
    dirty            INTEGER NOT NULL DEFAULT 0 CHECK (dirty IN (0, 1)),
    deleted          INTEGER NOT NULL DEFAULT 0 CHECK (deleted IN (0, 1)),
    pending          INTEGER NOT NULL DEFAULT 0 CHECK (pending BETWEEN 0 AND 7),
    generation       INTEGER NOT NULL DEFAULT 0,
    op_seq           INTEGER NOT NULL DEFAULT 0,
    changed_ms       INTEGER NOT NULL DEFAULT 0,
    retry_at_ms      INTEGER NOT NULL DEFAULT 0,
    attempts         INTEGER NOT NULL DEFAULT 0,
    last_error       TEXT,
    hydrated         INTEGER NOT NULL DEFAULT 0 CHECK (hydrated IN (0, 1)),
    pinned           INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
    accessed_ms      INTEGER NOT NULL DEFAULT 0,
    created_ms       INTEGER NOT NULL
) STRICT;

-- Child lookup (FUSE lookup, path resolution, collision checks); also the FK index.
CREATE UNIQUE INDEX node_parent_name_key_idx ON node (parent_id, name_key) WHERE deleted = 0;
CREATE INDEX node_parent_id_idx ON node (parent_id);
-- Work queue: only rows with pending work are indexed.
CREATE INDEX node_pending_idx ON node (retry_at_ms) WHERE pending != 0;
-- Eviction candidates in LRU order.
CREATE INDEX node_lru_idx ON node (accessed_ms) WHERE hydrated = 1 AND pinned = 0 AND kind = 0;

CREATE TABLE activity (
    id      INTEGER PRIMARY KEY,
    at_ms   INTEGER NOT NULL,
    kind    TEXT    NOT NULL CHECK (kind IN ('uploaded','downloaded','created_folder','moved','deleted','conflict','evicted','failed')),
    path    TEXT    NOT NULL,
    bytes   INTEGER NOT NULL DEFAULT 0,
    detail  TEXT
) STRICT;

CREATE TABLE issue (
    id          INTEGER PRIMARY KEY,
    kind        TEXT    NOT NULL CHECK (kind IN ('transfer_failed','invalid_name','conflict','mass_delete','wrong_password','auth','disk_full','file_locked','other')),
    dedupe_key  TEXT    NOT NULL UNIQUE,
    node_id     INTEGER REFERENCES node(id) ON DELETE CASCADE,
    path        TEXT,
    message     TEXT    NOT NULL,
    created_ms  INTEGER NOT NULL
) STRICT;
CREATE INDEX issue_node_id_idx ON issue (node_id);

-- Superseded remote versions (left behind when a file is re-uploaded) still to be trashed.
-- Scans ignore these ids so an old version is never mistaken for a new remote file.
CREATE TABLE orphan (
    remote_id        TEXT    PRIMARY KEY,
    kind             INTEGER NOT NULL CHECK (kind IN (0, 1)),
    name             TEXT    NOT NULL,
    remote_parent_id TEXT    NOT NULL,
    size             INTEGER NOT NULL DEFAULT 0,
    attempts         INTEGER NOT NULL DEFAULT 0,
    retry_at_ms      INTEGER NOT NULL DEFAULT 0
) STRICT, WITHOUT ROWID;

CREATE TABLE kv (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT, WITHOUT ROWID;
"#;

/// A node as most of the engine needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: NodeId,
    pub parent_id: Option<NodeId>,
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    pub mtime_ms: i64,
    pub remote_id: Option<String>,
    pub remote_parent_id: Option<String>,
    pub remote_name: Option<String>,
    pub dirty: bool,
    pub deleted: bool,
    pub pending: i64,
    pub generation: i64,
    pub op_seq: i64,
    pub changed_ms: i64,
    pub retry_at_ms: i64,
    pub attempts: i64,
    pub hydrated: bool,
    pub pinned: bool,
    pub accessed_ms: i64,
    pub upload_mode: i64,
}

pub const NODE_COLUMNS: &str = "id, parent_id, name, kind, size, mtime_ms, remote_id, remote_parent_id, remote_name, \
     dirty, deleted, pending, generation, op_seq, changed_ms, retry_at_ms, attempts, hydrated, pinned, accessed_ms, upload_mode";

impl Node {
    pub fn from_row(r: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Node {
            id: r.get(0)?,
            parent_id: r.get(1)?,
            name: r.get(2)?,
            kind: if r.get::<_, i64>(3)? == 1 {
                Kind::Folder
            } else {
                Kind::File
            },
            size: r.get::<_, i64>(4)? as u64,
            mtime_ms: r.get(5)?,
            remote_id: r.get(6)?,
            remote_parent_id: r.get(7)?,
            remote_name: r.get(8)?,
            dirty: r.get(9)?,
            deleted: r.get(10)?,
            pending: r.get(11)?,
            generation: r.get(12)?,
            op_seq: r.get(13)?,
            changed_ms: r.get(14)?,
            retry_at_ms: r.get(15)?,
            attempts: r.get(16)?,
            hydrated: r.get(17)?,
            pinned: r.get(18)?,
            accessed_ms: r.get(19)?,
            upload_mode: r.get(20)?,
        })
    }

    pub fn is_folder(&self) -> bool {
        self.kind == Kind::Folder
    }
}

pub fn kind_int(k: Kind) -> i64 {
    match k {
        Kind::File => 0,
        Kind::Folder => 1,
    }
}

/// Case-folded name used for collision checks. Windows (and, through the shared vault, every
/// device) treats names differing only in case as the same item.
pub fn name_key(name: &str) -> String {
    name.to_lowercase()
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn activity_kind_str(k: ActivityKind) -> &'static str {
    match k {
        ActivityKind::Uploaded => "uploaded",
        ActivityKind::Downloaded => "downloaded",
        ActivityKind::CreatedFolder => "created_folder",
        ActivityKind::Moved => "moved",
        ActivityKind::Deleted => "deleted",
        ActivityKind::Conflict => "conflict",
        ActivityKind::Evicted => "evicted",
        ActivityKind::Failed => "failed",
    }
}

fn activity_kind_parse(s: &str) -> ActivityKind {
    match s {
        "uploaded" => ActivityKind::Uploaded,
        "downloaded" => ActivityKind::Downloaded,
        "created_folder" => ActivityKind::CreatedFolder,
        "moved" => ActivityKind::Moved,
        "deleted" => ActivityKind::Deleted,
        "conflict" => ActivityKind::Conflict,
        "evicted" => ActivityKind::Evicted,
        _ => ActivityKind::Failed,
    }
}

pub fn issue_kind_str(k: IssueKind) -> &'static str {
    match k {
        IssueKind::TransferFailed => "transfer_failed",
        IssueKind::InvalidName => "invalid_name",
        IssueKind::Conflict => "conflict",
        IssueKind::MassDelete => "mass_delete",
        IssueKind::WrongPassword => "wrong_password",
        IssueKind::Auth => "auth",
        IssueKind::DiskFull => "disk_full",
        IssueKind::FileLocked => "file_locked",
        IssueKind::Other => "other",
    }
}

fn issue_kind_parse(s: &str) -> IssueKind {
    match s {
        "transfer_failed" => IssueKind::TransferFailed,
        "invalid_name" => IssueKind::InvalidName,
        "conflict" => IssueKind::Conflict,
        "mass_delete" => IssueKind::MassDelete,
        "wrong_password" => IssueKind::WrongPassword,
        "auth" => IssueKind::Auth,
        "disk_full" => IssueKind::DiskFull,
        "file_locked" => IssueKind::FileLocked,
        _ => IssueKind::Other,
    }
}

pub fn issue_actions(kind: IssueKind) -> Vec<IssueAction> {
    match kind {
        IssueKind::TransferFailed | IssueKind::FileLocked => {
            vec![IssueAction::Retry, IssueAction::Dismiss]
        }
        IssueKind::MassDelete => vec![IssueAction::ConfirmDelete, IssueAction::RestoreDeleted],
        IssueKind::Auth | IssueKind::WrongPassword => vec![IssueAction::SignIn],
        IssueKind::DiskFull => vec![IssueAction::Retry],
        IssueKind::InvalidName | IssueKind::Conflict | IssueKind::Other => {
            vec![IssueAction::Dismiss]
        }
    }
}

fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -16000;",
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    Ok(())
}

/// Writer and reader connections. Lock one at a time and never across an `.await`.
pub struct Db {
    writer: Mutex<Connection>,
    reader: Mutex<Connection>,
    temp_path: Option<std::path::PathBuf>,
}

impl Drop for Db {
    fn drop(&mut self) {
        if let Some(p) = self.temp_path.take() {
            // Connections close when the fields drop after this; remove files best-effort.
            let _ = std::fs::remove_file(&p);
            let _ = std::fs::remove_file(p.with_extension("db-wal"));
            let _ = std::fs::remove_file(p.with_extension("db-shm"));
        }
    }
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let writer = Connection::open(path)?;
        configure(&writer)?;
        migrate(&writer)?;
        let reader = Connection::open(path)?;
        configure(&reader)?;
        Ok(Self {
            writer: Mutex::new(writer),
            reader: Mutex::new(reader),
            temp_path: None,
        })
    }

    /// A throwaway database in a temporary file (deleted on drop), for tests and previews.
    /// A real file (not `:memory:`) so WAL and both connections behave exactly as in production.
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let path = std::env::temp_dir().join(format!("drive-test-{}.db", rand::random::<u64>()));
        let mut db = Self::open(&path)?;
        db.temp_path = Some(path);
        Ok(db)
    }

    /// Runs `f` inside a write transaction.
    pub fn write<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        let mut conn = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// Runs `f` on the read connection.
    pub fn read<T>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        let conn = self.reader.lock().unwrap_or_else(|e| e.into_inner());
        f(&conn)
    }

    /// `PRAGMA optimize` — cheap, keeps planner statistics fresh. Call periodically and on exit.
    pub fn optimize(&self) {
        let conn = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let _ = conn.execute_batch("PRAGMA optimize;");
    }

    // ---- key/value -----------------------------------------------------------------------

    pub fn kv_get(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.read(|c| {
            c.query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
                .optional()
        })
    }

    pub fn kv_set(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map(|_| ())
        })
    }

    // ---- nodes ---------------------------------------------------------------------------

    pub fn node(&self, id: NodeId) -> rusqlite::Result<Option<Node>> {
        self.read(|c| get_node(c, id))
    }

    pub fn child(&self, parent: NodeId, name: &str) -> rusqlite::Result<Option<Node>> {
        self.read(|c| get_child(c, parent, name))
    }

    pub fn children(&self, parent: NodeId) -> rusqlite::Result<Vec<Node>> {
        self.read(|c| {
            let mut st = c.prepare_cached(&format!(
                "SELECT {NODE_COLUMNS} FROM node WHERE parent_id = ?1 AND deleted = 0 ORDER BY name_key"
            ))?;
            st.query_map([parent], Node::from_row)?.collect()
        })
    }

    pub fn all_nodes(&self) -> rusqlite::Result<Vec<Node>> {
        self.read(|c| {
            let mut st = c.prepare(&format!("SELECT {NODE_COLUMNS} FROM node"))?;
            st.query_map([], Node::from_row)?.collect()
        })
    }

    /// Resolves a `/`-separated relative path ("" is the root).
    pub fn node_by_path(&self, path: &str) -> rusqlite::Result<Option<Node>> {
        self.read(|c| {
            let mut cur = match get_node(c, ROOT_ID)? {
                Some(n) => n,
                None => return Ok(None),
            };
            for part in path.split('/').filter(|p| !p.is_empty()) {
                match get_child(c, cur.id, part)? {
                    Some(n) => cur = n,
                    None => return Ok(None),
                }
            }
            Ok(Some(cur))
        })
    }

    /// Relative path of a node (one recursive query).
    pub fn path_of(&self, id: NodeId) -> rusqlite::Result<Option<String>> {
        self.read(|c| path_of(c, id))
    }

    pub fn node_by_remote_id(&self, remote_id: &str) -> rusqlite::Result<Option<Node>> {
        self.read(|c| {
            c.query_row(
                &format!("SELECT {NODE_COLUMNS} FROM node WHERE remote_id = ?1"),
                [remote_id],
                Node::from_row,
            )
            .optional()
        })
    }

    /// Nodes with pending work that is due, oldest change first.
    pub fn due_work(
        &self,
        now: i64,
        debounce_before: i64,
        limit: u32,
        gate: UploadGate,
    ) -> rusqlite::Result<Vec<Node>> {
        self.read(|c| {
            let mut st = c.prepare_cached(&format!(
                "SELECT {NODE_COLUMNS} FROM node
                 WHERE pending != 0 AND retry_at_ms <= :now AND (changed_ms <= :settled OR pending & 4 != 0 OR kind = 1)
                   AND NOT {HELD}
                 ORDER BY upload_mode = 3 DESC, retry_at_ms, changed_ms LIMIT :limit"
            ))?;
            st.query_map(
                rusqlite::named_params! {
                    ":now": now, ":settled": debounce_before, ":limit": limit,
                    ":night_closed": gate.night_closed, ":night_min": gate.night_min_bytes as i64,
                },
                Node::from_row,
            )?
            .collect()
        })
    }

    /// Files waiting to upload that aren't in `exclude`, held ones last (transfer list).
    pub fn queued_uploads(
        &self,
        limit: u32,
        gate: UploadGate,
    ) -> rusqlite::Result<(Vec<Node>, u32)> {
        self.read(|c| {
            let mut st = c.prepare_cached(&format!(
                "SELECT {NODE_COLUMNS} FROM node
                 WHERE kind = 0 AND deleted = 0 AND pending & 1 != 0
                 ORDER BY {HELD}, upload_mode = 3 DESC, retry_at_ms, changed_ms LIMIT :limit"
            ))?;
            let rows = st
                .query_map(
                    rusqlite::named_params! {
                        ":limit": limit, ":night_closed": gate.night_closed,
                        ":night_min": gate.night_min_bytes as i64,
                    },
                    Node::from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            let count: u32 = c.query_row(
                "SELECT COUNT(*) FROM node WHERE kind = 0 AND deleted = 0 AND pending & 1 != 0",
                [],
                |r| r.get(0),
            )?;
            Ok((rows, count))
        })
    }

    pub fn set_upload_mode(&self, id: NodeId, mode: i64) -> rusqlite::Result<bool> {
        self.write(|tx| {
            tx.execute(
                "UPDATE node SET upload_mode = ?2, retry_at_ms = CASE WHEN ?2 = 3 THEN 0 ELSE retry_at_ms END
                 WHERE id = ?1 AND kind = 0",
                params![id, mode],
            )
            .map(|n| n > 0)
        })
    }

    // ---- upload sessions -------------------------------------------------------------------

    pub fn upload_session(&self, id: NodeId) -> rusqlite::Result<Option<UploadSession>> {
        self.read(|c| {
            c.prepare_cached(
                "SELECT node_id, size, mtime_ms, state FROM upload_session WHERE node_id = ?1",
            )?
            .query_row([id], |r| {
                let state: String = r.get(3)?;
                Ok(UploadSession {
                    node_id: r.get(0)?,
                    size: r.get::<_, i64>(1)? as u64,
                    mtime_ms: r.get(2)?,
                    state: serde_json::from_str(&state).unwrap_or_default(),
                })
            })
            .optional()
        })
    }

    /// All sessions (small table: one row per upload in progress).
    pub fn upload_sessions(&self) -> rusqlite::Result<Vec<UploadSession>> {
        self.read(|c| {
            let mut st = c.prepare_cached(
                "SELECT node_id, size, mtime_ms, state FROM upload_session LIMIT 10000",
            )?;
            st.query_map([], |r| {
                let state: String = r.get(3)?;
                Ok(UploadSession {
                    node_id: r.get(0)?,
                    size: r.get::<_, i64>(1)? as u64,
                    mtime_ms: r.get(2)?,
                    state: serde_json::from_str(&state).unwrap_or_default(),
                })
            })?
            .collect()
        })
    }

    pub fn save_upload_session(
        &self,
        id: NodeId,
        size: u64,
        mtime_ms: i64,
        state: &crate::remote::resumable::ResumeState,
    ) -> rusqlite::Result<()> {
        let json = serde_json::to_string(state).unwrap_or_else(|_| "{}".into());
        let now = now_ms();
        self.write(|tx| {
            tx.execute(
                "INSERT INTO upload_session (node_id, size, mtime_ms, state, created_ms, updated_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT (node_id) DO UPDATE SET size = excluded.size, mtime_ms = excluded.mtime_ms,
                     state = excluded.state, updated_ms = excluded.updated_ms",
                params![id, size as i64, mtime_ms, json, now],
            )
            .map(|_| ())
        })
    }

    pub fn delete_upload_session(&self, id: NodeId) -> rusqlite::Result<()> {
        self.write(|tx| {
            tx.execute("DELETE FROM upload_session WHERE node_id = ?1", [id])
                .map(|_| ())
        })
    }

    /// Earliest time any pending work becomes due (for the worker's sleep).
    pub fn next_work_at(&self, gate: UploadGate) -> rusqlite::Result<Option<i64>> {
        self.read(|c| {
            c.query_row(
                &format!("SELECT MIN(MAX(retry_at_ms, changed_ms)) FROM node WHERE pending != 0 AND NOT {HELD}"),
                rusqlite::named_params! {
                    ":night_closed": gate.night_closed, ":night_min": gate.night_min_bytes as i64,
                },
                |r| r.get(0),
            )
        })
    }

    pub fn pending_count(&self) -> rusqlite::Result<u32> {
        self.read(|c| {
            c.query_row("SELECT COUNT(*) FROM node WHERE pending != 0", [], |r| {
                r.get(0)
            })
        })
    }

    /// Files deleted locally and waiting to be trashed remotely.
    pub fn pending_trash_files(&self) -> rusqlite::Result<u32> {
        self.read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM node WHERE pending & 4 != 0 AND kind = 0",
                [],
                |r| r.get(0),
            )
        })
    }

    /// LRU eviction candidates: downloaded, unpinned, clean files.
    pub fn eviction_candidates(&self, limit: u32) -> rusqlite::Result<Vec<Node>> {
        self.read(|c| {
            let mut st = c.prepare_cached(&format!(
                "SELECT {NODE_COLUMNS} FROM node
                 WHERE hydrated = 1 AND pinned = 0 AND kind = 0 AND dirty = 0 AND pending = 0 AND deleted = 0
                 ORDER BY accessed_ms LIMIT ?1"
            ))?;
            st.query_map([limit], Node::from_row)?.collect()
        })
    }

    pub fn storage_totals(&self) -> rusqlite::Result<StorageTotals> {
        self.read(|c| {
            c.query_row(
                "SELECT
                    COALESCE(SUM(CASE WHEN hydrated = 1 AND pinned = 0 THEN size END), 0),
                    COALESCE(SUM(CASE WHEN hydrated = 1 AND pinned = 1 THEN size END), 0),
                    COALESCE(SUM(CASE WHEN remote_id IS NOT NULL THEN size END), 0),
                    COUNT(CASE WHEN remote_id IS NOT NULL THEN 1 END)
                 FROM node WHERE kind = 0 AND deleted = 0",
                [],
                |r| {
                    Ok(StorageTotals {
                        cache_bytes: r.get::<_, i64>(0)? as u64,
                        pinned_bytes: r.get::<_, i64>(1)? as u64,
                        cloud_bytes: r.get::<_, i64>(2)? as u64,
                        cloud_files: r.get::<_, i64>(3)? as u64,
                    })
                },
            )
        })
    }

    pub fn set_access_times(&self, items: &[(NodeId, i64)]) -> rusqlite::Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        self.write(|tx| {
            let mut st = tx.prepare_cached(
                "UPDATE node SET accessed_ms = MAX(accessed_ms, ?2) WHERE id = ?1",
            )?;
            for (id, at) in items {
                st.execute(params![id, at])?;
            }
            Ok(())
        })
    }

    pub fn set_hydrated(&self, id: NodeId, hydrated: bool) -> rusqlite::Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE node SET hydrated = ?2, accessed_ms = CASE WHEN ?2 = 1 THEN ?3 ELSE accessed_ms END WHERE id = ?1",
                params![id, hydrated, now_ms()],
            )
            .map(|_| ())
        })
    }

    // ---- activity ------------------------------------------------------------------------

    pub fn add_activity(
        &self,
        kind: ActivityKind,
        path: &str,
        bytes: u64,
        detail: Option<&str>,
    ) -> rusqlite::Result<Activity> {
        let at = now_ms();
        self.write(|tx| {
            tx.execute(
                "INSERT INTO activity (at_ms, kind, path, bytes, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![at, activity_kind_str(kind), path, bytes as i64, detail],
            )?;
            let id = tx.last_insert_rowid();
            // Keep the table bounded: drop everything older than the newest 1000 rows.
            if id % 100 == 0 {
                tx.execute("DELETE FROM activity WHERE id <= ?1", [id - 1000])?;
            }
            Ok(Activity { id, at_ms: at, kind, path: path.to_string(), bytes, detail: detail.map(str::to_string) })
        })
    }

    /// Newest first; pass the smallest id seen so far as `before_id` to page (keyset pagination).
    pub fn recent_activity(
        &self,
        limit: u32,
        before_id: Option<i64>,
    ) -> rusqlite::Result<Vec<Activity>> {
        self.read(|c| {
            let mut st = c.prepare_cached(
                "SELECT id, at_ms, kind, path, bytes, detail FROM activity WHERE id < ?1 ORDER BY id DESC LIMIT ?2",
            )?;
            st.query_map(params![before_id.unwrap_or(i64::MAX), limit.min(500)], |r| {
                Ok(Activity {
                    id: r.get(0)?,
                    at_ms: r.get(1)?,
                    kind: activity_kind_parse(&r.get::<_, String>(2)?),
                    path: r.get(3)?,
                    bytes: r.get::<_, i64>(4)? as u64,
                    detail: r.get(5)?,
                })
            })?
            .collect()
        })
    }

    // ---- issues --------------------------------------------------------------------------

    /// Creates or refreshes an issue identified by `dedupe_key`. Returns true if it is new.
    pub fn raise_issue(
        &self,
        kind: IssueKind,
        dedupe_key: &str,
        node_id: Option<NodeId>,
        path: Option<&str>,
        message: &str,
    ) -> rusqlite::Result<bool> {
        self.write(|tx| {
            let existed: bool =
                tx.query_row("SELECT EXISTS (SELECT 1 FROM issue WHERE dedupe_key = ?1)", [dedupe_key], |r| r.get(0))?;
            tx.execute(
                "INSERT INTO issue (kind, dedupe_key, node_id, path, message, created_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (dedupe_key) DO UPDATE SET message = excluded.message, path = excluded.path",
                params![issue_kind_str(kind), dedupe_key, node_id, path, message, now_ms()],
            )?;
            Ok(!existed)
        })
    }

    /// Returns true if an issue was removed.
    pub fn clear_issue(&self, dedupe_key: &str) -> rusqlite::Result<bool> {
        self.write(|tx| {
            tx.execute("DELETE FROM issue WHERE dedupe_key = ?1", [dedupe_key])
                .map(|n| n > 0)
        })
    }

    pub fn clear_issues_with_prefix(&self, prefix: &str) -> rusqlite::Result<usize> {
        self.write(|tx| {
            tx.execute(
                "DELETE FROM issue WHERE substr(dedupe_key, 1, length(?1)) = ?1",
                [prefix],
            )
        })
    }

    pub fn issue(&self, id: i64) -> rusqlite::Result<Option<(Issue, String, Option<NodeId>)>> {
        self.read(|c| {
            c.query_row(
                "SELECT id, kind, path, message, created_ms, dedupe_key, node_id FROM issue WHERE id = ?1",
                [id],
                |r| {
                    let kind = issue_kind_parse(&r.get::<_, String>(1)?);
                    Ok((
                        Issue { id: r.get(0)?, kind, path: r.get(2)?, message: r.get(3)?, created_ms: r.get(4)?, actions: issue_actions(kind) },
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()
        })
    }

    pub fn issues(&self) -> rusqlite::Result<Vec<Issue>> {
        self.read(|c| {
            let mut st = c.prepare_cached(
                "SELECT id, kind, path, message, created_ms FROM issue ORDER BY created_ms DESC LIMIT 200",
            )?;
            st.query_map([], |r| {
                let kind = issue_kind_parse(&r.get::<_, String>(1)?);
                Ok(Issue { id: r.get(0)?, kind, path: r.get(2)?, message: r.get(3)?, created_ms: r.get(4)?, actions: issue_actions(kind) })
            })?
            .collect()
        })
    }

    // ---- orphans ---------------------------------------------------------------------------

    pub fn orphan_ids(&self) -> rusqlite::Result<std::collections::HashSet<String>> {
        self.read(|c| {
            let mut st = c.prepare_cached("SELECT remote_id FROM orphan")?;
            st.query_map([], |r| r.get(0))?.collect()
        })
    }

    pub fn due_orphans(
        &self,
        now: i64,
        limit: u32,
    ) -> rusqlite::Result<Vec<crate::remote::RemoteEntry>> {
        self.read(|c| {
            let mut st = c.prepare_cached(
                "SELECT remote_id, kind, name, remote_parent_id, size FROM orphan WHERE retry_at_ms <= ?1 LIMIT ?2",
            )?;
            st.query_map(params![now, limit], |r| {
                Ok(crate::remote::RemoteEntry {
                    id: r.get(0)?,
                    kind: if r.get::<_, i64>(1)? == 1 { Kind::Folder } else { Kind::File },
                    name: r.get(2)?,
                    parent_id: r.get(3)?,
                    size: r.get::<_, i64>(4)? as u64,
                    mtime_ms: 0,
                })
            })?
            .collect()
        })
    }

    pub fn orphan_failed(&self, remote_id: &str, retry_at: i64) -> rusqlite::Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE orphan SET attempts = attempts + 1, retry_at_ms = ?2 WHERE remote_id = ?1",
                params![remote_id, retry_at],
            )
            .map(|_| ())
        })
    }

    pub fn remove_orphan(&self, remote_id: &str) -> rusqlite::Result<()> {
        self.write(|tx| {
            tx.execute("DELETE FROM orphan WHERE remote_id = ?1", [remote_id])
                .map(|_| ())
        })
    }

    pub fn issue_count(&self) -> rusqlite::Result<u32> {
        self.read(|c| c.query_row("SELECT COUNT(*) FROM issue", [], |r| r.get(0)))
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StorageTotals {
    pub cache_bytes: u64,
    pub pinned_bytes: u64,
    pub cloud_bytes: u64,
    pub cloud_files: u64,
}

pub fn add_orphan(tx: &Connection, e: &crate::remote::RemoteEntry) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO orphan (remote_id, kind, name, remote_parent_id, size) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![e.id, kind_int(e.kind), e.name, e.parent_id, e.size as i64],
    )
    .map(|_| ())
}

pub fn get_node(c: &Connection, id: NodeId) -> rusqlite::Result<Option<Node>> {
    c.prepare_cached(&format!("SELECT {NODE_COLUMNS} FROM node WHERE id = ?1"))?
        .query_row([id], Node::from_row)
        .optional()
}

pub fn get_child(c: &Connection, parent: NodeId, name: &str) -> rusqlite::Result<Option<Node>> {
    c.prepare_cached(&format!(
        "SELECT {NODE_COLUMNS} FROM node WHERE parent_id = ?1 AND name_key = ?2 AND deleted = 0"
    ))?
    .query_row(params![parent, name_key(name)], Node::from_row)
    .optional()
}

pub fn path_of(c: &Connection, id: NodeId) -> rusqlite::Result<Option<String>> {
    let mut st = c.prepare_cached(
        "WITH RECURSIVE up(id, parent_id, name, depth) AS (
             SELECT id, parent_id, name, 0 FROM node WHERE id = ?1
             UNION ALL
             SELECT n.id, n.parent_id, n.name, up.depth + 1 FROM node n JOIN up ON n.id = up.parent_id
         )
         SELECT name, parent_id FROM up ORDER BY depth DESC",
    )?;
    let rows: Vec<(String, Option<NodeId>)> = st
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    if rows.is_empty() {
        return Ok(None);
    }
    // The first row must be the root (no parent); otherwise the chain is broken.
    if rows[0].1.is_some() {
        return Ok(None);
    }
    Ok(Some(
        rows.iter()
            .skip(1)
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join("/"),
    ))
}

fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "database schema {version} is newer than this version of Drive supports ({SCHEMA_VERSION})"
        )));
    }
    if version < 2 {
        let v1 = version < 1;
        if v1 {
            migrate_v1(conn)?;
        }
        conn.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             {SCHEMA_V2}
             PRAGMA user_version = 2;
             COMMIT;"
        ))?;
    }
    Ok(())
}

fn migrate_v1(conn: &Connection) -> rusqlite::Result<()> {
    {
        conn.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             {SCHEMA_V1}
             INSERT INTO node (id, parent_id, name, name_key, kind, created_ms) VALUES (1, NULL, '', '', 1, {now});
             PRAGMA user_version = 1;
             COMMIT;",
            now = now_ms()
        ))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(db: &Db, parent: NodeId, name: &str, kind: Kind) -> NodeId {
        db.write(|tx| {
            tx.execute(
                "INSERT INTO node (parent_id, name, name_key, kind, created_ms) VALUES (?1, ?2, ?3, ?4, 0)",
                params![parent, name, name_key(name), kind_int(kind)],
            )?;
            Ok(tx.last_insert_rowid())
        })
        .unwrap()
    }

    #[test]
    fn paths_and_lookup() {
        let db = Db::open_in_memory().unwrap();
        let a = insert(&db, ROOT_ID, "Docs", Kind::Folder);
        let b = insert(&db, a, "Report.PDF", Kind::File);
        assert_eq!(db.path_of(b).unwrap().unwrap(), "Docs/Report.PDF");
        assert_eq!(db.path_of(ROOT_ID).unwrap().unwrap(), "");
        assert_eq!(db.node_by_path("docs/report.pdf").unwrap().unwrap().id, b);
        assert_eq!(db.node_by_path("").unwrap().unwrap().id, ROOT_ID);
        assert!(db.node_by_path("Docs/missing").unwrap().is_none());
        assert_eq!(db.children(a).unwrap().len(), 1);
    }

    #[test]
    fn case_insensitive_uniqueness() {
        let db = Db::open_in_memory().unwrap();
        insert(&db, ROOT_ID, "a.txt", Kind::File);
        let dup = db.write(|tx| {
            tx.execute(
                "INSERT INTO node (parent_id, name, name_key, kind, created_ms) VALUES (1, 'A.TXT', 'a.txt', 0, 0)",
                [],
            )
        });
        assert!(dup.is_err());
    }

    #[test]
    fn activity_and_issues() {
        let db = Db::open_in_memory().unwrap();
        for i in 0..5 {
            db.add_activity(ActivityKind::Uploaded, &format!("f{i}"), 10, None)
                .unwrap();
        }
        let page1 = db.recent_activity(2, None).unwrap();
        assert_eq!(
            page1.iter().map(|a| a.path.as_str()).collect::<Vec<_>>(),
            ["f4", "f3"]
        );
        let page2 = db.recent_activity(2, Some(page1[1].id)).unwrap();
        assert_eq!(page2[0].path, "f2");

        assert!(
            db.raise_issue(IssueKind::Auth, "auth", None, None, "Sign in again")
                .unwrap()
        );
        assert!(
            !db.raise_issue(IssueKind::Auth, "auth", None, None, "Sign in again!")
                .unwrap()
        );
        assert_eq!(db.issue_count().unwrap(), 1);
        assert_eq!(db.issues().unwrap()[0].message, "Sign in again!");
        assert!(db.clear_issue("auth").unwrap());
        assert_eq!(db.issue_count().unwrap(), 0);
    }

    /// Every hot query must be served by an index (no full scans of `node`).
    #[test]
    fn hot_queries_use_indexes() {
        let db = Db::open_in_memory().unwrap();
        let plan = |sql: &str| -> String {
            db.read(|c| {
                let mut st = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                let rows: Vec<String> = st
                    .query_map([], |r| r.get::<_, String>(3))?
                    .collect::<Result<_, _>>()?;
                Ok(rows.join(" | "))
            })
            .unwrap()
        };
        let cases = [
            (
                "SELECT id FROM node WHERE parent_id = 1 AND name_key = 'a' AND deleted = 0",
                "node_parent_name_key_idx",
            ),
            (
                "SELECT id FROM node WHERE parent_id = 1",
                "node_parent_id_idx",
            ),
            (
                "SELECT id FROM node WHERE pending != 0 AND retry_at_ms <= 5 ORDER BY retry_at_ms LIMIT 10",
                "node_pending_idx",
            ),
            (
                "SELECT id FROM node WHERE hydrated = 1 AND pinned = 0 AND kind = 0 AND dirty = 0 ORDER BY accessed_ms LIMIT 10",
                "node_lru_idx",
            ),
            (
                "SELECT id FROM node WHERE remote_id = 'x'",
                "sqlite_autoindex_node_1",
            ),
            (
                "SELECT id FROM activity WHERE id < 100 ORDER BY id DESC LIMIT 10",
                "INTEGER PRIMARY KEY",
            ),
        ];
        for (sql, index) in cases {
            let p = plan(sql);
            assert!(
                p.contains(index),
                "{sql}
  plan: {p}"
            );
            assert!(
                !p.contains("SCAN node") || p.contains("USING"),
                "{sql}
  full scan: {p}"
            );
        }
    }

    #[test]
    fn upgrades_a_v1_database_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.db");
        {
            let c = Connection::open(&p).unwrap();
            migrate_v1(&c).unwrap();
            c.execute(
                "INSERT INTO node (parent_id, name, name_key, kind, size, pending, created_ms) VALUES (1, 'a.mp4', 'a.mp4', 0, 5, 1, 0)",
                [],
            )
            .unwrap();
        }
        let db = Db::open(&p).unwrap();
        let n = db.node_by_path("a.mp4").unwrap().expect("row survives");
        assert_eq!((n.size, n.upload_mode), (5, upload_mode::AUTO));
        let v: i64 = db
            .read(|c| c.query_row("PRAGMA user_version", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        // Sessions round-trip and cascade with their node.
        let mut st = crate::remote::resumable::ResumeState::default();
        st.set("crypt", &serde_json::json!({"nonce": "00"}));
        db.save_upload_session(n.id, 5, 7, &st).unwrap();
        assert_eq!(db.upload_session(n.id).unwrap().unwrap().state, st);
        db.write(|tx| {
            tx.execute("DELETE FROM node WHERE id = ?1", [n.id])
                .map(|_| ())
        })
        .unwrap();
        assert!(db.upload_session(n.id).unwrap().is_none());
    }

    #[test]
    fn held_uploads_are_not_due() {
        let db = Db::open_in_memory().unwrap();
        let mk = |name: &str, size: i64, mode: i64| {
            db.write(|tx| {
                tx.execute(
                    "INSERT INTO node (parent_id, name, name_key, kind, size, pending, upload_mode, created_ms)
                     VALUES (1, ?1, ?1, 0, ?2, 1, ?3, 0)",
                    params![name, size, mode],
                )
                .map(|_| ())
            })
            .unwrap()
        };
        mk("small", 10, upload_mode::AUTO);
        mk("big", 1_000, upload_mode::AUTO);
        mk("paused", 10, upload_mode::PAUSED);
        mk("tonight", 10, upload_mode::TONIGHT);
        mk("forced", 1_000, upload_mode::NOW);
        let names = |gate: UploadGate| -> Vec<String> {
            let mut v: Vec<String> = db
                .due_work(i64::MAX, i64::MAX, 100, gate)
                .unwrap()
                .into_iter()
                .map(|n| n.name)
                .collect();
            v.sort();
            v
        };
        assert_eq!(
            names(UploadGate::default()),
            ["big", "forced", "small", "tonight"]
        );
        let closed = UploadGate {
            night_closed: true,
            night_min_bytes: 500,
        };
        assert_eq!(names(closed), ["forced", "small"]);
        // Forced uploads go first.
        assert_eq!(
            db.due_work(i64::MAX, i64::MAX, 1, closed).unwrap()[0].name,
            "forced"
        );
        let (all, count) = db.queued_uploads(10, closed).unwrap();
        assert_eq!(count, 5);
        // Held uploads (paused / waiting for the window) come last.
        let last = all.last().map(|n| n.name.as_str()).unwrap_or("");
        assert!(["big", "paused", "tonight"].contains(&last), "{last}");
    }

    #[test]
    fn file_db_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.db");
        {
            let db = Db::open(&p).unwrap();
            db.kv_set("device_id", "abc").unwrap();
        }
        let db = Db::open(&p).unwrap();
        assert_eq!(db.kv_get("device_id").unwrap().as_deref(), Some("abc"));
    }
}
