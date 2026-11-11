// Shared reactive state for a window: the service status, connection, issues and activity,
// kept current from backend events.

import { api, EVENTS, listen } from "./api";
import type { Activity, ConnectionInfo, Issue, Settings, Status } from "./types";

export const drive = $state({
  status: null as Status | null,
  connection: { state: "connecting", mock: false, service_version: null } as ConnectionInfo,
  issues: [] as Issue[],
  activity: [] as Activity[],
  settings: null as Settings | null,
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

async function refreshAll(opts: { activity?: boolean; issues?: boolean; settings?: boolean }) {
  try {
    drive.status = await api.status();
  } catch {
    /* as above */
  }
  await Promise.all([
    opts.issues ? refreshIssues() : null,
    opts.activity ? refreshActivity() : null,
    opts.settings ? refreshSettings() : null,
  ]);
}

/** Loads the initial state and keeps it current. Call once per window. */
export async function connect(opts: { activity?: boolean; issues?: boolean; settings?: boolean }) {
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
  await listen(EVENTS.popoverShown, () => refreshAll(opts));

  try {
    drive.connection = await api.connection();
  } catch {
    /* stays "connecting" */
  }
  await refreshAll(opts);
}
