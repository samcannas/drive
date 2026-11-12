// Wording and arithmetic for the disk space advisor (ipc `SpaceReport`). Shared by the popover
// and Settings › Disk space.

import { bytes, GB } from "./format";
import type { DiskUsage, SpaceCategory, SpaceItem, SpaceReport, SpaceScan, Status } from "./types";

const locale = typeof navigator !== "undefined" ? navigator.language : "en";

/** How much of an on-demand folder stays on this device unless the person picks another size. */
export const KEEP_DEFAULT = 5 * GB;
/** The "Keep recently used files up to" choices (also used by Settings › Backups). */
export const KEEP_CHOICES = [1, 2, 5, 10, 20, 50].map((n) => n * GB);

/** Almost full: less than 10% or 10 GB free. */
export function diskLow(free: number, total: number): boolean {
  return total > 0 && (free < 10 * GB || free / total < 0.1);
}

/** The disk line from Status, when the service measured it. */
export function statusDisk(s: Status | null): { free: number; total: number; low: boolean } | null {
  const free = s?.disk_free_bytes;
  const total = s?.disk_total_bytes;
  if (free == null || !total) return null;
  return { free, total, low: diskLow(free, total) };
}

/** "51 GB free of 953 GB". */
export const freeOf = (free: number, total: number) => `${bytes(free)} free of ${bytes(total)}`;

/** A short, plain description of a category, shown under an item's name. */
export function categoryLabel(c: SpaceCategory, windows: boolean): string {
  switch (c) {
    case "user_folder":
      return "Your files";
    case "drive_folder":
      return "Downloaded from your Drive";
    case "backup":
      return "Backed-up folder";
    case "developer":
      return "Code and developer tools";
    case "apps":
      return windows ? "Programs and games" : "Programs";
    case "app_data":
      return "App data and caches";
    case "temporary":
      return "Temporary files";
    case "recycle_bin":
      return "Deleted files";
    case "other_cloud":
      return "Another cloud app";
    case "other_users":
      return "Other people's files";
    case "system":
      return windows ? "Windows" : "System";
    case "other":
      return "Other files";
  }
}

/** Colour groups for the disk bar; each has a `--space-<id>` token in app.css. */
export type Group = "files" | "drive" | "apps" | "cloud" | "system" | "other";

export function groupOf(c: SpaceCategory): Group {
  switch (c) {
    case "user_folder":
    case "backup":
    case "developer":
      return "files";
    case "drive_folder":
      return "drive";
    case "apps":
    case "app_data":
    case "temporary":
      return "apps";
    case "other_cloud":
      return "cloud";
    case "system":
      return "system";
    default:
      return "other";
  }
}

export function groupLabel(g: Group, windows: boolean): string {
  switch (g) {
    case "files":
      return "Your files";
    case "drive":
      return "Drive folder";
    case "apps":
      return "Apps and app data";
    case "cloud":
      return "Other cloud apps";
    case "system":
      return windows ? "Windows" : "System";
    case "other":
      return "Everything else";
  }
}

const sep = (p: string) => p.replace(/[\\/]+$/, "").toLowerCase();

/** Whether an item is on a disk. Buckets without a path count toward the first scanned disk. */
function onDisk(item: SpaceItem, disk: DiskUsage, first: DiskUsage | undefined, windows: boolean): boolean {
  if (!item.path) return disk === first;
  if (!windows) return disk === first; // One tree on Linux; the report's disk is "/".
  const root = sep(disk.path);
  const p = item.path.toLowerCase();
  return p === root || p.startsWith(`${root}\\`) || p.startsWith(`${root}/`);
}

export interface Segment {
  group: Group;
  label: string;
  bytes: number;
}

/** What a scanned disk holds, by colour group, largest first; the unlisted rest is "Everything else". */
export function breakdown(report: SpaceReport, disk: DiskUsage, windows: boolean): Segment[] {
  const first = report.disks.find((d) => d.scanned);
  const sums = new Map<Group, number>();
  for (const item of report.items) {
    if (!onDisk(item, disk, first, windows)) continue;
    const g = groupOf(item.category);
    sums.set(g, (sums.get(g) ?? 0) + item.bytes);
  }
  const used = Math.max(0, disk.total_bytes - disk.free_bytes);
  const listed = [...sums.values()].reduce((a, b) => a + b, 0);
  if (used > listed) sums.set("other", (sums.get("other") ?? 0) + used - listed);
  return [...sums]
    .filter(([, b]) => b > 0)
    .map(([group, b]) => ({ group, label: groupLabel(group, windows), bytes: b }))
    .sort((a, b) => b.bytes - a.bytes);
}

/** About how much saving space frees when `keep` stays on this device (the report assumes 5 GB). */
export function freesWith(item: SpaceItem, keep: number): number {
  return Math.max(0, Math.min(item.bytes, item.frees_bytes + KEEP_DEFAULT - keep));
}

const compact = new Intl.NumberFormat(locale, { notation: "compact", compactDisplay: "long", maximumFractionDigits: 1 });
const whole = new Intl.NumberFormat(locale);
const rules = new Intl.PluralRules(locale);

/** 1_200_000 → "1.2 million files"; 483_600 → "483,600 files". */
export function filesCompact(n: number): string {
  const num = n >= 1_000_000 ? compact.format(n) : whole.format(n);
  return `${num} ${rules.select(n) === "one" ? "file" : "files"}`;
}

/** "412 GB, 1.2 million files" for the scan's progress line. */
export function scanProgress(scan: SpaceScan): string {
  return `${bytes(scan.scanned_bytes)}, ${filesCompact(scan.scanned_files)}`;
}

/** Share of the scanned disks' used space read so far (0–1), or null when unknown. */
export function scanShare(report: SpaceReport): number | null {
  const used = report.disks.filter((d) => d.scanned).reduce((n, d) => n + Math.max(0, d.total_bytes - d.free_bytes), 0);
  if (used <= 0) return null;
  // Some of the disk can't be read (system files), so a running scan never claims to be done.
  return Math.min(0.99, report.scan.scanned_bytes / used);
}

/** The report has results to show (from this scan or the previous one). */
export const hasResults = (r: SpaceReport | null): r is SpaceReport =>
  r !== null && (r.scan.finished_ms !== null || r.items.length > 0);
