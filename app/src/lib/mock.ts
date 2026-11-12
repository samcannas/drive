// Browser-only stand-in for the Tauri backend, used by `pnpm dev` outside Tauri.
// Mirrors app/src-tauri/src/mock.rs closely enough to develop and screenshot every view.
//
// ?scenario= onboarding | massdelete | offline | signedout | paused | error | unavailable | idle
//            update (an update is available, automatic updates off) | update-ready (downloaded)
//            update-error | update-off (this installation doesn't update itself)
//            transfers (a long transfer list: one item in every state, a paused upload, large
//            files held for tonight, a partly done one that resumes) | transfers-night (only
//            uploads held for tonight) | transfers-paused (only paused uploads)
//            backups (backed-up folders in every state: backing up, up to date, off, folder missing)
//            backup-suggest (no backups yet; the popover suggests backing up Desktop, Documents…)
//            on-demand (Downloads saves space on this PC, Documents is a full copy; turning on takes
//            about 5 s, turning Downloads off fails for lack of disk space). Add &platform=linux to
//            see the switch unavailable.

import type {
  Activity,
  BackupInfo,
  BackupOverview,
  FolderSuggestion,
  Issue,
  KnownFolder,
  Release,
  Settings,
  Status,
  TransferDirection,
  TransferItem,
  TransferList,
  TransferMode,
  TransferState,
  UpdateState,
} from "./types";
import { bytes } from "./format";

const GB = 1024 ** 3;
const MB = 1024 ** 2;
const scenario = new URLSearchParams(location.search).get("scenario") ?? "";
const platform = new URLSearchParams(location.search).get("platform") ?? "windows";

type Handler = (p: unknown) => void;
const handlers = new Map<string, Set<Handler>>();

export function listen(event: string, h: Handler): () => void {
  if (!handlers.has(event)) handlers.set(event, new Set());
  handlers.get(event)!.add(h);
  return () => handlers.get(event)?.delete(h);
}

function emit(event: string, payload: unknown) {
  handlers.get(event)?.forEach((h) => h(payload));
}

const now = Date.now();
const state = {
  setUp: scenario !== "onboarding",
  paused: scenario === "paused",
  tick: scenario === "idle" ? 20 : 3,
  account: "alex.novak",
  settings: {
    folder: "C:\\Users\\alex\\Drive",
    cache_limit_bytes: 20 * GB,
    poll_interval_secs: 60,
    upload_limit_kbps: 0,
    download_limit_kbps: 0,
    adaptive_upload: true,
    max_parallel_transfers: 4,
    launch_at_login: true,
    notifications: true,
    device_name: "ALEX-DESKTOP",
    app_token_custom: false,
    close_action: "minimize",
    auto_update: scenario !== "update",
    night_uploads: scenario.startsWith("transfers"),
    night_start: "01:00",
    night_end: "07:00",
    night_min_bytes: 200 * MB,
  } as Settings,
  activity: [
    { id: 5, at_ms: now - 2 * 60_000, kind: "uploaded", path: "Documents/Meeting notes.docx", bytes: 48_000, detail: null },
    { id: 4, at_ms: now - 26 * 60_000, kind: "downloaded", path: "Photos/2026/Lisbon/IMG_4790.HEIC", bytes: 3_100_000, detail: null },
    { id: 3, at_ms: now - 3 * 3_600_000, kind: "created_folder", path: "Projects/Garden", bytes: 0, detail: null },
    { id: 2, at_ms: now - 5 * 3_600_000, kind: "moved", path: "Documents/Lease.pdf", bytes: 220_000, detail: null },
    { id: 1, at_ms: now - 26 * 3_600_000, kind: "conflict", path: "Notes/Shopping list.txt", bytes: 900, detail: "Both copies were kept." },
  ] as Activity[],
  issues: [
    {
      id: 1,
      kind: "file_locked",
      path: "Documents/Budget 2026.xlsx",
      message: "Budget 2026.xlsx is open in another app. Drive will upload it after you close it.",
      created_ms: now - 4 * 60_000,
      actions: ["retry", "dismiss"],
    },
  ] as Issue[],
  pendingDelete: null as number | null,
};

if (scenario === "backups") {
  state.activity.unshift(
    { id: 6, at_ms: now - 9 * 60_000, kind: "deleted", path: "Drafts/Old cover letter.docx", backup: "b1", bytes: 31_000, detail: null },
    { id: 7, at_ms: now - 60_000, kind: "uploaded", path: "Taxes/2025/Receipts/Pharmacy.pdf", backup: "b1", bytes: 220_000, detail: null },
  );
  state.activity.sort((x, y) => y.at_ms - x.at_ms);
}
if (scenario === "massdelete") {
  state.pendingDelete = 1240;
  state.issues.push({
    id: 2,
    kind: "mass_delete",
    path: "Photos",
    message: "1,240 items were deleted from your Drive folder. Drive is waiting for you before deleting them from the cloud.",
    created_ms: now - 60_000,
    actions: ["confirm_delete", "restore_deleted"],
  });
}
if (scenario === "signedout") {
  state.issues = [
    { id: 3, kind: "auth", path: null, message: "uloz.to stopped accepting your password. Sign in again to keep syncing.", created_ms: now - 600_000, actions: ["sign_in"] },
  ];
}

