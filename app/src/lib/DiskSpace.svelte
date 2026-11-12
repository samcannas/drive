<script lang="ts">
  // Settings › Disk space: where the space on this device's disks goes, what Drive already keeps
  // off it, and what it can free. Freeing space never moves or deletes files: Drive backs a folder
  // up and makes it on-demand (Windows), or frees downloaded copies in the Drive folder.
  import { onMount, tick } from "svelte";
  import { api, errorOf } from "./api";
  import { stateLine } from "./backups";
  import { drive, refreshBackups, requestSpace } from "./drive.svelte";
  import { bytes, relative } from "./format";
  import Icon from "./Icon.svelte";
  import {
    breakdown,
    categoryLabel,
    diskLow,
    filesCompact,
    freeOf,
    freesWith,
    groupOf,
    hasResults,
    KEEP_CHOICES,
    KEEP_DEFAULT,
    scanProgress,
    scanShare,
  } from "./space";
  import type { SpaceItem } from "./types";

  let { windows, onsection }: { windows: boolean; onsection: (id: "backups") => void } = $props();

  const device = $derived(windows ? "PC" : "computer");
  const fileManager = $derived(windows ? "File Explorer" : "the file manager");

  const report = $derived(drive.space);
  const scan = $derived(report?.scan ?? null);
  const scanning = $derived(scan?.state === "scanning");
  /** Results to show: this scan's, or the previous one's while a new scan runs. */
  const results = $derived(hasResults(report) ? report : null);
  const share = $derived(report && scanning ? scanShare(report) : null);
  const largest = $derived(Math.max(1, ...(results?.items.map((i) => i.bytes) ?? [1])));
  const saved = $derived(results ? results.saved.drive_folder_bytes + results.saved.backups_bytes : 0);

  let loadError = $state<string | null>(null);
  let busy = $state<string | null>(null);
  let announce = $state("");
  /** An error next to the item that caused it. */
  let itemError = $state<{ key: string; text: string } | null>(null);
  /** Folders Drive started saving space in (or backing up) from here: item path → backup id. */
  let started = $state<Record<string, string>>({});
  /** The Drive folder's downloads freed from here: item path → bytes freed. */
  let freed = $state<Record<string, number>>({});
  let expanded = $state<Record<string, boolean>>({});

  function say(text: string) {
    announce = "";
    queueMicrotask(() => (announce = text));
  }

  onMount(() => {
    // The cached report, or a first scan.
    load(false);
  });

  async function load(rescan: boolean) {
    if (busy === "scan") return;
    busy = "scan";
    loadError = null;
    try {
      await requestSpace(rescan);
    } catch (e) {
      loadError = errorOf(e).message;
    } finally {
      busy = null;
    }
  }

  async function cancelScan() {
    if (busy === "cancel") return;
    busy = "cancel";
    try {
      await api.cancelSpaceScan();
      say(results ? "Check stopped. The results below are from the last check." : "Check stopped.");
    } catch (e) {
      loadError = errorOf(e).message;
    } finally {
      busy = null;
    }
  }

  // Say when a check starts and ends; the numbers in between change twice a second and stay silent.
  let lastState: string | null = null;
  $effect(() => {
    const now = scan?.state ?? null;
    const was = lastState;
    lastState = now;
    if (now === was) return;
    if (now === "scanning") say("Checking your disk…");
    else if (now === "done" && was === "scanning") {
      const free = report?.can_free_bytes ?? 0;
      say(free > 0 ? `Check finished. Drive can free up to ${bytes(free)}.` : "Check finished.");
    }
  });

  /** A stable element id for an item (paths aren't valid ids). */
  function idOf(item: SpaceItem): string {
    const key = item.path || item.name;
    let h = 0;
    for (let i = 0; i < key.length; i++) h = (Math.imul(h, 31) + key.charCodeAt(i)) | 0;
    return `space-${(h >>> 0).toString(36)}`;
  }
  const keyOf = (item: SpaceItem) => item.path || item.name;

  /** Whether the item's action is still on offer (not already done from here). */
  function offered(item: SpaceItem): boolean {
    if (!item.action || item.on_demand || started[keyOf(item)] || freed[keyOf(item)] !== undefined) return false;
    return item.action !== "open_system_cleanup" || windows;
  }

  const ACTION_LABEL = {
    save_space: "Save space…",
    turn_on_save_space: "Save space…",
    back_up: "Back up…",
    free_drive_cache: "Free up space…",
    open_system_cleanup: "Open Storage settings",
  } as const;

  /** Joins an action's label to the item it acts on, for screen readers: "Save space… in Videos". */
  const ACTION_TARGET = {
    save_space: "in",
    turn_on_save_space: "in",
    back_up: "",
    free_drive_cache: "in",
    open_system_cleanup: "for",
  } as const;

  /** Folders can be opened; buckets, the Recycle Bin and other people's files can't. */
  const canOpen = (item: SpaceItem) =>
    item.path !== "" && !["recycle_bin", "system", "other_users"].includes(item.category);

  async function reveal(item: SpaceItem) {
    itemError = null;
    try {
      await api.revealSpaceItem(item.path);
    } catch (e) {
      itemError = { key: keyOf(item), text: errorOf(e).message };
    }
  }

  async function systemCleanup(item: SpaceItem) {
    itemError = null;
    try {
      await api.openSystemCleanup();
    } catch (e) {
      itemError = { key: keyOf(item), text: errorOf(e).message };
    }
  }

  // ----- Confirmation before acting
  let dialog = $state<HTMLDialogElement>();
  let target = $state<SpaceItem | null>(null);
  let keep = $state(KEEP_DEFAULT);
  let acting = $state(false);
  let dialogError = $state<string | null>(null);
  const saving = $derived(target?.action === "save_space" || target?.action === "turn_on_save_space");

  async function act(item: SpaceItem) {
    itemError = null;
    if (item.action === "open_system_cleanup") return systemCleanup(item);
    target = item;
    keep = KEEP_DEFAULT;
    dialogError = null;
    // Render the dialog's content first, so Cancel takes focus when it opens.
    await tick();
    dialog?.showModal();
  }

  async function confirm() {
    if (!target || acting) return;
    const item = target;
    const key = keyOf(item);
    acting = true;
    dialogError = null;
    try {
      if (item.action === "save_space" || item.action === "turn_on_save_space") {
        const info = await api.saveSpace(item.path, keep);
        started = { ...started, [key]: info.id };
        await refreshBackups();
        say(`Saving space in ${item.name}. See the progress in Backups.`);
      } else if (item.action === "back_up") {
        const info = await api.addBackup(item.path);
        started = { ...started, [key]: info.id };
        await refreshBackups();
        say(`Backing up ${item.name}.`);
      } else if (item.action === "free_drive_cache") {
        const r = await api.freeDriveCache();
        freed = { ...freed, [key]: r.freed_bytes };
        say(`Freed ${bytes(r.freed_bytes)} in the Drive folder.`);
      }
      dialog?.close();
      // The button that opened the dialog is gone: continue from the item.
      await tick();
      document.getElementById(`${idOf(item)}-name`)?.focus();
    } catch (e) {
      dialogError = errorOf(e).message;
    } finally {
      acting = false;
    }
  }

  const confirmLabel = $derived.by(() => {
    if (!target) return "";
    if (acting) return target.action === "free_drive_cache" ? "Freeing up…" : "Starting…";
    switch (target.action) {
      case "back_up":
        return "Back up";
      case "free_drive_cache":
        return `Free up ${bytes(target.frees_bytes)}`;
      default:
        return "Save space";
    }
  });
