<script lang="ts">
  import { onMount } from "svelte";
  import { api, closeWindow, env, errorOf } from "../lib/api";
  import { connect, drive, updateVersion } from "../lib/drive.svelte";
  import { bytes, clockOf, GB, plural, relative } from "../lib/format";
  import Icon, { type IconName } from "../lib/Icon.svelte";
  import ReleaseNotes from "../lib/ReleaseNotes.svelte";
  import type { AppInfo, SettingsPatch } from "../lib/types";

  type Section = "general" | "storage" | "network" | "account" | "recovery" | "updates" | "about";
  const SECTIONS: { id: Section; label: string; icon: IconName }[] = [
    { id: "general", label: "General", icon: "gear" },
    { id: "storage", label: "Storage", icon: "storage" },
    { id: "network", label: "Network", icon: "network" },
    { id: "account", label: "Account", icon: "user" },
    { id: "recovery", label: "Recovery", icon: "key" },
    { id: "updates", label: "Updates", icon: "arrow-up-circle" },
    { id: "about", label: "About", icon: "info" },
  ];
  const windows = env().platform === "windows";

  let section = $state<Section>("general");
  let info = $state<AppInfo | null>(null);
  let saveError = $state<string | null>(null);
  let announce = $state("");

  const s = $derived(drive.settings);
  const status = $derived(drive.status);
  const connected = $derived(drive.connection.state === "connected");
  const setUp = $derived(connected && status !== null && status.state !== "not_set_up");

  onMount(() => {
    connect({ settings: true });
    api.appInfo().then((i) => (info = i));
  });

  async function save(patch: SettingsPatch, label = "Setting saved") {
    saveError = null;
    try {
      drive.settings = await api.updateSettings(patch);
      announce = "";
      queueMicrotask(() => (announce = label));
    } catch (e) {
      saveError = `Couldn't save the change. ${errorOf(e).message}`;
    }
  }

  // ----- General
  let deviceName = $state("");
  let deviceError = $state<string | null>(null);
  $effect(() => {
    if (s) deviceName = s.device_name;
  });
  function saveDeviceName() {
    const name = deviceName.trim();
    if (!s || name === s.device_name) return;
    if (!name) {
      deviceError = "Enter a name for this device.";
      return;
    }
    deviceError = null;
    save({ device_name: name }, "Device name saved");
  }

  // ----- Storage
  let cacheGb = $state(0);
  $effect(() => {
    if (s) cacheGb = Math.round(s.cache_limit_bytes / GB);
  });
  const cacheMax = $derived(Math.max(200, cacheGb * 2));
  function saveCache() {
    const gb = Math.round(Math.min(Math.max(cacheGb, 1), 4096));
    cacheGb = gb;
    if (s && gb * GB !== s.cache_limit_bytes) save({ cache_limit_bytes: gb * GB }, `Limit set to ${gb} GB`);
  }

  // ----- Network
  const POLL = [
    [30, "Every 30 seconds"],
    [60, "Every minute"],
    [300, "Every 5 minutes"],
    [900, "Every 15 minutes"],
    [1800, "Every 30 minutes"],
  ] as const;
  const LIMITS = [
    ["upload_limit_kbps", "Upload speed"],
    ["download_limit_kbps", "Download speed"],
  ] as const;
  let syncRequested = $state(false);

  // Large uploads overnight
  const MB = 1024 ** 2;
  const NIGHT_SIZES = [100 * MB, 200 * MB, 500 * MB, GB, 2 * GB];
  let nightError = $state<string | null>(null);
  let nightErrorField = $state<"night_start" | "night_end" | null>(null);

  async function saveNight(patch: SettingsPatch, label: string, field: "night_start" | "night_end" | null = null) {
    nightError = null;
    nightErrorField = null;
    if (field && !patch[field]) {
      nightError = `Enter ${field === "night_start" ? "a start" : "an end"} time, like 01:00.`;
      nightErrorField = field;
      return;
    }
    try {
      drive.settings = await api.updateSettings(patch);
      announce = "";
      queueMicrotask(() => (announce = label));
    } catch (e) {
      // The service checks the times; its message says what to change.
      nightError = errorOf(e).message;
      nightErrorField = field;
    }
  }

  function limitValue(kbps: number) {
    return kbps === 0 ? "" : String(kbps);
  }
  function saveLimit(key: "upload_limit_kbps" | "download_limit_kbps", raw: string) {
    const n = Math.max(0, Math.round(Number(raw) || 0));
    save({ [key]: n });
  }

  // ----- Account
  let token = $state("");
  let tokenError = $state<string | null>(null);
  let signOutDialog = $state<HTMLDialogElement>();
  let signingOut = $state(false);
  let signOutError = $state<string | null>(null);

  async function saveToken(e: SubmitEvent) {
    e.preventDefault();
    if (!token.trim()) {
      tokenError = "Paste the API key you got from uloz.to.";
      return;
    }
    tokenError = null;
    await save({ app_token: token.trim() }, "API key saved");
    if (!saveError) token = "";
  }

  async function signOut() {
    if (signingOut) return;
    signingOut = true;
    signOutError = null;
    try {
      await api.signOut();
      signOutDialog?.close();
      await api.showWindow("onboarding");
      await closeWindow();
    } catch (e) {
      signOutError = `Couldn't sign out. ${errorOf(e).message}`;
    } finally {
      signingOut = false;
    }
  }

  // ----- Updates
  const upd = $derived(drive.update);
  const offered = $derived(updateVersion(upd));
  let updating = $state(false);
  let updateError = $state<string | null>(null);
  const updateBusy = $derived(
    updating || upd?.state === "checking" || upd?.state === "downloading" || upd?.state === "installing",
  );

  async function checkUpdates() {
    if (updateBusy) return;
    updateError = null;
    try {
      await api.checkForUpdates();
    } catch (e) {
      // The state carries the message too; this covers a failed call.
      if (drive.update?.state !== "error") updateError = errorOf(e).message;
    }
  }

  async function installUpdate() {
    if (updateBusy) return;
    updating = true;
    updateError = null;
    try {
      // Drive restarts once the update is installed; this only returns in the mock or on failure.
      await api.installUpdate();
    } catch (e) {
      if (drive.update?.state !== "error") updateError = errorOf(e).message;
    } finally {
      updating = false;
    }
  }

  // ----- Recovery
  let kitSaving = $state(false);
  let kitResult = $state<{ ok: boolean; text: string } | null>(null);
  async function saveKit() {
    if (kitSaving) return;
    kitSaving = true;
    kitResult = null;
    try {
      const path = await api.saveRecoveryKit();
      if (path) kitResult = { ok: true, text: `Saved to ${path}` };
    } catch (e) {
      kitResult = { ok: false, text: `Couldn't save the recovery kit. ${errorOf(e).message}` };
    } finally {
      kitSaving = false;
    }
  }