// ----- Backups of folders outside the Drive folder

const DEFAULT_EXCLUDES = [
  "node_modules",
  "__pycache__",
  ".cache",
  ".gradle",
  ".tox",
  "bower_components",
  "*.tmp",
  "~$*",
  "Thumbs.db",
  "desktop.ini",
  ".DS_Store",
];
const HOME = "C:\\Users\\alex";
const KNOWN: FolderSuggestion[] = [
  { known: "desktop", name: "Desktop", path: `${HOME}\\Desktop`, files: 449, bytes: 410 * MB, blocked: null },
  { known: "documents", name: "Documents", path: `${HOME}\\Documents`, files: 98_000, bytes: 14 * GB, blocked: null },
  { known: "downloads", name: "Downloads", path: `${HOME}\\Downloads`, files: 400, bytes: 22 * GB, blocked: null },
  { known: "pictures", name: "Pictures", path: `${HOME}\\Pictures`, files: 3_120, bytes: 6.3 * GB, blocked: null },
  // Still being measured.
  { known: "videos", name: "Videos", path: `${HOME}\\Videos`, files: null, bytes: null, blocked: null },
];
const SAMPLE_FILES = [
  "Taxes/2025/Receipts/Pharmacy.pdf",
  "Thesis/Chapter 3 - Methods.docx",
  "Recipes/Grandma's apple cake.txt",
  "Insurance/Home policy 2026.pdf",
];
/** What the folder picker returns, in turn: a fine folder, one holding the Drive folder, one inside it. */
const PICKS = ["D:\\Photo archive", HOME, `${HOME}\\Drive\\Projects`, "E:\\Music"];
let picks = 0;

interface MockBackup {
  info: BackupInfo;
  /** Ticks left while scanning. */
  scan: number;
  /** Ticks a full backup takes. */
  ticks: number;
  totalFiles: number;
  totalBytes: number;
  cursor: number;
  /** Switching between a full copy and on-demand: ticks left, and the size on this PC it ends at. */
  convert?: { ticks: number; from: number; to: number };
}

/** Free space the mock pretends this PC has: making Downloads a full copy again doesn't fit. */
const FREE_BYTES = 21 * GB;
const KEEP_DEFAULT = 5 * GB;

function backupInfo(id: string, name: string, path: string, known: KnownFolder | null, added: number): BackupInfo {
  return {
    id,
    name,
    path,
    known,
    enabled: true,
    state: "scanning",
    detail: "Looking through the folder…",
    excludes: [...DEFAULT_EXCLUDES],
    files: 0,
    bytes: 0,
    done_files: 0,
    done_bytes: 0,
    pending: 0,
    skipped_online_only: 0,
    added_ms: added,
    last_complete_ms: null,
    on_demand: false,
    on_demand_available: platform === "windows",
    local_bytes: 0,
    keep_bytes: KEEP_DEFAULT,
  };
}

function richBackups(): MockBackup[] {
  const k = (known: KnownFolder) => KNOWN.find((f) => f.known === known)!;
  const done = (info: BackupInfo, files: number, bytes: number) => ({
    ...info,
    detail: null,
    files,
    bytes,
    done_files: files,
    done_bytes: bytes,
  });
  const docs = k("documents");
  const desk = k("desktop");
  const pics = k("pictures");
  return [
    {
      info: {
        ...backupInfo("b1", docs.name, docs.path, "documents", now - 3_600_000),
        state: "backing_up",
        detail: null,
        files: 98_000,
        bytes: 14 * GB,
        done_files: 1_204,
        done_bytes: 900 * MB,
        pending: 98_000 - 1_204,
        skipped_online_only: 12,
      },
      scan: 0,
      ticks: 400,
      totalFiles: 98_000,
      totalBytes: 14 * GB,
      cursor: 0,
    },
    {
      info: {
        ...done(backupInfo("b2", desk.name, desk.path, "desktop", now - 2 * 86_400_000), 449, 410 * MB),
        state: "up_to_date",
        last_complete_ms: now - 5 * 60_000,
      },
      scan: 0,
      ticks: 20,
      totalFiles: 449,
      totalBytes: 410 * MB,
      cursor: 0,
    },
    {
      info: {
        ...done(backupInfo("b3", pics.name, pics.path, "pictures", now - 9 * 86_400_000), 3_120, 6.3 * GB),
        enabled: false,
        state: "off",
        last_complete_ms: now - 3 * 86_400_000,
      },
      scan: 0,
      ticks: 20,
      totalFiles: 3_120,
      totalBytes: 6.3 * GB,
      cursor: 0,
    },
    {
      info: {
        ...done(backupInfo("b4", "Old projects", "D:\\Old projects", null, now - 30 * 86_400_000), 12_400, 31 * GB),
        state: "problem",
        detail: "Folder not found. If it's on a removable drive, plug the drive back in.",
        last_complete_ms: now - 6 * 86_400_000,
      },
      scan: 0,
      ticks: 20,
      totalFiles: 12_400,
      totalBytes: 31 * GB,
      cursor: 0,
    },
  ];
}