</script>

{#snippet row(item: SpaceItem, child: boolean)}
  {@const id = idOf(item)}
  {@const key = keyOf(item)}
  {@const backupId = started[key]}
  {@const backup = backupId ? (drive.backups?.folders.find((f) => f.id === backupId) ?? null) : null}
  {@const line = backup ? stateLine(backup, drive.now) : null}
  {@const busyBackup = backupId !== undefined && backup?.state !== "up_to_date"}
  {@const open = offered(item)}
  {@const category = categoryLabel(item.category, windows)}
  {@const caption = [
    item.name.toLowerCase().includes(category.toLowerCase()) ? "" : category,
    item.files > 0 ? filesCompact(item.files) : "",
  ]
    .filter(Boolean)
    .join(" · ")}
  <li class="item" class:card={!child} class:child>
    <div class="item-head">
      <span class="space-swatch" style:background="var(--space-{groupOf(item.category)})" aria-hidden="true"></span>
      <div class="item-text">
        <h3 id="{id}-name" class:body-strong={!child} tabindex="-1">{item.name}</h3>
        {#if caption}
          <p class="caption secondary">{caption}</p>
        {/if}
        {#if item.path && !child}
          <p class="caption secondary path selectable">{item.path}</p>
        {/if}
      </div>
      <span class="size numeric" class:body-strong={!child}>{bytes(item.bytes)}</span>
    </div>

    <div class="item-body">
      {#if !child}
        <div class="share" aria-hidden="true">
          <span style:width="{(item.bytes / largest) * 100}%" style:background="var(--space-{groupOf(item.category)})"></span>
        </div>
      {/if}

      {#if item.on_demand && !busyBackup}
        <p class="caption line success">
          <Icon name="check-circle" size={14} /><span>Saving space. Files you haven't opened lately are only in the cloud.</span>
        </p>
      {/if}
      {#if busyBackup}
        <p class="caption line {line?.tone ?? 'accent'}">
          <Icon name={line?.icon ?? "sync"} size={14} /><span class="numeric">{line?.text ?? "Starting…"}</span>
        </p>
        <p class="caption">
          <button class="link" onclick={() => onsection("backups")}>See progress in Backups</button>
        </p>
      {/if}
      {#if freed[key] !== undefined}
        <p class="caption line success">
          <Icon name="check-circle" size={14} /><span>Freed {bytes(freed[key])}. The files are still in the cloud.</span>
        </p>
      {/if}

      {#if item.caution && open}
        <p class="caption line warning"><Icon name="warning" size={14} /><span>{item.caution}</span></p>
      {/if}
      {#if item.note}
        <p class="caption line secondary"><Icon name="info" size={14} /><span>{item.note}</span></p>
      {/if}

      {#if open && item.action !== "open_system_cleanup"}
        {#if item.action === "back_up"}
          <p class="caption secondary">Keeps a copy safe in the cloud. On Linux, it doesn't free space yet.</p>
        {:else if item.frees_bytes > 0}
          <p class="caption">Frees about <span class="body-strong numeric">{bytes(item.frees_bytes)}</span></p>
        {/if}
      {/if}

      {#if open || canOpen(item)}
        <div class="actions">
          {#if open && item.action}
            <button class="btn" onclick={() => act(item)}>
              {ACTION_LABEL[item.action]}<span class="visually-hidden">{` ${ACTION_TARGET[item.action]} ${item.name}`.replace("  ", " ")}</span>
              {#if item.action === "open_system_cleanup"}<Icon name="external" size={16} />{/if}
            </button>
          {/if}
          {#if canOpen(item)}
            <button class="btn subtle" onclick={() => reveal(item)}>
              Open in {fileManager}<span class="visually-hidden">{`: ${item.name}`}</span>
            </button>
          {/if}
        </div>
      {/if}

      {#if itemError?.key === key}
        <p class="error-text" role="alert"><Icon name="alert" size={14} />{itemError.text}</p>
      {/if}

      {#if !child && item.children.length}
        <button
          class="btn subtle disclosure"
          aria-expanded={expanded[key] ? "true" : "false"}
          aria-controls="{id}-children"
          onclick={() => (expanded = { ...expanded, [key]: !expanded[key] })}
        >
          <Icon name="chevron-right" size={16} class="chevron" />
          Largest folders inside<span class="visually-hidden">{` ${item.name}`}</span>
          <span class="secondary numeric">({item.children.length})</span>
        </button>
        <ul id="{id}-children" class="children" role="list" hidden={!expanded[key]}>
          {#each item.children as c (keyOf(c))}
            {@render row(c, true)}
          {/each}
        </ul>
      {/if}
    </div>
  </li>
{/snippet}

<div class="space">
  <p class="intro">
    See what takes up space on this {device}, and let Drive free some of it. Drive never moves or deletes your files.
  </p>
  <p class="visually-hidden" role="status">{announce}</p>

  {#if drive.spaceUnsupported}
    <div class="callout info">
      <Icon name="info" size={18} />
      <p>The Drive service running now can't check disk space. Quit Drive and open it again to finish updating.</p>
    </div>
  {:else}
    {#if loadError}
      <div class="callout danger" role="alert">
        <Icon name="alert" size={18} />
        <p>Couldn't check your disk. {loadError}</p>
        <div class="actions"><button class="btn" onclick={() => load(false)}>Try again</button></div>
      </div>
    {/if}

    {#if scanning && scan}
      <section class="card scan" aria-labelledby="scan-title">
        <div class="scan-head">
          <div class="scan-text">
            <h2 id="scan-title" class="body-strong">Checking your disk…</h2>
            <p class="secondary numeric">{scanProgress(scan)}</p>
          </div>
          <button class="btn" aria-disabled={busy === "cancel"} onclick={cancelScan}>
            Cancel<span class="visually-hidden">{" the check"}</span>
          </button>
        </div>
        {#if share !== null}
          <div
            class="meter"
            role="progressbar"
            aria-labelledby="scan-title"
            aria-valuemin="0"
            aria-valuemax="100"
            aria-valuenow={Math.round(share * 100)}
          >
            <span style:width="{share * 100}%"></span>
          </div>
        {/if}
        {#if scan.current}
          <p class="caption secondary truncate current" title={scan.current}>{scan.current}</p>
        {/if}
        {#if results}
          <p class="caption secondary">Until the check finishes, the list below is from the last check.</p>
        {/if}
      </section>
    {:else if scan?.state === "failed"}
      <div class="callout danger" role="alert">
        <Icon name="alert" size={18} />
        <div>
          <p class="body-strong">Couldn't check your disk</p>
          <p>{scan.error ?? "Something stopped the check."} Try again in a moment.</p>
        </div>
        <div class="actions">
          <button class="btn" aria-disabled={busy === "scan"} onclick={() => load(true)}>Try again</button>
        </div>
      </div>
    {:else if !results}
      {#if !report && !loadError}
        <p class="secondary" role="status">Loading…</p>
      {:else if report}
        <div class="card start">
          <span class="avatar" aria-hidden="true"><Icon name="pie" /></span>
          <div class="start-text">
            <p class="secondary">
              Drive adds up the size of the folders on this {device}. It only reads sizes: nothing is uploaded, moved or
              changed.
            </p>
            <div>
              <button class="btn primary" aria-disabled={busy === "scan"} onclick={() => load(true)}>
                Find out what's using space
              </button>
            </div>
          </div>
        </div>
      {/if}
    {/if}

    {#if results}
      {#if !scanning && results.scan.finished_ms}
        <p class="caption secondary checked">
          Checked {relative(results.scan.finished_ms, drive.now)} ·
          <button class="link" aria-disabled={busy === "scan"} onclick={() => load(true)}>Check again</button>
        </p>
      {/if}

      <div class="disks">
        {#each results.disks as disk, i (disk.path)}
          {@const used = Math.max(0, disk.total_bytes - disk.free_bytes)}
          {@const low = diskLow(disk.free_bytes, disk.total_bytes)}
          {@const segs = disk.scanned ? breakdown(results, disk, windows) : []}
          <section class="card disk" aria-labelledby="disk-{i}-name">
            <div class="disk-head">
              <h2 id="disk-{i}-name" class="body-strong">{disk.name}</h2>
              <p class="numeric disk-free" class:low>
                {#if low}<Icon name="warning" size={14} /><span>Almost full ·</span>{/if}
                <span>{freeOf(disk.free_bytes, disk.total_bytes)}</span>
              </p>
            </div>
            <div class="space-bar" aria-hidden="true">
              {#each segs as s (s.group)}
                <span style:flex="{s.bytes} 0 0" style:background="var(--space-{s.group})"></span>
              {/each}
              {#if !disk.scanned && used > 0}
                <span class="used" style:flex="{used} 0 0"></span>
              {/if}
              <span class="free" style:flex="{disk.free_bytes} 0 0"></span>
            </div>
            {#if disk.scanned}
              <ul class="legend" role="list" aria-label="What's on {disk.name}">
                {#each segs as s (s.group)}
                  <li>
                    <span class="space-swatch" style:background="var(--space-{s.group})" aria-hidden="true"></span>
                    <span class="legend-label">{s.label}</span>
                    <span class="numeric secondary">{bytes(s.bytes)}</span>
                  </li>
                {/each}
                <li>
                  <span class="space-swatch free" aria-hidden="true"></span>
                  <span class="legend-label">Free</span>
                  <span class="numeric secondary">{bytes(disk.free_bytes)}</span>
                </li>
              </ul>
            {:else}
              <p class="caption secondary">{bytes(used)} used. Drive didn't check what's on this disk.</p>
            {/if}
          </section>
        {/each}
      </div>

      {#if saved > 0}
        {@const s = results.saved}
        <div class="card saved">
          <span class="avatar success" aria-hidden="true"><Icon name="cloud-check" /></span>
          <div class="saved-text">
            <p class="body-strong">Drive already keeps {bytes(saved)} off this {device}</p>
            <p class="caption secondary">
              {#if s.drive_folder_bytes > 0 && s.backups_bytes > 0}
                {bytes(s.drive_folder_bytes)} from your Drive folder and {bytes(s.backups_bytes)} from backed-up folders.
              {:else if s.backups_bytes > 0}
                From backed-up folders that save space.
              {:else}
                From your Drive folder.
              {/if}
              These files are only in the cloud and download when you open them.
            </p>
          </div>
        </div>
      {/if}

      {#if results.can_free_bytes > 0}
        <div class="callout info">
          <Icon name="pie" size={18} />
          <div>
            <p class="body-strong">Drive can free up to {bytes(results.can_free_bytes)}</p>
            <p>Choose a folder below to save space in. Files stay where they are, and nothing is deleted.</p>
          </div>
        </div>
      {:else if !windows && results.items.some((i) => i.action === "back_up")}
        <div class="callout info">
          <Icon name="info" size={18} />
          <p>On Linux, Drive can back up your folders to keep them safe, but it can't free space in them yet.</p>
        </div>
      {/if}

      <section aria-labelledby="space-items-title">
        <h2 id="space-items-title" class="body-strong list-title">What's using space</h2>
        <ul class="items" role="list">
          {#each results.items as item (keyOf(item))}
            {@render row(item, false)}
          {/each}
        </ul>
      </section>
    {/if}
  {/if}

  <dialog bind:this={dialog} aria-labelledby="space-dialog-title" aria-describedby="space-dialog-body">
    {#if target}
      <div class="dialog-body">
        <h2 id="space-dialog-title" class="subtitle">
          {#if saving}
            Save space in {target.name}?
          {:else if target.action === "back_up"}
            Back up {target.name}?
          {:else}
            Free up {bytes(target.frees_bytes)} in the Drive folder?
          {/if}
        </h2>
        <div class="dialog-text">
          {#if saving}
            <p id="space-dialog-body">
              {target.name} stays where it is. Files you haven't opened lately are kept only in the cloud and download
              again when you open them.
              {target.action === "save_space"
                ? `Drive first backs up ${target.name}, then frees space as files are safely in the cloud.`
                : ""}
              Nothing is deleted, and you can switch this off anytime in Backups.
            </p>
            <div class="keep">
              <label for="space-keep">Keep recently used files on this {device} up to</label>
              <select id="space-keep" class="input" bind:value={keep}>
                {#each KEEP_CHOICES as n}
                  <option value={n}>{bytes(n)}</option>
                {/each}
              </select>
            </div>
            <p>Frees about <span class="body-strong numeric">{bytes(freesWith(target, keep))}</span></p>
          {:else if target.action === "back_up"}
            <p id="space-dialog-body">
              Drive keeps a copy of {target.name} in your encrypted cloud, so your files are safe if this {device} breaks.
              The folder stays where it is, and nothing in it changes.
            </p>
            <p>On Linux, backing up doesn't free space yet: every file also stays on this {device}.</p>
          {:else}
            <p id="space-dialog-body">
              Downloaded copies of files in your Drive folder are removed from this {device}. The files stay in the cloud
              and download again when you open them.
            </p>
            <p>Files you chose to always keep on this device, and changes waiting to upload, stay.</p>
          {/if}
          {#if target.caution}
            <p class="callout-inline"><Icon name="warning" size={16} /><span>{target.caution}</span></p>
          {/if}
          {#if dialogError}
            <p class="error-text" role="alert"><Icon name="alert" size={14} />{dialogError}</p>
          {/if}
        </div>
      </div>
      <div class="dialog-actions">
        <button class="btn primary" aria-disabled={acting} onclick={confirm}>{confirmLabel}</button>
        <!-- svelte-ignore a11y_autofocus -->
        <button class="btn" onclick={() => dialog?.close()} autofocus>Cancel</button>
      </div>
    {/if}
  </dialog>
</div>

<style>
  .space {
    display: grid;
    gap: var(--s2);
    max-width: 720px;
  }
  .intro {
    max-width: 640px;
    margin-block-end: var(--s2);
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
  .avatar.success {
    background: var(--success-soft);
    color: var(--success-text);
  }

  /* Start, progress */
  .start {
    display: flex;
    align-items: flex-start;
    gap: var(--s4);
    padding: var(--s4);
  }
  .start-text {
    display: grid;
    gap: var(--s3);
    flex: 1;
    min-width: 0;
  }
  .scan {
    display: grid;
    gap: var(--s2);
    padding: var(--s4);
  }
  .scan-head {
    display: flex;
    align-items: flex-start;
    gap: var(--s4);
  }
  .scan-text {
    display: grid;
    gap: 2px;
    flex: 1;
    min-width: 0;
  }
  .current {
    direction: ltr;
  }
  .checked {
    margin-inline: var(--s1);
  }
  .checked .link {
    font-size: inherit;
  }

  /* Disks */
  .disks {
    display: grid;
    gap: var(--s2);
  }
  .disk {
    display: grid;
    gap: var(--s3);
    padding: var(--s4);
  }
  .disk-head {
    display: flex;
    flex-wrap: wrap;
    justify-content: space-between;
    align-items: baseline;
    gap: var(--s1) var(--s4);
  }
  .disk-free {
    display: inline-flex;
    align-items: center;
    gap: var(--s1);
    color: var(--text-2);
  }
  .disk-free.low {
    color: var(--warning-text);
    font-weight: 600;
  }
  .disk-free.low :global(.icon) {
    color: var(--warning-icon);
  }
  .space-bar {
    display: flex;
    gap: 2px;
    height: 12px;
    border-radius: var(--radius-control);
    overflow: hidden;
  }
  .space-bar > span {
    min-width: 3px;
  }
  .space-bar > .used {
    background: var(--space-other);
  }
  .space-bar > .free,
  .space-swatch.free {
    background: var(--track);
  }
  .legend {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(min(100%, 200px), 1fr));
    gap: var(--s1) var(--s6);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .legend li {
    display: flex;
    align-items: center;
    gap: var(--s2);
  }
  .legend-label {
    flex: 1;
    min-width: 0;
  }
  .space-swatch {
    flex: none;
    width: 10px;
    height: 10px;
    border-radius: 2px;
  }

  .saved {
    display: flex;
    align-items: center;
    gap: var(--s4);
    padding: var(--s3) var(--s4);
  }
  .saved-text {
    display: grid;
    gap: 2px;
    min-width: 0;
  }

  .callout {
    margin-block: var(--s2) 0;
  }
  .list-title {
    margin: var(--s4) var(--s1) var(--s2);
  }

  /* Items */
  .items,
  .children {
    display: grid;
    gap: var(--s1);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .item {
    display: grid;
    gap: var(--s2);
    padding: var(--s3) var(--s4);
  }
  .item-head {
    display: flex;
    align-items: flex-start;
    gap: var(--s3);
  }
  .item-head > .space-swatch {
    margin-block-start: 5px;
  }
  .item-text {
    display: grid;
    gap: 2px;
    flex: 1;
    min-width: 0;
  }
  .size {
    flex: none;
    white-space: nowrap;
  }
  .path {
    overflow-wrap: anywhere;
  }
  .item-body {
    display: grid;
    gap: var(--s2);
    /* Lines up with the name, past the swatch. */
    padding-inline-start: calc(10px + var(--s3));
  }
  .share {
    height: 4px;
    border-radius: 2px;
    background: var(--track);
    overflow: hidden;
  }
  .share > span {
    display: block;
    height: 100%;
    min-width: 2px;
    border-radius: 2px;
  }
  .line {
    display: flex;
    gap: var(--s1);
    align-items: flex-start;
  }
  .line :global(.icon) {
    margin-block-start: 1px;
  }
  .line.success :global(.icon) {
    color: var(--success-text);
  }
  .line.accent :global(.icon) {
    color: var(--accent-text);
  }
  .line.warning {
    color: var(--warning-text);
  }
  .line.warning :global(.icon) {
    color: var(--warning-icon);
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--s2);
  }
  /* A subtle button on its own: its text, not its padding, lines up with the lines above. */
  .actions > .btn.subtle:first-child {
    margin-inline-start: -12px;
  }
  .disclosure {
    justify-self: start;
    /* Subtle buttons: the icon, not the padding, lines up with the lines above. */
    margin-inline-start: -10px;
  }
  .disclosure :global(.chevron) {
    color: var(--text-2);
  }
  @media (prefers-reduced-motion: no-preference) {
    .disclosure :global(.chevron) {
      transition: rotate 150ms ease-out;
    }
  }
  .disclosure[aria-expanded="true"] :global(.chevron) {
    rotate: 90deg;
  }
  .children {
    gap: 0;
    border-block-start: 1px solid var(--stroke-divider);
  }
  .children[hidden] {
    display: none;
  }
  .item.child {
    padding: var(--s3) 0 var(--s3);
    border-block-end: 1px solid var(--stroke-divider);
  }
  .item.child:last-child {
    border-block-end: 0;
    padding-block-end: 0;
  }

  .keep {
    display: grid;
    gap: var(--s1);
  }
  .keep select {
    width: auto;
    justify-self: start;
    min-width: 120px;
  }
</style>
