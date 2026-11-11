// The only bridge between the webview and the Tauri backend. In a plain browser (`pnpm dev`
// without Tauri) a mock answers instead, so views can be developed and screenshotted.

import type {
  AccountCheck,
  Activity,
  AppInfo,
  CallError,
  ConnectionInfo,
  DriveEnv,
  Issue,
  IssueAction,
  Material,
  Release,
  Settings,
  SettingsPatch,
  SetupRequest,
  SetupResult,
  Status,
  UpdateState,
  View,
} from "./types";

export const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export const EVENTS = {
  status: "drive://status",
  activity: "drive://activity",
  issues: "drive://issues",
  connection: "drive://connection",
  settings: "drive://settings",
  update: "drive://update",
  popoverShown: "drive://popover-shown",
} as const;

async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (inTauri) {
    const core = await import("@tauri-apps/api/core");
    return core.invoke<T>(cmd, args);
  }
  if (import.meta.env.DEV) {
    const mock = await import("./mock");
    return mock.invoke<T>(cmd, args);
  }
  throw { code: "unavailable", message: "Drive isn't available." } satisfies CallError;
}

/** Subscribes to a backend event; returns the unsubscribe function. */
export async function listen<T>(event: string, handler: (payload: T) => void): Promise<() => void> {
  if (inTauri) {
    const ev = await import("@tauri-apps/api/event");
    return ev.listen<T>(event, (e) => handler(e.payload));
  }
  if (import.meta.env.DEV) {
    const mock = await import("./mock");
    return mock.listen(event, handler as (p: unknown) => void);
  }
  return () => {};
}

export function errorOf(e: unknown): CallError {
  if (e && typeof e === "object" && "message" in e) {
    const o = e as { code?: unknown; message: unknown };
    return { code: typeof o.code === "string" ? o.code : "internal", message: String(o.message) };
  }
  return { code: "internal", message: String(e) };
}

/** Window environment injected by the backend; query parameters stand in for it in a browser. */
export function env(): DriveEnv {
  const injected = (window as unknown as { __DRIVE_ENV__?: DriveEnv }).__DRIVE_ENV__;
  if (injected) return injected;
  const q = new URLSearchParams(location.search);
  return {
    view: (q.get("view") as View) ?? "popover",
    material: (q.get("material") as Material) ?? "none",
    platform: q.get("platform") ?? "windows",
  };
}

export const api = {
  appInfo: () => invoke<AppInfo>("app_info"),
  connection: () => invoke<ConnectionInfo>("connection_state"),
  status: () => invoke<Status>("get_status"),
  settings: () => invoke<Settings>("get_settings"),
  updateSettings: (patch: SettingsPatch) => invoke<Settings>("update_settings", { patch }),
  recentActivity: (limit = 20) => invoke<Activity[]>("recent_activity", { limit }),
  issues: () => invoke<Issue[]>("list_issues"),
  resolveIssue: (id: number, action: IssueAction) => invoke<void>("resolve_issue", { id, action }),
  pause: () => invoke<void>("pause"),
  resume: () => invoke<void>("resume"),
  syncNow: () => invoke<void>("sync_now"),
  checkAccount: (login: string, password: string) => invoke<AccountCheck>("check_account", { login, password }),
  setup: (request: SetupRequest) => invoke<SetupResult>("setup", { request }),
  defaultFolder: () => invoke<string>("default_folder"),
  pickFolder: () => invoke<string | null>("pick_folder"),
  saveRecoveryKit: () => invoke<string | null>("save_recovery_kit"),
  signOut: () => invoke<void>("sign_out"),
  openFolder: () => invoke<void>("open_folder"),
  revealItem: (path: string) => invoke<void>("reveal_item", { path }),
  openUrl: (url: string) => invoke<void>("open_url", { url }),
  showWindow: (name: View) => invoke<void>("show_window", { name }),
  finishOnboarding: () => invoke<void>("finish_onboarding"),
  hidePopover: () => invoke<void>("hide_popover"),
  /** The X button: hides Drive to the tray or quits, depending on Settings. */
  closeMain: () => invoke<void>("close_main"),
  reconnect: () => invoke<void>("reconnect"),
  updateState: () => invoke<UpdateState>("update_state"),
  /** Checks now; resolves to the state afterwards. Progress also arrives as `drive://update`. */
  checkForUpdates: () => invoke<UpdateState>("check_for_updates"),
  /** Downloads if needed, installs and restarts Drive. Only resolves in the mock or on failure. */
  installUpdate: () => invoke<void>("install_update"),
  /** Releases newer than this version, newest first. */
  updateChangelog: () => invoke<Release[]>("update_changelog"),
  quit: () => invoke<void>("quit"),
};

/** Starts moving the current window with the mouse (for frameless windows). */
export async function startDragging(): Promise<void> {
  if (!inTauri) return;
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  await getCurrentWindow().startDragging();
}

/** Closes the current window (settings, onboarding). */
export async function closeWindow(): Promise<void> {
  if (inTauri) {
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    await getCurrentWindow().close();
  } else {
    console.info("[mock] close window");
  }
}

/** Follows the window's theme, which on Linux can differ from what the webview reports. */
export async function followWindowTheme(apply: (theme: "light" | "dark" | null) => void): Promise<void> {
  if (!inTauri) {
    const t = new URLSearchParams(location.search).get("theme");
    apply(t === "light" || t === "dark" ? t : null);
    return;
  }
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  const w = getCurrentWindow();
  apply(await w.theme());
  await w.onThemeChanged(({ payload }) => apply(payload));
}