/** Downloads saves space on this PC; Documents is a full copy. */
function onDemandBackups(): MockBackup[] {
  const k = (known: KnownFolder) => KNOWN.find((f) => f.known === known)!;
  const downloads = k("downloads");
  const docs = k("documents");
  const full = (id: string, f: FolderSuggestion, files: number, size: number, added: number): BackupInfo => ({
    ...backupInfo(id, f.name, f.path, f.known, added),
    state: "up_to_date",
    detail: null,
    files,
    bytes: size,
    done_files: files,
    done_bytes: size,
    local_bytes: size,
    last_complete_ms: now - 4 * 60_000,
  });
  return [
    {
      info: { ...full("b1", downloads, 2_310, 95.6 * GB, now - 6 * 86_400_000), on_demand: platform === "windows", local_bytes: 4.8 * GB },
      scan: 0,
      ticks: 20,
      totalFiles: 2_310,
      totalBytes: 95.6 * GB,
      cursor: 0,
    },
    {
      info: full("b2", docs, 98_000, 14 * GB, now - 9 * 86_400_000),
      scan: 0,
      ticks: 20,
      totalFiles: 98_000,
      totalBytes: 14 * GB,
      cursor: 0,
    },
  ];
}

const backupState = {
  folders: scenario === "backups" ? richBackups() : scenario === "on-demand" ? onDemandBackups() : ([] as MockBackup[]),
  // Outside the backup scenarios the popover stays as it was; Settings still lists the suggestions.
  dismissed: scenario !== "backup-suggest",
  ever: scenario === "backups" || scenario === "on-demand",
  next: 10,
};

function overview(): BackupOverview {
  const folders = backupState.folders.map((b) => {
    const info = { ...b.info, excludes: [...b.info.excludes] };
    // A full copy is all on this PC.
    if (!info.on_demand && info.state !== "converting") info.local_bytes = info.bytes;
    if (state.paused && (info.state === "scanning" || info.state === "backing_up")) {
      info.state = "waiting";
      info.detail = "Syncing is paused. The backup continues when you resume.";
    }
    return info;
  });
  const suggestions = KNOWN.filter((k) => !backupState.folders.some((b) => b.info.known === k.known))
    .map((k) => ({ ...k }))
    .sort((a, b) => (b.bytes ?? -1) - (a.bytes ?? -1));
  return {
    folders,
    suggestions,
    suggestions_dismissed: backupState.dismissed,
    cloud_folder: backupState.ever ? `Backups/${state.settings.device_name}` : null,
    default_excludes: [...DEFAULT_EXCLUDES],
  };
}

function emitBackups() {
  emit("drive://backups", overview());
}

function tickBackups() {
  if (!state.setUp || state.paused) return;
  let changed = false;
  for (const b of backupState.folders) {
    const i = b.info;
    if (i.state === "scanning") {
      changed = true;
      b.scan--;
      if (b.scan <= 0) {
        Object.assign(i, { state: "backing_up", detail: null, files: b.totalFiles, bytes: b.totalBytes });
      } else {
        const share = (8 - Math.min(b.scan, 7)) / 8;
        i.files = Math.round(b.totalFiles * share);
        i.bytes = Math.round(b.totalBytes * share);
        i.detail = `Looking through ${i.files.toLocaleString("en")} files…`;
      }
      i.pending = i.files - Math.min(i.done_files, i.files);
    } else if (i.state === "converting" && b.convert) {
      changed = true;
      const c = b.convert;
      c.ticks--;
      i.local_bytes = Math.round(c.to + ((c.from - c.to) * Math.max(0, c.ticks)) / 7);
      if (!i.on_demand) i.detail = `Downloading ${bytes(i.bytes - i.local_bytes)} back to this PC…`;
      if (c.ticks <= 0) {
        b.convert = undefined;
        i.local_bytes = c.to;
        i.detail = null;
        i.state = !i.enabled ? "off" : i.done_files === i.files ? "up_to_date" : "backing_up";
      }
    } else if (i.state === "backing_up") {
      changed = true;
      i.done_files = Math.min(i.files, i.done_files + Math.max(1, Math.round(b.totalFiles / b.ticks)));
      i.done_bytes = i.done_files === i.files ? i.bytes : Math.round((i.bytes / Math.max(1, i.files)) * i.done_files);
      i.pending = i.files - i.done_files;
      if (i.done_files === i.files) {
        i.state = "up_to_date";
        i.last_complete_ms = Date.now();
      }
      if (b.cursor++ % 3 === 0 || i.state === "up_to_date") {
        const a: Activity = {
          id: ++state.tick + 20_000,
          at_ms: Date.now(),
          kind: "uploaded",
          path: SAMPLE_FILES[Math.floor(b.cursor / 3) % SAMPLE_FILES.length],
          backup: i.id,
          bytes: 220_000,
          detail: null,
        };
        state.activity.unshift(a);
        emit("drive://activity", a);
      }
    }
  }
  if (changed) {
    emitBackups();
    emit("drive://status", status());
  }
}

if (scenario !== "idle") setInterval(tickBackups, 700);

