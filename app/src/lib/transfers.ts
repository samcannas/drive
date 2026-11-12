// Wording and grouping for the transfer list (ipc `TransferList`). Shared by the popover's
// compact section and its full transfers panel.

import { basename, bytes, count, duration, plural, rate, waitingForWindow } from "./format";
import type { IconName } from "./Icon.svelte";
import type { TransferItem, TransferList, TransferMode, TransferState } from "./types";

/** Preparing, sending or verifying: holds a transfer slot. */
export const isMoving = (s: TransferState) => s === "preparing" || s === "active" || s === "verifying";
/** Will start on its own once it can. */
export const isPending = (s: TransferState) => s === "queued" || s === "waiting";
/** Held by the user or by the overnight setting. */
export const isHeld = (s: TransferState) => s === "paused" || s === "scheduled";

export interface Summary {
  title: string;
  detail: string | null;
}

/**
 * One line for the whole list: "Uploading 3 files · about 25 min left", or when nothing moves,
 * what everything is waiting for.
 */
export function summarize(list: TransferList, device: string): Summary {
  const toGo = list.items.filter((t) => !isHeld(t.state));
  const ups = toGo.filter((t) => t.direction === "upload").length;
  const downs = toGo.length - ups;

  if (toGo.length) {
    if (!toGo.some((t) => isMoving(t.state))) {
      const what = ups ? plural(ups, "file", "files") : plural(downs, "file", "files");
      return { title: `Waiting to ${ups ? "upload" : "download"} ${what}`, detail: null };
    }
    // Uploads lead (they're what people wait for); downloads get a short mention below.
    let title = ups ? `Uploading ${plural(ups, "file", "files")}` : `Downloading ${plural(downs, "file", "files")}`;
    const eta = ups ? list.upload_eta_secs : null;
    if (eta !== null) title += ` · ${duration(eta, { about: true })} left`;
    const notes: string[] = [];
    if (ups && list.auto_upload_checking) notes.push("Paused for a moment to check your connection.");
    else if (ups && list.auto_upload_limit_bps)
      notes.push(`Slowed to ${rate(list.auto_upload_limit_bps)} so your connection stays responsive.`);
    if (ups && downs) notes.push(`Also downloading ${plural(downs, "file", "files")}.`);
    if (eta !== null && eta > 5 * 60) notes.push(`Keep your ${device} on until it's done.`);
    return { title, detail: notes.join(" ") || null };
  }

  const scheduled = list.items.filter((t) => t.state === "scheduled");
  const paused = list.items.filter((t) => t.state === "paused");
  const left = (items: TransferItem[]) => bytes(items.reduce((n, t) => n + Math.max(0, t.bytes_total - t.bytes_done), 0));
  if (scheduled.length) {
    const title =
      scheduled.find((t) => t.reason)?.reason ??
      (list.night_window_opens_ms ? waitingForWindow(list.night_window_opens_ms) : "Waiting for tonight");
    let detail = `${plural(scheduled.length, "file", "files")} · ${left(scheduled)}`;
    if (paused.length) detail += ` · ${count(paused.length)} paused`;
    return { title, detail };
  }
  return { title: "Paused", detail: `${plural(paused.length, "upload", "uploads")} · ${left(paused)} to go` };
}

/** What a row says under the file name. */
export function caption(t: TransferItem, now: number): string {
  const size = bytes(t.bytes_total);
  const progress = `${bytes(Math.min(t.bytes_done, t.bytes_total))} of ${size}`;
  switch (t.state) {
    case "active": {
      const parts = [progress];
      if (t.rate_bps > 0) parts.push(rate(t.rate_bps));
      if (t.eta_secs !== null) parts.push(t.eta_secs < 60 ? "almost done" : `${duration(t.eta_secs)} left`);
      return parts.join(" · ");
    }
    case "preparing":
      return t.reason ?? "Getting ready…";
    case "verifying":
      return t.reason ?? "Finishing up…";
    case "queued":
      return t.reason ?? `Waiting its turn · ${size}`;
    case "waiting": {
      if (t.reason) return t.reason;
      if (t.retry_at_ms) return `Trying again in ${Math.max(1, Math.round((t.retry_at_ms - now) / 1000))} s`;
      return "Waiting to start";
    }
    case "paused":
      return t.reason ?? `Paused · ${t.bytes_done > 0 ? progress : size}`;
    case "scheduled":
      return `${t.reason ?? "Waiting for tonight"} · ${t.bytes_done > 0 ? progress : size}`;
  }
}

/** Partly sent, not moving, and the service keeps the part that's done. */
export const resumes = (t: TransferItem) => t.resumable && t.bytes_done > 0 && t.bytes_done < t.bytes_total && !isMoving(t.state);

export function iconOf(t: TransferItem): IconName {
  switch (t.state) {
    case "waiting":
      return "clock";
    case "paused":
      return "pause";
    case "scheduled":
      return "moon";
    default:
      return t.direction === "upload" ? "upload" : "download";
  }
}

export type RowAction = "pause" | "resume" | "tonight" | "now" | "reveal";

export const ACTION_MODE: Record<Exclude<RowAction, "reveal">, TransferMode> = {
  pause: "paused",
  resume: "auto",
  tonight: "tonight",
  now: "now",
};

/** Menu items for one row. Downloads have none: the service decides when they run. */
export function actionsOf(t: TransferItem): RowAction[] {
  if (t.direction !== "upload") return [];
  const out: RowAction[] = [];
  switch (t.state) {
    case "verifying":
      break;
    case "paused":
      out.push("resume", "tonight");
      break;
    case "scheduled":
      out.push("now", "pause");
      break;
    default:
      out.push("pause");
      if (t.mode !== "tonight") out.push("tonight");
  }
  out.push("reveal");
  return out;
}

export function actionLabel(a: RowAction, fileManager: string): string {
  switch (a) {
    case "pause":
      return "Pause";
    case "resume":
      return "Resume";
    case "tonight":
      return "Upload tonight";
    case "now":
      return "Upload now";
    case "reveal":
      return `Show in ${fileManager}`;
  }
}

/** What a screen reader hears after an action. */
export function actionDone(a: RowAction, t: TransferItem): string {
  const name = basename(t.path);
  switch (a) {
    case "pause":
      return `Paused ${name}`;
    case "resume":
      return `Resumed ${name}`;
    case "tonight":
      return `${name} uploads tonight`;
    case "now":
      return `Uploading ${name} now`;
    case "reveal":
      return "";
  }
}

export interface Group {
  key: string;
  label: string;
  items: TransferItem[];
}

export function groupsOf(list: TransferList): Group[] {
  return [
    { key: "active", label: "Active", items: list.items.filter((t) => isMoving(t.state)) },
    { key: "waiting", label: "Waiting", items: list.items.filter((t) => isPending(t.state)) },
    { key: "held", label: "Paused and scheduled", items: list.items.filter((t) => isHeld(t.state)) },
  ].filter((g) => g.items.length > 0);
}

/** Moving first, then waiting, then held; the service already sends them that way. */
export function ordered(items: TransferItem[]): TransferItem[] {
  const rank = (t: TransferItem) => (isMoving(t.state) ? 0 : isPending(t.state) ? 1 : 2);
  return [...items].sort((a, b) => rank(a) - rank(b));
}