</script>

<div class="settings">
  <nav aria-label="Settings">
    <ul role="list">
      {#each SECTIONS as item}
        <li>
          <button
            class="nav-item"
            aria-current={section === item.id ? "page" : undefined}
            onclick={() => (section = item.id)}
          >
            <Icon name={item.icon} size={16} />
            {item.label}
          </button>
        </li>
      {/each}
    </ul>
  </nav>

  <main>
    <h1 class="title">{SECTIONS.find((x) => x.id === section)?.label}</h1>
    <p class="visually-hidden" role="status">{announce}</p>
    {#if saveError}
      <p class="error-text" role="alert"><Icon name="alert" size={16} />{saveError}</p>
    {/if}

    {#if section === "updates"}
      <div class="group">
        <div class="card setting update-status">
          <span class="avatar" class:accent={offered !== null} aria-hidden="true"><Icon name="arrow-up-circle" /></span>
          <div class="setting-text">
            <p class="body-strong numeric">Drive {info?.version ?? "…"}</p>
            <!-- Only the phase is live; the byte count would be re-announced several times a second. -->
            <p class="caption secondary numeric">
              <span role="status">{#if !upd || upd.state === "idle"}
                Drive checks for updates every few hours.
              {:else if upd.state === "disabled"}
                Updates aren't available for this installation.
              {:else if upd.state === "checking"}
                Checking for updates…
              {:else if upd.state === "up_to_date"}
                Up to date · checked {relative(upd.checked_ms, drive.now)}
              {:else if upd.state === "available"}
                Version {upd.version} is available
              {:else if upd.state === "downloading"}
                Downloading version {upd.version}…
              {:else if upd.state === "ready"}
                Version {upd.version} is ready to install
              {:else if upd.state === "installing"}
                Installing version {upd.version}… Drive restarts in a moment.
              {:else if upd.state === "error"}
                {upd.version ? `Version ${upd.version} is available` : "Couldn't check for updates"}
              {/if}</span>{#if upd?.state === "downloading" && upd.total_bytes}
                {` ${bytes(upd.done_bytes)} of ${bytes(upd.total_bytes)}`}
              {/if}
            </p>
          </div>
          {#if upd && upd.state !== "disabled"}
            {#if offered}
              <button class="btn primary" aria-disabled={updateBusy} onclick={installUpdate}>
                {upd.state === "installing" || updating ? "Updating…" : "Update and restart"}
              </button>
            {:else}
              <button class="btn" aria-disabled={updateBusy} onclick={checkUpdates}>
                {upd.state === "checking" ? "Checking…" : "Check for updates"}
              </button>
            {/if}
          {/if}
          {#if upd?.state === "downloading" && upd.total_bytes}
            <div
              class="meter update-meter"
              role="progressbar"
              aria-label="Download progress"
              aria-valuemin="0"
              aria-valuemax="100"
              aria-valuenow={Math.round((Math.min(upd.done_bytes, upd.total_bytes) / upd.total_bytes) * 100)}
            >
              <span style:width="{(Math.min(upd.done_bytes, upd.total_bytes) / upd.total_bytes) * 100}%"></span>
            </div>
          {/if}
        </div>

        {#if upd?.state === "disabled"}
          <div class="callout info">
            <Icon name="info" size={18} />
            <p>{upd.reason}</p>
          </div>
        {/if}

        {#if (upd?.state === "error" && !updateBusy) || updateError}
          <div class="callout danger" role="alert">
            <Icon name="alert" size={18} />
            <p>{updateError ?? (upd?.state === "error" ? upd.message : "")}</p>
            <div class="actions">
              <button class="btn" onclick={offered ? installUpdate : checkUpdates}>Try again</button>
            </div>
          </div>
        {/if}

        {#if offered && drive.changelog.length}
          <section class="card changelog" aria-labelledby="changelog-title">
            <h2 id="changelog-title" class="body-strong">What's new</h2>
            <ReleaseNotes releases={drive.changelog} />
          </section>
        {/if}

        {#if upd?.state !== "disabled"}
          {#if s}
            <label class="card setting">
              <span class="setting-text">
                <span id="auto_update-label">Download updates automatically</span>
                <span class="caption secondary" id="auto_update-desc">
                  {s.auto_update
                    ? "New versions download in the background. Drive installs them when you choose to restart."
                    : "Drive tells you when a new version is out and downloads it only when you choose to update."}
                </span>
              </span>
              <input
                type="checkbox"
                role="switch"
                class="switch"
                aria-labelledby="auto_update-label"
                aria-describedby="auto_update-desc"
                checked={s.auto_update}
                onchange={(e) => save({ auto_update: e.currentTarget.checked })}
              />
            </label>
          {:else}
            <p class="caption secondary">The automatic download setting appears when Drive is running.</p>
          {/if}
        {/if}
      </div>
    {:else if section === "about"}
      <div class="group">
        <div class="card about">
          <img src="/app-icon.svg" alt="" width="56" height="56" />
          <div>
            <p class="subtitle">Drive</p>
            <p class="secondary numeric">Version {info?.version ?? "…"}</p>
            {#if info?.connection.service_version}
              <p class="caption secondary numeric">
                Sync service {info.connection.service_version}{info.connection.mock ? " (mock)" : ""}
              </p>
            {/if}
          </div>
        </div>
        <div class="card setting">
          <div class="setting-text">
            <p>Drive is free, open-source software under the MIT License.</p>
            <p class="caption secondary">Your files are encrypted in the same format as rclone crypt.</p>
          </div>
          <button class="btn" onclick={() => info && api.openUrl(info.repository)}>
            View source on GitHub
            <Icon name="external" size={16} />
          </button>
        </div>
      </div>
    {:else if !connected}
      <div class="callout info">
        <Icon name="info" size={18} />
        <div>
          <p class="body-strong">{drive.connection.state === "unavailable" ? "Drive isn't running" : "Connecting to Drive…"}</p>
          <p>Settings appear when Drive is running.</p>
        </div>
        {#if drive.connection.state === "unavailable"}
          <div class="actions"><button class="btn" onclick={() => api.reconnect()}>Try again</button></div>
        {/if}
      </div>
    {:else if !setUp}
      <div class="callout info">
        <Icon name="info" size={18} />
        <div>
          <p class="body-strong">Drive isn't set up yet</p>
          <p>Connect your uloz.to account first. Settings appear after setup.</p>
        </div>
        <div class="actions">
          <button class="btn primary" onclick={() => api.showWindow("onboarding")}>Set up Drive</button>
        </div>
      </div>
    {:else if !s}
      <p class="secondary" role="status">Loading settings…</p>
    {:else if section === "general"}
      <div class="group">
        <div class="card setting">
          <div class="setting-text">
            <p id="folder-label">Drive folder</p>
            <p class="caption secondary selectable path">{s.folder}</p>
            <p class="caption secondary">To use a different folder, sign out and set up Drive again.</p>
          </div>
          <button class="btn" onclick={() => api.openFolder()}>
            <Icon name="folder" size={16} />
            Open
          </button>
        </div>

        <label class="card setting">
          <span class="setting-text">
            <span id="launch_at_login-label">Start Drive when you sign in</span>
            <span class="caption secondary" id="launch_at_login-desc">Keeps your files in sync without opening Drive yourself.</span>
          </span>
          <input
            type="checkbox"
            role="switch"
            class="switch"
            aria-labelledby="launch_at_login-label"
            aria-describedby="launch_at_login-desc"
            checked={s.launch_at_login}
            onchange={(e) => save({ launch_at_login: e.currentTarget.checked })}
          />
        </label>

        <label class="card setting">
          <span class="setting-text">
            <span id="close-label">Quit Drive when you close its window</span>
            <span class="caption secondary" id="close-desc">
              {s.close_action === "quit"
                ? "The X button quits Drive and syncing stops until you open it again."
                : "The X button hides Drive in the tray and your files keep syncing."}
            </span>
          </span>
          <input
            type="checkbox"
            role="switch"
            class="switch"
            aria-labelledby="close-label"
            aria-describedby="close-desc"
            checked={s.close_action === "quit"}
            onchange={(e) => save({ close_action: e.currentTarget.checked ? "quit" : "minimize" })}
          />
        </label>

        <label class="card setting">
          <span class="setting-text">
            <span id="notifications-label">Show notifications</span>
            <span class="caption secondary" id="notifications-desc">
              Only when something needs you, like signing in again or confirming a large delete.
            </span>
          </span>
          <input
            type="checkbox"
            role="switch"
            class="switch"
            aria-labelledby="notifications-label"
            aria-describedby="notifications-desc"
            checked={s.notifications}
            onchange={(e) => save({ notifications: e.currentTarget.checked })}
          />
        </label>

        <div class="card setting">
          <div class="setting-text">
            <label for="device-name">Device name</label>
            <p class="caption secondary" id="device-hint">Shown in your uloz.to account so you can tell devices apart.</p>
            {#if deviceError}
              <p class="error-text" id="device-error"><Icon name="alert" size={14} />{deviceError}</p>
            {/if}
          </div>
          <input
            id="device-name"
            class="input narrow"
            bind:value={deviceName}
            maxlength="64"
            autocomplete="off"
            aria-invalid={deviceError ? "true" : undefined}
            aria-describedby={deviceError ? "device-hint device-error" : "device-hint"}
            onchange={saveDeviceName}
            onkeydown={(e) => e.key === "Enter" && saveDeviceName()}
          />
        </div>
      </div>
    {:else if section === "storage"}
      <div class="group">
        {#if status}
          <div class="card usage">
            <div class="usage-row">
              <span>Downloaded files</span>
              <span class="secondary numeric">{bytes(status.cache_bytes)} of {bytes(status.cache_limit_bytes)}</span>
            </div>
            <div class="meter" aria-hidden="true">
              <span style:width="{Math.min(100, (status.cache_bytes / Math.max(1, status.cache_limit_bytes)) * 100)}%"></span>
            </div>
            <div class="usage-row">
              <span>Always kept on this device</span>
              <span class="secondary numeric">{bytes(status.pinned_bytes)}</span>
            </div>
            <div class="usage-row">
              <span>Everything in the cloud</span>
              <span class="secondary numeric">{bytes(status.cloud_bytes)} · {plural(status.cloud_files, "file", "files")}</span>
            </div>
          </div>
        {/if}

        <div class="card setting stacked">
          <div class="setting-text">
            <label for="cache-gb">Space for downloaded files</label>
            <p class="caption secondary" id="cache-hint">
              When downloaded files take more space than this, Drive removes the copies you opened least recently.
              They stay in the cloud and download again when you open them. Files you keep on this device don't count.
            </p>
          </div>
          <div class="cache-control">
            <input
              type="range"
              min="1"
              max={cacheMax}
              step="1"
              bind:value={cacheGb}
              onchange={saveCache}
              aria-label="Space for downloaded files, in gigabytes"
              aria-valuetext="{cacheGb} GB"
            />
            <span class="gb-input">
              <input
                id="cache-gb"
                class="input numeric"
                type="number"
                min="1"
                max="4096"
                inputmode="numeric"
                bind:value={cacheGb}
                onchange={saveCache}
                aria-describedby="cache-hint"
              />
              <span aria-hidden="true">GB</span>
            </span>
          </div>
        </div>

        <div class="callout info">
          <Icon name="info" size={18} />
          <div>
            <p class="body-strong">Free up space now</p>
            {#if windows}
              <p>
                In File Explorer, right-click a file or folder in your Drive folder and choose <b>Free up space</b>. To
                keep something available offline, choose <b>Always keep on this device</b>.
              </p>
            {:else}
              <p>
                Drive frees up space on its own when downloaded files go over the limit above. To free space now, run
                <code class="mono selectable">drive free &lt;path&gt;</code> in a terminal.
              </p>
            {/if}
          </div>
        </div>
      </div>
    {:else if section === "network"}
      <div class="group">
        {#each LIMITS as [key, label]}
          {@const value = s[key]}
          <div class="card setting">
            <div class="setting-text">
              <label for="{key}-mode">{label}</label>
              {#if value > 0}
                <p class="caption secondary">Limited to {value} KB/s</p>
              {/if}
            </div>
            <div class="limit">
              <select
                id="{key}-mode"
                class="input"
                value={value === 0 ? "off" : "on"}
                onchange={(e) => save({ [key]: e.currentTarget.value === "off" ? 0 : 1024 })}
              >
                <option value="off">Don't limit</option>
                <option value="on">Limit to</option>
              </select>
              {#if value > 0}
                <span class="gb-input">
                  <input
                    class="input numeric"
                    type="number"
                    min="1"
                    inputmode="numeric"
                    aria-label="{label} limit in kilobytes per second"
                    value={limitValue(value)}
                    onchange={(e) => saveLimit(key, e.currentTarget.value)}
                  />
                  <span aria-hidden="true">KB/s</span>
                </span>
              {/if}
            </div>
          </div>
        {/each}

        <div class="card setting">
          <div class="setting-text">
            <label for="parallel">Files to transfer at once</label>
            <p class="caption secondary">More is faster on a good connection. Fewer leaves room for other apps.</p>
          </div>
          <select
            id="parallel"
            class="input narrow"
            value={String(s.max_parallel_transfers)}
            onchange={(e) => save({ max_parallel_transfers: Number(e.currentTarget.value) })}
          >
            {#each [1, 2, 3, 4, 6, 8, 12, 16] as n}
              <option value={String(n)}>{n}</option>
            {/each}
          </select>
        </div>

        <div class="card setting">
          <div class="setting-text">
            <label for="poll">Check uloz.to for changes</label>
            <p class="caption secondary">Changes you make on this {windows ? "PC" : "computer"} upload right away.</p>
          </div>
          <div class="limit">
            <select
              id="poll"
              class="input"
              value={String(s.poll_interval_secs)}
              onchange={(e) => save({ poll_interval_secs: Number(e.currentTarget.value) })}
            >
              {#if !POLL.some(([v]) => v === s.poll_interval_secs)}
                <option value={String(s.poll_interval_secs)}>Every {s.poll_interval_secs} seconds</option>
              {/if}
              {#each POLL as [v, text]}
                <option value={String(v)}>{text}</option>
              {/each}
            </select>
            <button
              class="btn"
              onclick={async () => {
                await api.syncNow();
                syncRequested = true;
              }}
            >
              Check now
            </button>
          </div>
        </div>
        <p class="caption secondary" role="status">{syncRequested ? "Checking uloz.to for changes…" : ""}</p>
      </div>

      <section class="group" aria-labelledby="night-title">
        <h2 id="night-title" class="body-strong group-title">Large uploads overnight</h2>
        <label class="card setting">
          <span class="setting-text">
            <span id="night-label">Upload large files overnight</span>
            <span class="caption secondary" id="night-desc">
              Files of {bytes(s.night_min_bytes)} or more upload between {clockOf(s.night_start)} and {clockOf(s.night_end)}, so
              they don't slow your connection during the day. You can still upload one right away from the tray.
            </span>
          </span>
          <input
            type="checkbox"
            role="switch"
            class="switch"
            aria-labelledby="night-label"
            aria-describedby="night-desc"
            checked={s.night_uploads}
            onchange={(e) =>
              saveNight(
                { night_uploads: e.currentTarget.checked },
                e.currentTarget.checked ? "Large files upload overnight" : "Large files upload right away",
              )}
          />
        </label>

        {#if s.night_uploads}
          <div class="card setting night-window">
            <div class="setting-text">
              <p id="night-window-label">Upload window</p>
              {#if s.night_end < s.night_start}
                <p class="caption secondary" id="night-window-hint">Ends the next morning.</p>
              {/if}
            </div>
            <div class="limit" role="group" aria-labelledby="night-window-label">
              <label class="time-label" for="night-start">From</label>
              <input
                id="night-start"
                class="input time"
                type="time"
                required
                value={s.night_start}
                aria-invalid={nightErrorField === "night_start" ? "true" : undefined}
                aria-describedby={nightErrorField === "night_start" ? "night-error" : undefined}
                onchange={(e) => saveNight({ night_start: e.currentTarget.value }, "Start time saved", "night_start")}
              />
              <label class="time-label" for="night-end">to</label>
              <input
                id="night-end"
                class="input time"
                type="time"
                required
                value={s.night_end}
                aria-invalid={nightErrorField === "night_end" ? "true" : undefined}
                aria-describedby={nightErrorField === "night_end" ? "night-error" : s.night_end < s.night_start ? "night-window-hint" : undefined}
                onchange={(e) => saveNight({ night_end: e.currentTarget.value }, "End time saved", "night_end")}
              />
            </div>
            {#if nightError}
              <p class="error-text night-error" id="night-error" role="alert"><Icon name="alert" size={14} />{nightError}</p>
            {/if}
          </div>

          <div class="card setting">
            <div class="setting-text">
              <label for="night-min">File size</label>
              <p class="caption secondary" id="night-min-hint">Files this size or bigger wait for the upload window.</p>
            </div>
            <select
              id="night-min"
              class="input narrow"
              value={String(s.night_min_bytes)}
              aria-describedby="night-min-hint"
              onchange={(e) =>
                saveNight({ night_min_bytes: Number(e.currentTarget.value) }, `Files of ${bytes(Number(e.currentTarget.value))} or more wait for the window`)}
            >
              {#if !NIGHT_SIZES.includes(s.night_min_bytes)}
                <option value={String(s.night_min_bytes)}>{bytes(s.night_min_bytes)}</option>
              {/if}
              {#each NIGHT_SIZES as n}
                <option value={String(n)}>{bytes(n)}</option>
              {/each}
            </select>
          </div>
        {:else if nightError}
          <p class="error-text" role="alert"><Icon name="alert" size={14} />{nightError}</p>
        {/if}
      </section>
    {:else if section === "account"}
      <div class="group">
        <div class="card setting">
          <span class="avatar" aria-hidden="true"><Icon name="user" /></span>
          <div class="setting-text">
            <p class="body-strong selectable">{status?.account ?? "—"}</p>
            <p class="caption secondary">uloz.to account</p>
          </div>
          <button class="btn" onclick={() => signOutDialog?.showModal()}>Sign out…</button>
        </div>

        <form class="card setting stacked" onsubmit={saveToken} novalidate>
          <div class="setting-text">
            <label for="token">uloz.to API key</label>
            <p class="caption secondary" id="token-hint">
              uloz.to asks apps to use their own API key. Drive uses a shared key unless you add one you requested from
              uloz.to. A key of your own isn't affected when other people use Drive heavily.
            </p>
            <p class="caption">
              {#if s.app_token_custom}
                <Icon name="check" size={14} /> Using your own key.
              {:else}
                Using the shared key.
              {/if}
            </p>
          </div>
          <div class="token-row">
            <input
              id="token"
              class="input"
              type="password"
              autocomplete="off"
              spellcheck="false"
              placeholder={s.app_token_custom ? "Paste a new key to replace yours" : "Paste your key"}
              bind:value={token}
              aria-describedby={tokenError ? "token-hint token-error" : "token-hint"}
              aria-invalid={tokenError ? "true" : undefined}
            />
            <button class="btn" type="submit">Save key</button>
            {#if s.app_token_custom}
              <button class="btn subtle" type="button" onclick={() => save({ app_token: "" }, "Using the shared key")}>
                Use shared key
              </button>
            {/if}
          </div>
          {#if tokenError}
            <p class="error-text" id="token-error"><Icon name="alert" size={14} />{tokenError}</p>
          {/if}
        </form>
      </div>

      <dialog bind:this={signOutDialog} aria-labelledby="signout-title" aria-describedby="signout-body">
        <div class="dialog-body">
          <h2 id="signout-title" class="subtitle">Sign out of Drive?</h2>
          <div id="signout-body" class="dialog-text">
            <p>
              Your files stay in your uloz.to account. Drive stops syncing and removes the Drive folder from this
              {windows ? "PC" : "computer"}, including downloaded copies.
            </p>
            {#if status && status.pending > 0}
              <p class="callout-inline">
                <Icon name="warning" size={16} />
                <span>
                  {plural(status.pending, "change hasn't", "changes haven't")} uploaded yet. Signing out now loses
                  {status.pending === 1 ? "it" : "them"}.
                </span>
              </p>
            {/if}
            <p>To sign in again, you'll need your encryption password.</p>
            {#if signOutError}
              <p class="error-text" role="alert"><Icon name="alert" size={14} />{signOutError}</p>
            {/if}
          </div>
        </div>
        <div class="dialog-actions">
          <button class="btn danger" aria-disabled={signingOut} onclick={signOut}>
            {signingOut ? "Signing out…" : "Sign out"}
          </button>
          <!-- svelte-ignore a11y_autofocus -->
          <button class="btn" onclick={() => signOutDialog?.close()} autofocus>Cancel</button>
        </div>
      </dialog>
    {:else if section === "recovery"}
      <div class="group">
        <div class="callout warning">
          <Icon name="lock" size={18} />
          <div>
            <p class="body-strong">Only you can unlock your files</p>
            <p>
              Your files are encrypted with your encryption password before they upload. uloz.to and the people who make
              Drive can't read them, and can't recover them if the password is lost.
            </p>
          </div>
        </div>
        <div class="card setting stacked">
          <div class="setting-text">
            <p>Recovery kit</p>
            <p class="caption secondary">
              A text file with your encryption password and an rclone configuration. With it you can open your files on
              any computer, even without Drive. Anyone who has it can read your files, so keep it in a password manager or
              print it and store it somewhere safe.
            </p>
          </div>
          <div>
            <button class="btn primary" aria-disabled={kitSaving} onclick={saveKit}>
              {kitSaving ? "Preparing…" : "Save recovery kit…"}
            </button>
          </div>
          <p class:error-text={kitResult && !kitResult.ok} class="caption kit-result selectable" role="status">
            {kitResult?.text ?? ""}
          </p>
        </div>
      </div>
    {/if}
  </main>
</div>

<style>
  .settings {
    display: grid;
    grid-template-columns: minmax(168px, 220px) minmax(0, 1fr);
    height: 100%;
  }
  nav {
    padding: var(--s4) var(--s2) var(--s4) var(--s3);
  }
  nav ul {
    display: grid;
    gap: 2px;
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .nav-item {
    position: relative;
    display: flex;
    align-items: center;
    gap: var(--s3);
    width: 100%;
    min-height: 36px;
    padding: 0 var(--s3);
    border: 0;
    border-radius: var(--radius-control);
    background: transparent;
    color: var(--text);
    font: inherit;
    text-align: start;
    transition: background-color 100ms ease-out;
  }
  @media (hover: hover) {
    .nav-item:hover {
      background: var(--fill-hover);
    }
  }
  .nav-item[aria-current="page"] {
    background: var(--fill-hover);
    font-weight: 600;
  }
  .nav-item[aria-current="page"]::before {
    content: "";
    position: absolute;
    inset-inline-start: 0;
    inset-block: 10px;
    width: 3px;
    border-radius: 2px;
    background: var(--accent);
  }

  main {
    display: flex;
    flex-direction: column;
    gap: var(--s4);
    min-width: 0;
    padding: var(--s6) var(--s6) var(--s8) var(--s4);
    overflow-y: auto;
    scrollbar-width: thin;
  }
  .group {
    display: grid;
    gap: var(--s1);
    max-width: 720px;
  }
  .group-title {
    margin: var(--s4) var(--s1) var(--s2);
  }
  .night-window {
    flex-wrap: wrap;
  }
  .night-error {
    flex-basis: 100%;
  }
  .time-label {
    color: var(--text-2);
  }
  .input.time {
    width: auto;
    min-width: 104px;
  }
  .group > .callout {
    margin-block: var(--s2) 0;
  }
  .group > .callout:first-child {
    margin-block: 0 var(--s2);
  }

  .setting {
    display: flex;
    align-items: center;
    gap: var(--s4);
    min-height: 64px;
    padding: var(--s3) var(--s4);
  }
  label.setting {
    cursor: default;
  }
  .setting.stacked {
    flex-direction: column;
    align-items: stretch;
    gap: var(--s3);
  }
  .setting-text {
    display: grid;
    gap: 2px;
    flex: 1;
    min-width: 0;
  }
  .path {
    overflow-wrap: anywhere;
  }
  .narrow {
    width: auto;
    min-width: 96px;
    max-width: 240px;
  }
  #device-name {
    width: 220px;
  }

  .usage {
    display: grid;
    gap: var(--s2);
    padding: var(--s4);
  }
  .usage-row {
    display: flex;
    justify-content: space-between;
    gap: var(--s4);
    flex-wrap: wrap;
  }

  .cache-control {
    display: flex;
    align-items: center;
    gap: var(--s4);
  }
  .cache-control input[type="range"] {
    flex: 1;
    min-width: 120px;
  }
  .gb-input {
    display: inline-flex;
    align-items: center;
    gap: var(--s2);
  }
  .gb-input .input {
    width: 88px;
  }

  .limit {
    display: flex;
    gap: var(--s2);
    align-items: center;
    flex-wrap: wrap;
    justify-content: flex-end;
  }
  .limit select {
    width: auto;
  }

  .avatar {
    display: grid;
    place-items: center;
    width: 40px;
    height: 40px;
    border-radius: 50%;
    background: var(--neutral-soft);
    color: var(--text-2);
    flex: none;
  }
  .token-row {
    display: flex;
    gap: var(--s2);
    flex-wrap: wrap;
  }
  .token-row .input {
    flex: 1;
    min-width: 200px;
  }

  .update-status {
    flex-wrap: wrap;
  }
  .update-meter {
    flex-basis: 100%;
  }
  .avatar.accent {
    background: var(--accent-soft);
    color: var(--accent-text);
  }
  .changelog {
    display: grid;
    gap: var(--s3);
    padding: var(--s4);
  }

  .about {
    display: flex;
    align-items: center;
    gap: var(--s4);
    padding: var(--s4);
  }

  .dialog-body {
    display: grid;
    gap: var(--s3);
    padding: var(--s6);
  }
  .dialog-text {
    display: grid;
    gap: var(--s3);
  }
  .callout-inline {
    display: flex;
    gap: var(--s2);
    padding: var(--s2) var(--s3);
    border-radius: var(--radius-control);
    background: var(--warning-soft);
  }
  .callout-inline :global(.icon) {
    color: var(--warning-icon);
    margin-block-start: 1px;
  }
  .dialog-actions {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: var(--s2);
    padding: var(--s6);
    border-block-start: 1px solid var(--stroke-divider);
    background: var(--bg);
  }
  :root[data-material] .dialog-actions {
    background: color-mix(in srgb, var(--bg-dialog, #fff), var(--text) 3%);
  }

  @media (max-width: 600px) {
    .settings {
      grid-template-columns: minmax(0, 1fr);
      grid-template-rows: auto minmax(0, 1fr);
    }
    nav {
      padding: var(--s2) var(--s3) 0;
    }
    nav ul {
      display: flex;
      flex-wrap: wrap;
    }
    .nav-item {
      white-space: nowrap;
    }
    .nav-item[aria-current="page"]::before {
      inset-block: auto 0;
      inset-inline: var(--s3);
      width: auto;
      height: 3px;
    }
    main {
      padding: var(--s4);
    }
    .setting:not(label) {
      flex-wrap: wrap;
    }
  }
</style>