/** The backup that's uploading, as the service shows it in the transfer list. */
function backupItems(): TransferItem[] {
  if (state.paused) return [];
  return backupState.folders
    .filter((b) => b.info.state === "backing_up")
    .map((b, n) => {
      const total = 1.4 * MB;
      const done = ((b.cursor % 3) + 1) * (total / 3);
      return {
        id: 900 + n,
        path: `${b.info.name} › ${SAMPLE_FILES[Math.floor(b.cursor / 3) % SAMPLE_FILES.length]}`,
        direction: "upload",
        state: "active",
        reason: null,
        bytes_done: Math.round(done),
        bytes_total: Math.round(total),
        rate_bps: Math.round(1.2 * MB),
        eta_secs: 1,
        retry_at_ms: null,
        resumable: false,
        mode: "auto",
      } satisfies TransferItem;
    });
}

function addBackup(raw: string): BackupInfo | Promise<never> {
  const path = raw.trim().replace(/[\\/]+$/, "");
  const inside = (p: string, root: string) => p.toLowerCase() === root.toLowerCase() || p.toLowerCase().startsWith(root.toLowerCase() + "\\");
  const drive = state.settings.folder;
  if (!path) return fail("bad_request", "Choose a folder to back up.");
  if (inside(path, drive)) return fail("bad_request", "That folder is inside your Drive folder, so it's already in the cloud.");
  if (inside(drive, path))
    return fail("bad_request", "That folder contains your Drive folder. Choose a folder inside it instead, like Documents.");
  for (const b of backupState.folders) {
    if (b.info.path.toLowerCase() === path.toLowerCase()) return fail("bad_request", "That folder is already backed up.");
    if (inside(path, b.info.path)) return fail("bad_request", `That folder is inside ${b.info.name}, which is already backed up.`);
    if (inside(b.info.path, path))
      return fail("bad_request", `That folder contains ${b.info.name}, which is already backed up. Stop backing up ${b.info.name} first.`);
  }
  const known = KNOWN.find((k) => k.path.toLowerCase() === path.toLowerCase()) ?? null;
  const base = path.split(/[\\/]/).pop() || path;
  let name = base;
  for (let n = 2; backupState.folders.some((b) => b.info.name === name); n++) name = `${base} (${n})`;
  const info = backupInfo(`b${++backupState.next}`, name, path, known?.known ?? null, Date.now());
  backupState.folders.push({
    info,
    scan: 7,
    ticks: 22,
    totalFiles: known?.files ?? 4_210,
    totalBytes: known?.bytes ?? 2.6 * GB,
    cursor: 0,
  });
  backupState.ever = true;
  emitBackups();
  return { ...info };
}

function findBackup(id: unknown): MockBackup | undefined {
  return backupState.folders.find((b) => b.info.id === id);
}

// ----- Updates

const MOCK_RELEASES: Release[] = [
  {
    version: "0.1.2",
    date: new Date(now - 2 * 86_400_000).toISOString(),
    url: "https://github.com/samcannas/drive/releases/tag/v0.1.2",
    notes: [
      "Update Drive from the status window or Settings",
      "Choose whether updates download automatically",
      "Faster retries for uploads that had to wait",
    ],
  },
  {
    version: "0.1.1",
    date: new Date(now - 5 * 86_400_000).toISOString(),
    url: "https://github.com/samcannas/drive/releases/tag/v0.1.1",
    notes: ["Drag the status window anywhere, and close it with the X button", "Choose what closing the window does"],
  },
];
const latest = MOCK_RELEASES[0];
const offered = { version: latest.version, date: latest.date, notes: latest.notes };

let update: UpdateState = (() => {
  switch (scenario) {
    case "update":
      return { state: "available", ...offered };
    case "update-ready":
      return { state: "ready", ...offered };
    case "update-error":
      return {
        state: "error",
        message: "The download stopped. Check your internet connection and try again.",
        version: latest.version,
      };
    case "update-off":
      return { state: "disabled", reason: "Drive was installed by your package manager, which keeps it up to date." };
    default:
      return { state: "up_to_date", checked_ms: now - 3 * 60_000 };
  }
})();
const offersUpdate = scenario.startsWith("update") && scenario !== "update-off";

function setUpdate(u: UpdateState) {
  update = u;
  emit("drive://update", u);
}

async function downloadUpdate() {
  const total = 18 * MB;
  for (let i = 0; i <= 30; i++) {
    setUpdate({ state: "downloading", version: latest.version, done_bytes: Math.round((total * i) / 30), total_bytes: total });
    await sleep(100);
  }
  setUpdate({ state: "ready", ...offered });
}

// ----- Transfers

type Hold = "none" | "locked" | "offline";
interface MockTransfer {
  id: number;
  path: string;
  direction: TransferDirection;
  state: TransferState;
  mode: TransferMode;
  hold: Hold;
  done: number;
  total: number;
  speed: number;
  resumable: boolean;
  countdown: number;
  retryAt: number | null;
}

function upload(id: number, path: string, total: number, done: number, s: TransferState, extra: Partial<MockTransfer> = {}): MockTransfer {
  return {
    id,
    path,
    direction: "upload",
    state: s,
    mode: "auto",
    hold: "none",
    done,
    total,
    speed: 2.1 * MB,
    resumable: total > 8 * MB,
    countdown: 4,
    retryAt: null,
    ...extra,
  };
}

