// Browser-only stand-in for the Tauri backend, used by `pnpm dev` outside Tauri.
// Mirrors app/src-tauri/src/mock.rs closely enough to develop and screenshot every view.
//
// ?scenario= onboarding | massdelete | offline | signedout | paused | error | unavailable | idle
//            update (an update is available, automatic updates off) | update-ready (downloaded)
//            update-error | update-off (this installation doesn't update itself)
//            transfers (a long transfer list: one item in every state, a paused upload, large
//            files held for tonight, a partly done one that resumes) | transfers-night (only
//            uploads held for tonight) | transfers-paused (only paused uploads)

import type {
  Activity,
  Issue,
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

const GB = 1024 ** 3;
const MB = 1024 ** 2;
const scenario = new URLSearchParams(location.search).get("scenario") ?? "";

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
  const items = (transfers ? transfers.map((t) => itemOf(t, t0)) : batchItems()).sort((a, b) => rank(a.state) - rank(b.state));
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
      return null;
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
