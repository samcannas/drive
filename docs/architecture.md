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
