# Local database

Drive keeps its sync state in one SQLite file per device (`%LOCALAPPDATA%\Drive\state.db`,
`~/.local/share/Drive/state.db`). Schema and queries live in
[`crates/drive-core/src/db.rs`](../crates/drive-core/src/db.rs).

## Model

| Table      | One row per                                   | Notes |
|------------|-----------------------------------------------|-------|
| `node`     | file or folder in the vault (+ tombstones)    | local location, last-synced remote location, pending work, cache state |
| `orphan`   | superseded remote version still to be trashed | scans ignore these ids |
| `activity` | recent event shown in the tray                | capped at ~1000 rows |
| `issue`    | thing that needs the user's attention         | `dedupe_key` makes raising idempotent |
| `kv`       | small settings (`last_sync_ms`, `paused`, …)  | `WITHOUT ROWID` |

Pending work is **state, not a queue**: `node.pending` is a bitmask (`1` push, `2` move,
`4` trash) next to the remote location as of the last sync (`remote_parent_id`,
`remote_name`). The worker converges the cloud to the rows. A crash at any point leaves a
consistent database, because the next run simply redoes whatever is still pending.
Concurrent local edits during a transfer are detected with `generation` (bumped by every
local change), and our own fresh remote operations are protected from stale scans by
`op_seq`.

## Guidance applied (from the Postgres / database-designer playbooks, adapted to SQLite)

**Schema**
- `INTEGER PRIMARY KEY` (rowid alias, 64-bit) for every table. No random UUID keys: remote
  slugs are stored as a `UNIQUE TEXT` attribute, not used as the key.
- `STRICT` tables, `NOT NULL` wherever possible, and `CHECK` constraints instead of
  enum types (`kind IN (0,1)`, `pending BETWEEN 0 AND 7`, activity/issue kinds).
- Timestamps are `INTEGER` Unix milliseconds in UTC (SQLite has no `TIMESTAMPTZ`).
- Foreign keys are enforced (`PRAGMA foreign_keys = ON`) with explicit
  `ON DELETE CASCADE`, and every FK column is indexed (`node_parent_id_idx`,
  `issue_node_id_idx`). Deleting a folder row removes its subtree in one statement.
- Names: singular snake_case tables and columns, `{table}_{column}_idx` indexes.

**Indexes: one per hot query, no more**
- `node_parent_name_key_idx (parent_id, name_key) WHERE deleted = 0`: unique and partial.
  It serves FUSE `lookup`, path resolution and collision checks, and enforces
  case-insensitive uniqueness among live items while ignoring tombstones.
- `node_pending_idx (retry_at_ms) WHERE pending != 0`: the work queue only indexes rows
  that have work.
- `node_lru_idx (accessed_ms) WHERE hydrated = 1 AND pinned = 0 AND kind = 0`: eviction
  candidates in LRU order.
- Equality columns come first in composite indexes, then the range/sort column.
- Predicates are SARGable: case-insensitive lookups compare a stored `name_key` instead of
  calling `lower(name)` in `WHERE`.

**Queries**
- Explicit column lists (`NODE_COLUMNS`), never `SELECT *`.
- Every list query has a `LIMIT`. Activity uses keyset pagination
  (`WHERE id < ? ORDER BY id DESC`), not `OFFSET`.
- No N+1: a scan loads the whole node table in one query, plans in memory, and folds the
  result back in. Subtree operations use one recursive CTE, not a loop of queries.
- Prepared statements are cached (`prepare_cached`) on hot paths.

**Transactions and durability** (the SQLite counterparts of the MVCC/WAL advice)
- WAL journal with `synchronous = NORMAL`: durable across application crashes, fast
  commits. `fsync` is never disabled.
- One writer connection and one reader connection. Readers never block the writer, which
  matters for the FUSE layer.
- Transactions are short and are never held across network I/O. Long-lived readers would
  pin the WAL and grow it, the SQLite version of a long transaction blocking `VACUUM`.
- `BEGIN IMMEDIATE` for writes, plus `busy_timeout`, instead of failing on contention.
- `PRAGMA optimize` runs periodically and on shutdown to keep planner statistics fresh.

**Migrations**
- `PRAGMA user_version` tracks the schema version. A newer database is refused rather than
  silently misread. Future changes use expand–contract: add the new column/table, backfill,
  switch reads, drop the old one in a later release.

**Not applicable here:** connection pooling, partitioning, replication, autovacuum or XID
tuning, and PgBouncer. A single-user embedded database with tens of thousands of rows needs
none of them.

## Inspecting a database

```sh
sqlite3 state.db "SELECT id, name, pending, dirty, hydrated, pinned FROM node LIMIT 20;"
sqlite3 state.db "EXPLAIN QUERY PLAN SELECT * FROM node WHERE parent_id = 1 AND name_key = 'docs' AND deleted = 0;"
```