function richTransfers(): MockTransfer[] {
  const night: MockTransfer[] = [
    upload(210, "Videos/Drone/Coast flight.mov", 3.2 * GB, 0, "scheduled"),
    upload(211, "Videos/Drone/Harbor at dusk.mov", 2.4 * GB, 600 * MB, "scheduled"),
  ];
  const paused = upload(209, "Backups/Laptop image 2026-11.vhdx", 48 * GB, 12.4 * GB, "paused", { mode: "paused" });
  if (scenario === "transfers-night") return night;
  if (scenario === "transfers-paused")
    return [paused, upload(212, "Videos/2026/Lisbon trip, all clips.mp4", 6.8 * GB, 2.1 * GB, "paused", { mode: "paused" })];
  return [
    upload(201, "Videos/2026/Birthday party at the lake house (full length, 4K).mp4", 1640 * MB, 45 * MB, "active"),
    upload(202, "Photos/2026/Lisbon/IMG_4822.HEIC", 3.2 * MB, 0, "active", { speed: 0.9 * MB }),
    upload(203, "Projects/drive/Design review.pdf", 84 * MB, 30 * MB, "active", { direction: "download", speed: 6 * MB }),
    upload(204, "Documents/Taxes/2025 return.pdf", 640 * 1024, 640 * 1024, "verifying", { countdown: 6 }),
    upload(205, "Documents/Scans/Passport and ID card.pdf", 2 * MB, 0, "preparing", { countdown: 8 }),
    upload(206, "Music/Field recordings/Rain on tin roof.flac", 38 * MB, 0, "queued"),
    upload(207, "Documents/Budget 2026.xlsx", 84 * 1024, 0, "waiting", { hold: "locked" }),
    upload(208, "Projects/Garden/Site plan.dwg", 12 * MB, 5 * MB, "waiting", { hold: "offline", retryAt: now + 25_000 }),
    paused,
    ...night,
  ];
}

const transfers: MockTransfer[] | null = scenario.startsWith("transfers") ? richTransfers() : null;
const moving = (s: TransferState) => s === "preparing" || s === "active" || s === "verifying";
const rank = (s: TransferState) => (moving(s) ? 0 : s === "queued" || s === "waiting" ? 1 : 2);

function nextWindow(hhmm: string): number {
  const [h, m] = hhmm.split(":").map(Number);
  const d = new Date();
  d.setHours(h, m, 0, 0);
  if (d.getTime() <= Date.now()) d.setDate(d.getDate() + 1);
  return d.getTime();
}

function waitsForNight(t: MockTransfer): boolean {
  if (t.direction !== "upload") return false;
  if (t.mode === "tonight") return true;
  return t.mode === "auto" && state.settings.night_uploads && t.total >= state.settings.night_min_bytes;
}

function tickTransfers() {
  if (!transfers) return;
  const t0 = Date.now();
  const finished: MockTransfer[] = [];
  for (const t of transfers) {
    if (t.state === "preparing" || t.state === "verifying") {
      if (--t.countdown <= 0) {
        if (t.state === "verifying") finished.push(t);
        else t.state = "queued";
      }
    } else if (t.state === "active") {
      t.done = Math.min(t.total, t.done + t.speed * 0.7 * (0.8 + Math.random() * 0.4));
      if (t.done >= t.total) {
        if (t.direction === "upload") {
          t.state = "verifying";
          t.countdown = 3;
        } else finished.push(t);
      }
    } else if (t.state === "waiting" && t.hold === "offline" && (t.retryAt ?? 0) <= t0) {
      t.retryAt = t0 + 30_000;
    }
  }
  for (const t of finished) {
    transfers.splice(transfers.indexOf(t), 1);
    const a: Activity = {
      id: ++state.tick + 10_000,
      at_ms: t0,
      kind: t.direction === "upload" ? "uploaded" : "downloaded",
      path: t.path,
      bytes: t.total,
      detail: null,
    };
    state.activity.unshift(a);
    emit("drive://activity", a);
    // The same file comes back as a new change, so the demo never runs dry.
    transfers.push({ ...t, id: t.id + 1000, done: 0, state: "preparing", countdown: 4 });
  }
  for (const [dir, slots] of [["upload", 2], ["download", 1]] as const) {
    let busy = transfers.filter((t) => t.direction === dir && moving(t.state)).length;
    for (const t of transfers) {
      if (busy >= slots) break;
      if (t.direction === dir && t.state === "queued") {
        t.state = "active";
        busy++;
      }
    }
  }
}

function itemOf(t: MockTransfer, t0: number): TransferItem {
  const rate = t.state === "active" ? Math.round(t.speed * (0.9 + Math.random() * 0.2)) : 0;
  let reason: string | null = null;
  if (t.state === "waiting" && t.hold === "locked") reason = "Open in another app — Drive will upload it once it's closed";
  if (t.state === "waiting" && t.hold === "offline")
    reason = `Offline — retrying in ${Math.max(1, Math.round(((t.retryAt ?? t0) - t0) / 1000))} s`;
  if (t.state === "scheduled") reason = `Waiting for tonight (${state.settings.night_start})`;
  return {
    id: t.id,
    path: t.path,
    direction: t.direction,
    state: t.state,
    reason,
    bytes_done: Math.round(t.done),
    bytes_total: Math.round(t.total),
    rate_bps: rate,
    eta_secs: rate > 0 ? Math.round((t.total - t.done) / rate) : null,
    retry_at_ms: t.retryAt,
    resumable: t.resumable,
    mode: t.mode,
  };
}

