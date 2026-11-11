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
  max_parallel_transfers: number;
  launch_at_login: boolean;
  notifications: boolean;
  device_name: string;
  app_token_custom: boolean;
  close_action: CloseAction;
  /** Download updates in the background and offer to restart; when off, only notify. */
  auto_update: boolean;
}

/** What the window's close (X) button does: hide to the tray, or quit Drive. */
export type CloseAction = "minimize" | "quit";

export type SettingsPatch = Partial<Omit<Settings, "folder" | "app_token_custom">> & { app_token?: string };

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
