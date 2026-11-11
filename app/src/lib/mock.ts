// Browser-only stand-in for the Tauri backend, used by `pnpm dev` outside Tauri.
// Mirrors app/src-tauri/src/mock.rs closely enough to develop and screenshot every view.
//
// ?scenario= onboarding | massdelete | offline | signedout | paused | error | unavailable | idle

import type { Activity, Issue, Settings, Status } from "./types";

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
    max_parallel_transfers: 4,
    launch_at_login: true,
    notifications: true,
    device_name: "ALEX-DESKTOP",
    app_token_custom: false,
    close_action: "minimize",
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
    state.tick++;
    if (state.tick % 32 === 15) {
      const a: Activity = { id: state.tick, at_ms: Date.now(), kind: "uploaded", path: "Photos/2026/Lisbon/IMG_4822.HEIC", bytes: 3 * MB, detail: null };
      state.activity.unshift(a);
      emit("drive://activity", a);
    }
    if (state.tick % 32 <= 15) emit("drive://status", status());
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
      for (const [k, v] of Object.entries(p)) {
        if (v === undefined || v === null) continue;
        if (k === "app_token") state.settings.app_token_custom = v !== "";
        else (state.settings as unknown as Record<string, unknown>)[k] = v;
      }
      emit("drive://settings", { ...state.settings });
      return { ...state.settings };
    }
    case "recent_activity":
      return state.activity.slice(0, a.limit as number);
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
    default:
      console.info(`[mock] ${cmd}`, a);
      return null;
  }
}

function connection() {
  return { state: scenario === "unavailable" ? "unavailable" : "connected", mock: true, service_version: "0.1.0-mock" };
}
