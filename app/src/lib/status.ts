import type { IconName } from "./Icon.svelte";
import type { ActivityKind, ConnectionInfo, IssueAction, IssueKind, Status } from "./types";

export type Tone = "success" | "accent" | "neutral" | "warning" | "danger";

export interface StateVisual {
  icon: IconName;
  tone: Tone;
  headline: string;
  /** Bytes are moving right now (the only time anything animates). */
  active: boolean;
}

export function stateVisual(status: Status | null, conn: ConnectionInfo): StateVisual {
  if (conn.state === "unavailable") return { icon: "alert", tone: "danger", headline: "Drive isn't running", active: false };
  if (conn.state !== "connected" || !status) return { icon: "cloud", tone: "neutral", headline: "Starting…", active: false };
  const headline = status.headline;
  switch (status.state) {
    case "up_to_date":
      return { icon: "cloud-check", tone: "success", headline, active: false };
    case "syncing":
      return { icon: "sync", tone: "accent", headline, active: status.uploading + status.downloading > 0 };
    case "paused":
      return { icon: "pause", tone: "neutral", headline, active: false };
    case "offline":
      return { icon: "cloud-off", tone: "neutral", headline, active: false };
    case "signed_out":
      return { icon: "user", tone: "warning", headline, active: false };
    case "error":
      return { icon: "alert", tone: "danger", headline, active: false };
    case "not_set_up":
      return { icon: "cloud", tone: "neutral", headline, active: false };
  }
}

export const ACTIVITY: Record<ActivityKind, { icon: IconName; verb: string }> = {
  uploaded: { icon: "upload", verb: "Uploaded" },
  downloaded: { icon: "download", verb: "Downloaded" },
  created_folder: { icon: "folder-plus", verb: "Folder created" },
  moved: { icon: "move", verb: "Moved" },
  deleted: { icon: "trash", verb: "Deleted" },
  conflict: { icon: "copy", verb: "Edited in two places" },
  evicted: { icon: "cloud", verb: "Space freed up" },
  failed: { icon: "alert", verb: "Couldn't sync" },
};

export const ISSUE_TITLE: Record<IssueKind, string> = {
  transfer_failed: "Couldn't sync a file",
  invalid_name: "Name can't be synced",
  conflict: "Edited in two places",
  mass_delete: "Many items deleted",
  wrong_password: "Encryption password needed",
  auth: "Sign in again",
  disk_full: "Disk almost full",
  file_locked: "File in use",
  other: "Needs attention",
};

export const ACTION_LABEL: Record<IssueAction, string> = {
  retry: "Try again",
  dismiss: "Dismiss",
  confirm_delete: "Delete from cloud",
  restore_deleted: "Restore files",
  sign_in: "Sign in",
};
