# Architecture

```
 Explorer / Nautilus / apps
          │  open, save, rename…
          ▼
 ┌────────────────────────┐        ┌──────────────────────────────┐
 │ Platform layer         │◄──────►│ Engine (drive-core)          │
 │ Windows: Cloud Files   │ Local- │ scanner · planner · worker   │
 │ Linux:   FUSE + cache  │ Side   │ evictor · status             │
 └────────────────────────┘        │ SQLite state (docs/database) │
                                    └──────────────┬───────────────┘
 ┌────────────────────────┐  IPC (local socket,    │ Remote trait
 │ Tray app (Tauri 2)     │◄─ JSON lines) ─────────┤
 └────────────────────────┘        ┌───────────────▼──────────────┐
 ┌────────────────────────┐        │ CryptRemote (rclone crypt)   │
 │ CLI (`drive …`)        │◄──IPC──│  └ UlozRemote (uloz.to API)  │
 └────────────────────────┘        └──────────────────────────────┘
```

## Crates

- **drive-core**: everything platform-independent.
  - `crypt`: rclone-compatible encryption (scrypt keys, EME file names, NaCl secretbox
    64 KiB blocks), cross-checked against the real rclone binary in tests.
  - `ulozto`: the uloz.to API client (sessions, pacing, recursive listings, uploads with
    MD5 verification, ranged downloads) and rclone's description metadata format.
  - `remote`: the `Remote` trait, `CryptRemote` and an in-memory/dev `MemRemote`.
  - `db`: the SQLite state (see [database.md](database.md)).
  - `engine`: the sync engine (below), `LocalSide` (platform contract) and the planner.
  - `ipc` / `ipc_client`: the protocol and client used by the tray app and the CLI.
- **drive-daemon**: the `drive` binary. `drive daemon` runs the service: config, secrets,
  account setup, IPC server, and the platform layer (`platform/windows`, `platform/linux`).
  The other subcommands are the CLI.