/** Without the long list: the status bar's three files, as rows. */
function batchItems(): TransferItem[] {
  const s = status();
  if (s.state !== "syncing") return [];
  const share = [0.62, 0.3, 0.08];
  const files = ["Photos/2026/Lisbon/IMG_4822.HEIC", "Documents/Meeting notes (final).docx", "Projects/Garden/Plan.pdf"];
  return files.map((path, i) => {
    const total = s.transfer_bytes_total * share[i];
    const done = s.transfer_bytes_done * share[i];
    const rate = total / 10;
    return {
      id: 100 + i,
      path,
      direction: i === 2 ? "download" : "upload",
      state: "active",
      reason: null,
      bytes_done: Math.round(done),
      bytes_total: Math.round(total),
      rate_bps: Math.round(rate),
      eta_secs: Math.round((total - done) / rate),
      retry_at_ms: null,
      resumable: false,
      mode: "auto",
    };
  });
}

function transferList(): TransferList {
  const t0 = Date.now();
  const items = [...(transfers ? transfers.map((t) => itemOf(t, t0)) : batchItems()), ...backupItems()].sort(
    (a, b) => rank(a.state) - rank(b.state),
  );
  const ups = items.filter((t) => t.direction === "upload");
  const upRate = ups.reduce((n, t) => n + t.rate_bps, 0);
  const remaining = ups.filter((t) => rank(t.state) < 2).reduce((n, t) => n + t.bytes_total - t.bytes_done, 0);
  const held = state.settings.night_uploads || ups.some((t) => t.mode === "tonight");
  return {
    items,
    total_count: items.length,
    upload_rate_bps: upRate,
    download_rate_bps: items.filter((t) => t.direction === "download").reduce((n, t) => n + t.rate_bps, 0),
    upload_remaining_bytes: remaining,
    upload_eta_secs: upRate > 0 ? Math.round(remaining / upRate) : null,
    night_window_opens_ms: held ? nextWindow(state.settings.night_start) : null,
    night_window_open: false,
    // The transfers scenarios show uploads slowed to keep the connection responsive.
    auto_upload_limit_bps: state.settings.adaptive_upload && upRate > 0 ? 1_450_000 : null,
    auto_upload_checking: false,
  };
}

function reschedule() {
  for (const t of transfers ?? []) {
    if (t.mode !== "auto") continue;
    const hold = waitsForNight(t);
    if (hold && (t.state === "queued" || t.state === "active")) t.state = "scheduled";
    else if (!hold && t.state === "scheduled") t.state = "queued";
  }
}

function status(): Status {
  if (!state.setUp) {
    return { ...empty(), state: "not_set_up", headline: "Not set up" };
  }
  const phase = state.tick % 32;
  const syncing = !state.paused && phase >= 1 && phase < 15;
  const total = 52 * MB;
  const base: Status = {
    ...empty(),
    state: "up_to_date",
    headline: "Up to date",
    account: state.account,
    folder: state.settings.folder,
    issues: state.issues.length,
    cache_bytes: 6 * GB + 300 * MB,
    cache_limit_bytes: state.settings.cache_limit_bytes,
    pinned_bytes: 2 * GB + 150 * MB,
    cloud_bytes: 148 * GB + 700 * MB,
    cloud_files: 23_418,
    last_sync_ms: Date.now() - 40_000,
    pending_delete_confirmation: state.pendingDelete,
  };
  if (scenario === "offline") return { ...base, state: "offline", headline: "Offline – changes will sync when you're back online", pending: 3 };
  if (scenario === "signedout") return { ...base, state: "signed_out", headline: "Sign in to keep syncing" };
  if (scenario === "error") return { ...base, state: "error", headline: "Can't open your files – encryption password needed" };
  if (state.paused) return { ...base, state: "paused", headline: "Syncing paused" };
  if (transfers) {
    const live = transfers.filter((t) => moving(t.state));
    if (!live.length) return { ...base, pending: transfers.length };
    const up = live.filter((t) => t.direction === "upload").length;
    return {
      ...base,
      state: "syncing",
      headline: `Syncing ${live.length} ${live.length === 1 ? "file" : "files"}…`,
      uploading: up,
      downloading: live.length - up,
      pending: transfers.length,
      transfer_bytes_total: Math.round(live.reduce((n, t) => n + t.total, 0)),
      transfer_bytes_done: Math.round(live.reduce((n, t) => n + t.done, 0)),
    };
  }
  const backingUp = backupState.folders.filter((b) => b.info.state === "backing_up");
  if (!syncing && backingUp.length) {
    return {
      ...base,
      state: "syncing",
      headline: `Backing up ${backingUp[0].info.name}…`,
      uploading: backingUp.length,
      pending: backingUp.reduce((n, b) => n + b.info.pending, 0),
    };
  }
  if (syncing) {
    return {
      ...base,
      state: "syncing",
      headline: "Syncing 3 files…",
      uploading: 2,
      downloading: 1,
      pending: 3,
      transfer_bytes_total: total,
      transfer_bytes_done: Math.round((total * phase) / 15),
    };
  }
  return base;
}

