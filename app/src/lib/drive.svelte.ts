// Shared reactive state for a window: the service status, connection, issues and activity,
// kept current from backend events.

import { api, EVENTS, listen } from "./api";
import type { Activity, ConnectionInfo, Issue, Release, Settings, Status, TransferList, UpdateState } from "./types";

export const drive = $state({
  status: null as Status | null,
  connection: { state: "connecting", mock: false, service_version: null } as ConnectionInfo,
  issues: [] as Issue[],
  activity: [] as Activity[],
  settings: null as Settings | null,
  /** Per-file transfers. Null until loaded, or when the service is too old to list them. */
  transfers: null as TransferList | null,
  update: null as UpdateState | null,
  /** Newer releases for the version on offer (see `updateVersion`). */
  changelog: [] as Release[],
  /** Ticks every 30 s so relative times stay fresh. */
  now: Date.now(),
});

export async function refreshIssues() {
  try {
    drive.issues = await api.issues();
  } catch {
    // Keep the last list; the connection notice explains why.
  }
}

export async function refreshActivity() {
  try {
    drive.activity = await api.recentActivity(20);
  } catch {
    /* as above */
  }
}

export async function refreshSettings() {
  try {
    drive.settings = await api.settings();
  } catch {
    /* as above */
  }
}

export async function refreshTransfers() {
  try {
    drive.transfers = await api.transfers();
  } catch (e) {
    // An older service doesn't know the call: the popover falls back to the totals in Status.
    if ((e as { code?: string })?.code === "bad_request") drive.transfers = null;
  }
}

/** The version an update state is about, if any. */
export function updateVersion(u: UpdateState | null): string | null {
  if (!u) return null;
  if ("version" in u) return u.version;
  return null;
}

let changelogFor: string | null = null;

function setUpdate(u: UpdateState) {
  drive.update = u;
  const v = updateVersion(u);
  if (v && v !== changelogFor) {
    changelogFor = v;
    api
      .updateChangelog()
      .then((r) => {
        if (changelogFor === v) drive.changelog = r;
      })
      .catch(() => {});
  } else if (!v && u.state !== "checking") {
    changelogFor = null;
    drive.changelog = [];
  }
}

export async function refreshUpdate() {
  try {
    setUpdate(await api.updateState());
  } catch {
    /* as above */
  }
}

type Opts = { activity?: boolean; issues?: boolean; settings?: boolean; transfers?: boolean };

async function refreshAll(opts: Opts) {
  try {
    drive.status = await api.status();
  } catch {
    /* as above */
  }
  await Promise.all([
    opts.issues ? refreshIssues() : null,
    opts.activity ? refreshActivity() : null,
    opts.settings ? refreshSettings() : null,
    opts.transfers ? refreshTransfers() : null,
  ]);
}

/** Loads the initial state and keeps it current. Call once per window. */
export async function connect(opts: Opts) {
  setInterval(() => (drive.now = Date.now()), 30_000);

  await listen<Status>(EVENTS.status, (s) => {
    const before = drive.status;
    drive.status = s;
    if (opts.issues && before?.issues !== s.issues) refreshIssues();
  });
  await listen<ConnectionInfo>(EVENTS.connection, (c) => {
    const was = drive.connection.state;
    drive.connection = c;
    if (c.state === "connected" && was !== "connected") refreshAll(opts);
  });
  if (opts.activity) {
    await listen<Activity>(EVENTS.activity, (a) => {
      drive.activity = [a, ...drive.activity.filter((x) => x.id !== a.id)].slice(0, 20);
    });
  }
  if (opts.issues) await listen<number>(EVENTS.issues, () => refreshIssues());
  if (opts.settings) await listen<Settings>(EVENTS.settings, (s) => (drive.settings = s));
  // The backend only sends these while the popover is visible; showing it fetches the list once.
  if (opts.transfers) await listen<TransferList>(EVENTS.transfers, (t) => (drive.transfers = t));
  await listen<UpdateState>(EVENTS.update, setUpdate);
  await listen(EVENTS.popoverShown, () => {
    refreshAll(opts);
    refreshUpdate();
  });
  refreshUpdate();

  try {
    drive.connection = await api.connection();
  } catch {
    /* stays "connecting" */
  }
  await refreshAll(opts);
}