- **app/**: the Tauri 2 tray app. It talks to the service over IPC and starts it if needed.

## Sync model

uloz.to has no change feed, so the cloud is **polled** (default every 2 minutes, and right
away after local changes need it). Each scan lists the vault recursively with two API calls
(`depth=-1`). A per-folder walk is the fallback, and the result is cross-checked against the
server's file count so a partial listing is never mistaken for deletions.

The **planner** (`engine/plan.rs`) is a pure function from (local rows, remote listing) to
a list of changes. Because it is pure it is unit-tested in isolation:

| Situation | Result |
|---|---|
| New remote item | placeholder created locally |
| Moved/renamed remotely | moved locally, unless a local move is pending (local wins) |
| Replaced remotely, clean locally | placeholder updated, local copy dropped |
| Replaced remotely, edited locally | local version renamed `name (conflict DEVICE DATE).ext` and uploaded |
| Same name uploaded by two devices at once | the smaller remote id keeps the name on every device; the other becomes `name (conflicted copy).ext` |
| Deleted remotely, clean locally | removed locally |
| Deleted remotely, edited locally | kept and uploaded again |
| More than 100 files and over half the vault deleted remotely | held until the user confirms |
| Nothing decrypts with the saved password | nothing changes; "wrong password" issue |

The **worker** pushes rows with pending bits: folders first (they need a remote id before
their children), then uploads and moves, then trash. Concurrency is bounded and failures
back off exponentially. Before uploading an edit, the worker checks that the version it was
based on is still current; if not, it rescans first so the planner can turn the edit into a
conflicted copy. Re-uploading a file creates a new remote file (uloz.to files are
immutable). The previous version is trashed right away and stays in the uloz.to trash for
30 days. Local mass deletions (over 500 files) wait for confirmation.

The **evictor** keeps downloaded-but-unpinned files under the cache limit (LRU) and frees
more when the disk gets low. Pinned files ("Always keep on this device") are never
evicted.

## Folder backups

Folders outside the Drive folder are mirrored **one way** by a second engine
(`crates/drive-daemon/src/backup/`, `drive-core/src/engine/mirror.rs`):

```
 Documents, Desktop, …  ──read only──►  backup engine (mirror)  ──►  vault: Backups/<device>/<name>/…
                                          │ shares upload throttle + automatic upload speed
 Drive folder  ◄──two-way──►  main engine ┘ (excludes Backups/<this device>)
```

- **Mirror planner.** The local folder is the source of truth. Cloud items we uploaded that
  went missing are uploaded again. Items someone moved are moved back. Unknown items in a
  backed-up folder go to the trash, unless they match a not-yet-uploaded local file (same
  name, size and modification time); those are adopted without uploading. That makes
  "switch off and on again" and "reinstall" free. More than 200 removals that make up a
  quarter of the copy wait for confirmation.
- **Local side** (`backup/local.rs`) only reads. Every write method is a no-op, so even a
  planner bug can't touch the user's files.
- **Reading folders** (`backup/scan.rs`) prunes skipped folders without entering them
  (`node_modules`, `CACHEDIR.TAG`, `pyvenv.cfg`, the user's skip list). Junctions and
  symlinks are never followed, and other cloud apps' online-only files are skipped.
  Unreadable folders count as unknown, not deleted. A missing backed-up folder is reported
  and nothing is changed.
- **Change detection.** A recursive watch on Windows, one inotify watch per included folder on
  Linux, debounced and diffed against the database (shallow for changed folders, deep for
  new ones). A move becomes a move instead of a delete plus an upload. A full rescan runs
  every 30 minutes as a safety net, or every 5 minutes if the folders can't be watched.
- **Main engine exclusion.** `EngineConfig::exclude_remote/exclude_local` hide this device's
  own `Backups/<device>` from its Drive folder, so it isn't shown twice. Other devices see it
  as normal cloud files.
- **One view.** The service merges every engine's status, activity, issues and transfers.
  Backup ids are negative, `-(id * 1024 + slot)` (slot 0 is the mirror engine, 1.. are
  on-demand folders), so calls can be routed back to the right engine.

### Drive - Backups

`backup/links.rs` keeps a folder next to the Drive folder (`Drive - Backups`) with one link per
backed-up folder: a junction on Windows (no special rights needed), a symbolic link on Linux.
Only links are ever created or removed there, and removing one never touches its target. On
Windows it gets a sidebar entry of its own (`platform/windows/navpane.rs`): a per-user shell
folder that delegates to the file-system folder, the classic way cloud apps add one. The
Drive folder hides `Backups` entirely while only this device has backups
(`BackupCloud::other_devices`); with other devices it shows theirs.

### Save space (on-demand backed-up folders, Windows)

A backed-up folder can stay where it is but become on-demand like the Drive folder
(`service/on_demand.rs`):

- **Switching on.** The mirror engine forgets the folder (its cloud copy stays). The folder gets
  its own **two-way** engine on `Backups/<device>/<name>`, linked to the main one, and its own
  Cloud Files sync root (id `<login>~<backup id>`). Windows gives every sync root a sidebar
  entry; these are unpinned (`System.IsPinnedToNameSpaceTree = 0`), so the folders are reached
  through *Drive - Backups*. The mirror engine stops its uploads under the folder (and waits
  for them) before the on-demand engine lists the cloud copy, so no late upload lands as a
  duplicate. `scan_before_upload` holds uploads until the
  cloud has been listed, so files already backed up are **adopted** (same name, size and
  modification time) instead of uploaded again. The evictor then frees the files used least
  recently (new files count as last used when last modified) beyond `keep_bytes` (5 GB by
  default). The skip list still applies: skipped files stay ordinary files and never upload.
- **Switching off.** Refused unless the disk has room for everything only in the cloud plus
  1 GB. The folder is pinned, so everything downloads (shown as `converting`; survives a
  restart via `leaving_on_demand`). Once nothing is cloud-only, the folder is disconnected,
  every placeholder is reverted to an ordinary file (`CfRevertPlaceholder`, pin attributes
  cleared), the sync root is unregistered by its id, and the mirror engine takes the folder
  back, linking files without uploading them again. `platform::release` refuses to run while
  any file is still cloud-only.
- **Guards.** Turning the backup off, removing it, changing its skip list and signing out are
  refused while a folder is on-demand.
- **Linux** reports `on_demand_available = false`: a FUSE mount over an existing folder would
  hide its files.

## Encryption

Everything in the vault is encrypted on the device before upload, in exactly the format
`rclone crypt` uses (standard file-name encryption, directory names encrypted). The
encryption password never leaves the device unencrypted. On Windows it is protected with
DPAPI; on Linux it lives in a `0600` file. The salt is derived from the uloz.to login, so a
second device needs only the password. A **recovery kit** (`drive recovery`, or Settings →
Recovery) produces an `rclone.conf` that opens the vault with stock rclone, so your data
never depends on Drive itself.

The vault is the folder `Drive vault` in the uloz.to account. It also holds an encrypted
`.drive-vault` marker, which lets a new device check its password even when the vault is
otherwise empty.