function empty(): Status {
  return {
    state: "not_set_up",
    headline: "",
    account: null,
    folder: null,
    uploading: 0,
    downloading: 0,
    pending: 0,
    transfer_bytes_total: 0,
    transfer_bytes_done: 0,
    issues: 0,
    cache_bytes: 0,
    cache_limit_bytes: 0,
    pinned_bytes: 0,
    cloud_bytes: 0,
    cloud_files: 0,
    last_sync_ms: null,
    pending_delete_confirmation: null,
  };
}

if (scenario !== "idle") {
  setInterval(() => {
    if (!state.setUp || state.paused) return;
    if (transfers) {
      tickTransfers();
      emit("drive://status", status());
      emit("drive://transfers", transferList());
      return;
    }
    state.tick++;
    if (state.tick % 32 === 15) {
      const a: Activity = { id: state.tick, at_ms: Date.now(), kind: "uploaded", path: "Photos/2026/Lisbon/IMG_4822.HEIC", bytes: 3 * MB, detail: null };
      state.activity.unshift(a);
      emit("drive://activity", a);
    }
    if (state.tick % 32 <= 15) {
      emit("drive://status", status());
      emit("drive://transfers", transferList());
    }
  }, 700);
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const fail = (code: string, message: string) => Promise.reject({ code, message });

export async function invoke<T>(cmd: string, args: Record<string, unknown> = {}): Promise<T> {
  await sleep(cmd === "check_account" || cmd === "setup" ? 900 : 30);
  const r = await handle(cmd, args);
  return r as T;
}

async function handle(cmd: string, a: Record<string, unknown>): Promise<unknown> {
  switch (cmd) {
    case "app_info":
      return { version: "0.1.0", platform: "windows", repository: "https://github.com/samcannas/drive", connection: connection() };
    case "connection_state":
      return connection();
    case "get_status":
      return status();
    case "get_settings":
      return { ...state.settings };
    case "update_settings": {
      const p = a.patch as Record<string, unknown>;
      const hhmm = /^([01]\d|2[0-3]):[0-5]\d$/;
      const start = (p.night_start as string | undefined) ?? state.settings.night_start;
      const end = (p.night_end as string | undefined) ?? state.settings.night_end;
      if (!hhmm.test(start) || !hhmm.test(end)) return fail("bad_request", "Enter a time like 01:00.");
      if (start === end) return fail("bad_request", "Start and end can't be the same time. Change one of them.");
      for (const [k, v] of Object.entries(p)) {
        if (v === undefined || v === null) continue;
        if (k === "app_token") state.settings.app_token_custom = v !== "";
        else (state.settings as unknown as Record<string, unknown>)[k] = v;
      }
      reschedule();
      emit("drive://settings", { ...state.settings });
      if (transfers) emit("drive://transfers", transferList());
      if (p.auto_update === true && update.state === "available") void downloadUpdate();
      return { ...state.settings };
    }
    case "recent_activity":
      return state.activity.slice(0, a.limit as number);
    case "transfers":
      return transferList();
    case "set_transfer_mode": {
      const t = transfers?.find((x) => x.id === a.id);
      if (!t) return fail("not_found", "That transfer has already finished.");
      if (t.direction === "download") return fail("bad_request", "Downloads can't be paused or scheduled one at a time.");
      const mode = a.mode as TransferMode;
      t.mode = mode;
      t.hold = "none";
      t.retryAt = null;
      t.state = mode === "paused" ? "paused" : mode === "now" ? "active" : waitsForNight(t) ? "scheduled" : "queued";
      const list = transferList();
      emit("drive://transfers", list);
      emit("drive://status", status());
      return list;
    }
    case "list_issues":
      return state.issues;
    case "resolve_issue": {
      const issue = state.issues.find((i) => i.id === a.id);
      state.issues = state.issues.filter((i) => i.id !== a.id);
      if (issue?.kind === "mass_delete") state.pendingDelete = null;
      emit("drive://issues", state.issues.length);
      emit("drive://status", status());
      return null;
    }
    case "pause":
    case "resume":
      state.paused = cmd === "pause";
      emit("drive://status", status());
      emitBackups();
      return null;
    case "backups":
      return overview();
    case "add_backup":
      return addBackup(String(a.path ?? ""));
    case "pick_backup_folder":
      return PICKS[picks++ % PICKS.length];
    case "set_backup_enabled": {
      const b = findBackup(a.id);
      if (!b) return fail("not_found", "That folder isn't backed up anymore.");
      b.info.enabled = a.enabled as boolean;
      if (!b.info.enabled) Object.assign(b.info, { state: "off", detail: null });
      else if (b.info.state !== "problem") {
        // Looks through the folder again for what changed while it was off.
        Object.assign(b.info, { state: "scanning", detail: "Looking for changes…" });
        b.scan = 3;
      }
      emitBackups();
      emit("drive://status", status());
      return overview();
    }
    case "remove_backup": {
      const b = findBackup(a.id);
      if (!b) return fail("not_found", "That folder isn't backed up anymore.");
      backupState.folders.splice(backupState.folders.indexOf(b), 1);
      console.info(`[mock] stopped backing up ${b.info.name}`, a.deleteFromCloud ? "(cloud copy to trash)" : "(cloud copy kept)");
      emitBackups();
      emit("drive://status", status());
      return overview();
    }
    case "set_backup_excludes": {
      const b = findBackup(a.id);
      if (!b) return fail("not_found", "That folder isn't backed up anymore.");
      const list: string[] = [];
      for (const raw of a.excludes as string[]) {
        const e = raw.trim();
        if (!e || list.includes(e)) continue;
        if (/[\\/]/.test(e)) return fail("bad_request", `"${e}" has a slash. Enter just a name, like node_modules, or a pattern like *.tmp.`);
        list.push(e);
      }
      b.info.excludes = list;
      if (b.info.enabled && b.info.state !== "problem") {
        Object.assign(b.info, { state: "scanning", detail: "Looking through the folder…" });
        b.scan = 3;
      }
      emitBackups();
      return { ...b.info, excludes: [...list] };
    }
    case "set_backup_on_demand": {
      const b = findBackup(a.id);
      if (!b) return fail("not_found", "That folder isn't backed up anymore.");
      const i = b.info;
      if (!i.on_demand_available) return fail("bad_request", "Saving space is available on Windows only for now.");
      if (i.state === "converting") return fail("bad_request", `Drive is still switching ${i.name}. Try again when it's done.`);
      const keep = typeof a.keepBytes === "number" ? a.keepBytes : i.keep_bytes;
      const local = i.on_demand ? i.local_bytes : i.bytes;
      if (a.onDemand) {
        i.keep_bytes = keep;
        if (i.on_demand) {
          // A smaller limit frees space right away; a larger one fills up as files are opened.
          i.local_bytes = Math.min(i.local_bytes, keep);
        } else {
          i.on_demand = true;
          b.convert = { ticks: 7, from: local, to: Math.min(i.bytes, keep) };
          Object.assign(i, { state: "converting", detail: `Freeing up space in ${i.name}…` });
        }
      } else if (i.on_demand) {
        const missing = i.bytes - local;
        if (missing > FREE_BYTES)
          return fail(
            "bad_request",
            `Drive needs ${bytes(missing)} to download ${i.name} back, but this PC has only ${bytes(FREE_BYTES)} free. Free up space, then try again.`,
          );
        i.on_demand = false;
        b.convert = { ticks: 7, from: local, to: i.bytes };
        Object.assign(i, { state: "converting", detail: `Downloading ${bytes(missing)} back to this PC…` });
      }
      emitBackups();
      return { ...overview().folders.find((f) => f.id === i.id)! };
    }
    case "dismiss_backup_suggestions":
      backupState.dismissed = true;
      emitBackups();
      return overview();
    case "check_account": {
      const { login, password } = a as { login: string; password: string };
      if (password === "wrong") return fail("auth_failed", "That login or password isn't right.");
      const exists = !login.includes("new");
      return { login, vault_exists: exists, vault_has_files: exists };
    }
    case "setup": {
      const req = a.request as { login: string; encryption_password: string | null; folder: string | null };
      const exists = !req.login.includes("new");
      if (exists && req.encryption_password === "wrong")
        return fail("wrong_encryption_password", "That encryption password doesn't open this vault.");
      state.setUp = true;
      state.account = req.login;
      if (req.folder) state.settings.folder = req.folder;
      emit("drive://status", status());
      return {
        folder: state.settings.folder,
        generated_encryption_password: !exists && !req.encryption_password ? "k7pqm-x2vhd-9rwta-c4nfe-zb8sj-u3ygm" : null,
        created_vault: !exists,
      };
    }
    case "default_folder":
      return "C:\\Users\\alex\\Drive";
    case "pick_folder":
      return "D:\\Cloud\\Drive";
    case "save_recovery_kit":
      return "C:\\Users\\alex\\Documents\\Drive recovery kit.txt";
    case "sign_out":
      state.setUp = false;
      emit("drive://status", status());
      return null;
    case "update_state":
      return update;
    case "check_for_updates": {
      if (update.state === "disabled" || update.state === "downloading" || update.state === "installing") return update;
      setUpdate({ state: "checking" });
      await sleep(800);
      if (!offersUpdate) {
        setUpdate({ state: "up_to_date", checked_ms: Date.now() });
        return update;
      }
      setUpdate({ state: "available", ...offered });
      if (state.settings.auto_update) void downloadUpdate();
      return update;
    }
    case "install_update": {
      if (update.state !== "ready") await downloadUpdate();
      setUpdate({ state: "installing", version: latest.version });
      await sleep(1500);
      console.info("[mock] Drive would restart now");
      setUpdate({ state: "up_to_date", checked_ms: Date.now() });
      return null;
    }
    case "update_changelog":
      return offersUpdate ? MOCK_RELEASES : [];
    default:
      console.info(`[mock] ${cmd}`, a);
      return null;
  }
}

function connection() {
  return { state: scenario === "unavailable" ? "unavailable" : "connected", mock: true, service_version: "0.1.0-mock" };
}
