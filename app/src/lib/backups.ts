// Wording for backed-up folders (ipc `BackupOverview`). Shared by the popover and Settings.

import { bytes, count, plural, relative } from "./format";
import type { IconName } from "./Icon.svelte";
import type { Tone } from "./status";
import type { BackupInfo, BackupOverview, FolderSuggestion, KnownFolder } from "./types";

const locale = typeof navigator !== "undefined" ? navigator.language : "en";
const list = new Intl.ListFormat(locale, { type: "conjunction" });

/** Order people think of these folders in (not by size). */
const KNOWN_ORDER: KnownFolder[] = ["desktop", "documents", "downloads", "pictures", "videos", "music"];

/** Scanning or uploading right now. */
export const isBusy = (b: BackupInfo) => b.state === "scanning" || b.state === "backing_up";

/** On-demand: bytes of the folder that are only in the cloud right now. */
export const cloudOnlyBytes = (b: BackupInfo) => Math.max(0, b.bytes - b.local_bytes);

/** On-demand: "4.8 GB on this PC · 90.8 GB only in the cloud". */
export function storageLine(b: BackupInfo, device: string): string {
  return `${bytes(b.local_bytes)} on this ${device} · ${bytes(cloudOnlyBytes(b))} only in the cloud`;
}

export interface BackupLine {
  icon: IconName;
  tone: Tone;
  text: string;
}

/** One line for a folder's state, e.g. "Backed up 5 minutes ago" or "1,204 of 98,000 files". */
export function stateLine(b: BackupInfo, now: number): BackupLine {
  switch (b.state) {
    case "scanning":
      return { icon: "sync", tone: "accent", text: b.detail ?? "Looking through the folder…" };
    case "backing_up":
      return {
        icon: "sync",
        tone: "accent",
        text: `Backing up · ${count(b.done_files)} of ${plural(b.files, "file", "files")} · ${bytes(b.done_bytes)} of ${bytes(b.bytes)}`,
      };
    case "up_to_date":
      return {
        icon: "check-circle",
        tone: "success",
        text: b.last_complete_ms ? `Backed up ${relative(b.last_complete_ms, now)}` : "Up to date",
      };
    case "off":
      return {
        icon: "pause",
        tone: "neutral",
        text: b.last_complete_ms
          ? `Off · Last backed up ${relative(b.last_complete_ms, now)}. The cloud copy is kept.`
          : "Off · The cloud copy is kept.",
      };
    case "waiting":
      return { icon: "clock", tone: "neutral", text: b.detail ?? "Waiting to continue" };
    case "problem":
      return { icon: "warning", tone: "warning", text: b.detail ?? "Drive can't read this folder." };
    case "converting":
      // The service says which way, e.g. "Downloading 12.4 GB back to this PC…".
      return {
        icon: "sync",
        tone: "accent",
        text: b.detail ?? (b.on_demand ? "Freeing up space…" : "Downloading files back…"),
      };
  }
}

/** Share of files already in the cloud, 0–1. */
export function progressOf(b: BackupInfo): number {
  return b.files > 0 ? Math.min(1, b.done_files / b.files) : 0;
}

/** Suggestions that can be backed up as they are, in the usual folder order. */
export function offerable(o: BackupOverview): FolderSuggestion[] {
  return o.suggestions
    .filter((s) => !s.blocked)
    .sort((a, b) => KNOWN_ORDER.indexOf(a.known) - KNOWN_ORDER.indexOf(b.known));
}

/** "Desktop, Documents and Downloads aren't backed up" / "Desktop, Documents and 3 other folders aren't backed up". */
export function offerTitle(items: FolderSuggestion[]): string {
  if (items.length === 1) return `${items[0].name} isn't backed up`;
  if (items.length <= 3) return `${list.format(items.map((s) => s.name))} aren't backed up`;
  const others = items.length - 2;
  return `${list.format([...items.slice(0, 2).map((s) => s.name), plural(others, "other folder", "other folders")])} aren't backed up`;
}

/** Total known size of the suggestions; null while all are still being measured. */
export function offerBytes(items: FolderSuggestion[]): number | null {
  const known = items.filter((s) => s.bytes !== null);
  return known.length ? known.reduce((n, s) => n + (s.bytes ?? 0), 0) : null;
}

/** Size line for a suggestion: "449 files · 410 MB", or "Measuring…". */
export function suggestionSize(s: FolderSuggestion): string {
  if (s.files === null || s.bytes === null) return "Measuring…";
  return `${plural(s.files, "file", "files")} · ${bytes(s.bytes)}`;
}

/**
 * The service shows backup transfers as "<backup name> › rel/path". Returns the backup and the
 * path inside it, or null for an ordinary Drive path.
 */
export function splitBackupPath(path: string, o: BackupOverview | null): { backup: BackupInfo; rel: string } | null {
  const at = path.indexOf(" › ");
  if (at < 0 || !o) return null;
  const name = path.slice(0, at);
  const backup = o.folders.find((f) => f.name === name);
  return backup ? { backup, rel: path.slice(at + 3) } : null;
}
