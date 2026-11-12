// Mirrors crates/drive-core/src/ipc.rs and the app-side types in app/src-tauri/src.

export type SyncState = "not_set_up" | "signed_out" | "error" | "offline" | "paused" | "syncing" | "up_to_date";

export interface Status {
  state: SyncState;
  headline: string;
  account: string | null;
  folder: string | null;
  uploading: number;
  downloading: number;
  pending: number;
  transfer_bytes_total: number;
  transfer_bytes_done: number;
  issues: number;
  cache_bytes: number;
  cache_limit_bytes: number;
  pinned_bytes: number;
  cloud_bytes: number;
  cloud_files: number;
  last_sync_ms: number | null;
  pending_delete_confirmation: number | null;
}

export interface Settings {
  folder: string;
  cache_limit_bytes: number;
  poll_interval_secs: number;
  upload_limit_kbps: number;
  download_limit_kbps: number;
  /** Slow uploads down while they make the connection sluggish (and only then). */
  adaptive_upload: boolean;
  max_parallel_transfers: number;
  launch_at_login: boolean;
  notifications: boolean;
  device_name: string;
  app_token_custom: boolean;
  close_action: CloseAction;
  /** Download updates in the background and offer to restart; when off, only notify. */
  auto_update: boolean;
  /** Hold uploads of large files until the overnight window. */
  night_uploads: boolean;
  /** Window start and end, local time, "HH:MM". The end may be earlier than the start (wraps past midnight). */
  night_start: string;
  night_end: string;
  /** Files at least this large wait for the window when `night_uploads` is on. */
  night_min_bytes: number;
}

/** What the window's close (X) button does: hide to the tray, or quit Drive. */
export type CloseAction = "minimize" | "quit";

export type SettingsPatch = Partial<Omit<Settings, "folder" | "app_token_custom">> & { app_token?: string };

export type TransferDirection = "upload" | "download";

/** What a transfer is doing right now (ipc `TransferState`). */
export type TransferState = "preparing" | "active" | "verifying" | "queued" | "waiting" | "paused" | "scheduled";

/** The user's choice for one upload: follow settings, paused, wait for tonight, or go now. */
export type TransferMode = "auto" | "paused" | "tonight" | "now";

export interface TransferItem {
  /** Stable id for actions (the file's node id). */
  id: number;
  /** Relative to the Drive folder, `/`-separated. */
  path: string;
  direction: TransferDirection;
  state: TransferState;
  /** Plain-language explanation from the service for waiting / scheduled (and a hint for others). */
  reason: string | null;
  bytes_done: number;
  bytes_total: number;
  rate_bps: number;
  eta_secs: number | null;
  /** Unix ms of the next automatic retry. */
  retry_at_ms: number | null;
  /** Progress is kept if Drive restarts or the connection drops. */
  resumable: boolean;
  mode: TransferMode;
}

export interface TransferList {
  /** Active first, then queued/waiting, then paused/scheduled; at most 200 entries. */
  items: TransferItem[];
  /** Total number of transfers (may exceed `items.length`). */
  total_count: number;
  upload_rate_bps: number;
  download_rate_bps: number;
  /** Bytes still to upload for everything that isn't paused or scheduled. */
  upload_remaining_bytes: number;
  upload_eta_secs: number | null;
  /** Unix ms when the overnight window next opens (night uploads on and the window closed). */
  night_window_opens_ms: number | null;
  night_window_open: boolean;
  /** Uploads are slowed to this many bytes/s to keep the connection responsive. */
  auto_upload_limit_bps: number | null;
  /** Uploads are paused for a few seconds to check whether they slow the connection. */
  auto_upload_checking: boolean;
}

export type ActivityKind =
  | "uploaded"
  | "downloaded"
  | "created_folder"
  | "moved"
  | "deleted"
  | "conflict"
  | "evicted"
  | "failed";

export interface Activity {
  id: number;
  at_ms: number;
  kind: ActivityKind;
  path: string;
  bytes: number;
  detail: string | null;
}

export type IssueKind =
  | "transfer_failed"
  | "invalid_name"
  | "conflict"
  | "mass_delete"
  | "wrong_password"
  | "auth"
  | "disk_full"
  | "file_locked"
  | "other";

export type IssueAction = "retry" | "dismiss" | "confirm_delete" | "restore_deleted" | "sign_in";

export interface Issue {
  id: number;
  kind: IssueKind;
  path: string | null;
  message: string;
  created_ms: number;
  actions: IssueAction[];
}

export interface AccountCheck {
  login: string;
  vault_exists: boolean;
  vault_has_files: boolean;
}

export interface SetupRequest {
  login: string;
  password: string;
  encryption_password: string | null;
  folder: string | null;
  app_token: string | null;
}

export interface SetupResult {
  folder: string;
  generated_encryption_password: string | null;
  created_vault: boolean;
}

export type Link = "connecting" | "connected" | "unavailable";

export interface ConnectionInfo {
  state: Link;
  mock: boolean;
  service_version: string | null;
}

export interface AppInfo {
  version: string;
  platform: string;
  repository: string;
  connection: ConnectionInfo;
}

/** The in-app updater's state (app/src-tauri/src/updater.rs `UpdateStatus`). Dates are RFC 3339. */
export type UpdateState =
  | { state: "disabled"; reason: string }
  | { state: "idle" }
  | { state: "checking" }
  | { state: "up_to_date"; checked_ms: number }
  | { state: "available"; version: string; date: string | null; notes: string[] }
  | { state: "downloading"; version: string; done_bytes: number; total_bytes: number | null }
  | { state: "ready"; version: string; date: string | null; notes: string[] }
  | { state: "installing"; version: string }
  /** `version` is set while an update is still on offer: retrying installs it. */
  | { state: "error"; message: string; version: string | null };

/** A release newer than the running version, from the release changelog. */
export interface Release {
  version: string;
  date: string | null;
  url: string | null;
  notes: string[];
}

/** Error shape returned by every command. `code` is an ipc ErrorCode, or unavailable / timeout. */
export interface CallError {
  code: string;
  message: string;
}

export type View = "popover" | "settings" | "onboarding";
export type Material = "acrylic" | "mica" | "none";

export interface DriveEnv {
  view: View;
  material: Material;
  platform: string;
}
