<script lang="ts">
  import { onMount, tick } from "svelte";
  import { api, env, errorOf, startDragging } from "../lib/api";
  import { connect, drive, refreshIssues, refreshTransfers } from "../lib/drive.svelte";
  import { isBusy, offerable, offerBytes, offerTitle, progressOf, splitBackupPath } from "../lib/backups";
  import { basename, bytes, count, dirname, plural, relative } from "../lib/format";
  import Icon from "../lib/Icon.svelte";
  import StateBadge from "../lib/StateBadge.svelte";
  import TransferRow from "../lib/TransferRow.svelte";
  import { ACTION_MODE, actionDone, groupsOf, ordered, summarize, type RowAction } from "../lib/transfers";
  import UpdateButton from "../lib/UpdateButton.svelte";
  import { ACTION_LABEL, ACTIVITY, ISSUE_TITLE, stateVisual } from "../lib/status";
  import type { Activity, BackupInfo, Issue, IssueAction, TransferItem } from "../lib/types";

  const flyout = env().platform === "windows";

  let panel = $state<"home" | "issues" | "transfers">("home");
  let busy = $state<string | null>(null);
  let actionError = $state<string | null>(null);
  let issuesHeading = $state<HTMLHeadingElement>();
  let issuesButton = $state<HTMLButtonElement>();
  let transfersHeading = $state<HTMLHeadingElement>();
  let transfersButton = $state<HTMLButtonElement>();
  let transfersTitle = $state<HTMLHeadingElement>();
  /** Polite announcement after a transfer action (the row itself changes silently). */
  let announce = $state("");
  /** Shown as rows on the home panel; the rest is behind "Show all". */
  const ROWS = 3;

  const status = $derived(drive.status);
  const connected = $derived(drive.connection.state === "connected");
  const visual = $derived(stateVisual(status, drive.connection));
  const setUp = $derived(connected && status !== null && status.state !== "not_set_up");
  const paused = $derived(status?.state === "paused");
  const canPause = $derived(setUp && status !== null && !["signed_out", "not_set_up"].includes(status.state));
  const massDelete = $derived(drive.issues.find((i) => i.kind === "mass_delete") ?? null);
  const issueCount = $derived(status?.issues ?? drive.issues.length);
  const transferring = $derived(status !== null && status.uploading + status.downloading > 0);
  const transfers = $derived(drive.transfers);
  const device = flyout ? "PC" : "computer";
  const fileManager = flyout ? "File Explorer" : "the file manager";
  const summary = $derived(transfers && transfers.items.length ? summarize(transfers, device) : null);
  const backups = $derived(drive.backups);
  /** Backups worth a line on the home panel: scanning, uploading, or stuck. */
  const backupRows = $derived(backups?.folders.filter((b) => isBusy(b) || b.state === "problem") ?? []);
  /** The one calm suggestion to back up Desktop, Documents…; never while something else needs attention. */
  const offer = $derived.by(() => {
    if (!backups || backups.suggestions_dismissed || backups.folders.length >= 2) return null;
    if (!status || !["up_to_date", "syncing"].includes(status.state) || status.pending_delete_confirmation) return null;
    const items = offerable(backups);
    if (!items.length) return null;
    const size = offerBytes(items);
    return { title: offerTitle(items), size };
  });
  let recentHeading = $state<HTMLHeadingElement>();
  // Ticks every second while something waits to retry, so its countdown stays honest.
  let clock = $state(Date.now());
  $effect(() => {
    if (!transfers?.items.some((t) => t.retry_at_ms)) return;
    const t = setInterval(() => (clock = Date.now()), 1000);
    return () => clearInterval(t);
  });

  onMount(() => {
    connect({ activity: true, issues: true, settings: true, transfers: true, backups: true });
    if (flyout) {
      const onKey = (e: KeyboardEvent) => {
        // An open dialog handles Escape itself.
        if (e.key === "Escape" && !e.defaultPrevented && !document.querySelector("dialog[open]")) {
          if (panel !== "home") showHome();
          else api.hidePopover();
        }
      };
      window.addEventListener("keydown", onKey);
      return () => window.removeEventListener("keydown", onKey);
    }
  });

  async function run(key: string, fn: () => Promise<unknown>) {
    if (busy) return;
    busy = key;
    actionError = null;
    try {
      await fn();
    } catch (e) {
      actionError = errorOf(e).message;
    } finally {
      busy = null;
    }
  }

  async function showIssues() {
    panel = "issues";
    refreshIssues();
    await tick();
    issuesHeading?.focus();
  }

  async function showTransfers() {
    panel = "transfers";
    refreshTransfers();
    await tick();
    transfersHeading?.focus();
  }

  async function showHome() {
    const from = panel;
    panel = "home";
    await tick();
    if (from === "transfers") (transfersButton ?? transfersTitle)?.focus();
    else issuesButton?.focus();
  }

  function backupOf(id: string | null | undefined): BackupInfo | null {
    return (id && backups?.folders.find((f) => f.id === id)) || null;
  }

  /** What happened where: "Uploaded in Projects/Garden", or for a backup "Backed up from Documents". */
  function activityCaption(item: Activity): string {
    const kind = ACTIVITY[item.kind];
    if (item.backup) {
      const name = backupOf(item.backup)?.name;
      const backup = name ? `the ${name} backup` : "a backup";
      // "Uploaded" reads oddly for a backup; say what happened to the copy.
      if (item.kind === "uploaded") return name ? `Backed up from ${name}` : "Backed up";
      if (item.kind === "deleted") return `Removed from ${backup}`;
      return `${kind.verb} in ${backup}`;
    }
    const dir = dirname(item.path);
    return `${kind.verb}${dir ? ` in ${dir}` : ""}`;
  }

  function activityPath(item: Activity): string {
    const name = item.backup ? backupOf(item.backup)?.name : null;
    return name ? `${name} › ${item.path}` : item.path;
  }

  function issuePath(issue: Issue): string | null {
    if (!issue.path) return null;
    const name = issue.backup ? backupOf(issue.backup)?.name : null;
    return name ? `${name} › ${issue.path}` : issue.path;
  }

  function dismissOffer() {
    return run("offer", async () => {
      drive.backups = await api.dismissBackupSuggestions();
      announce = "";
      queueMicrotask(() => (announce = "Suggestion hidden. You can back up folders anytime from Settings."));
      await tick();
      recentHeading?.focus();
    });
  }

  function transferAction(item: TransferItem, action: RowAction) {
    if (action === "reveal") {
      const inBackup = splitBackupPath(item.path, backups);
      return run(`reveal:t${item.id}`, () =>
        inBackup ? api.revealItem(inBackup.rel, inBackup.backup.id) : api.revealItem(item.path),
      );
    }
    return run(`t${item.id}:${action}`, async () => {
      drive.transfers = await api.setTransferMode(item.id, ACTION_MODE[action]);
      announce = "";
      queueMicrotask(() => (announce = actionDone(action, item)));
      // The row may have moved to another group or out of the top rows: keep focus nearby.
      await tick();
      if (document.activeElement === document.body) {
        const again = document.querySelector<HTMLElement>(`[data-transfer-id="${item.id}"] [aria-haspopup="menu"]`);
        (again ?? (panel === "transfers" ? transfersHeading : transfersTitle))?.focus();
      }
    });
  }

  function act(issue: Issue, action: IssueAction) {
    if (action === "sign_in") return run(`sign_in`, () => api.showWindow("onboarding"));
    return run(`${issue.id}:${action}`, async () => {
      await api.resolveIssue(issue.id, action);
      await refreshIssues();
      if (panel === "issues" && drive.issues.length === 0) showHome();
    });
  }

  // The header doubles as the title bar: press and drag anywhere on it except its buttons.
  function dragHeader(e: MouseEvent) {
    if (e.button !== 0 || e.detail > 1) return;
    if ((e.target as HTMLElement).closest("button, a, input, select, textarea, [data-no-drag]")) return;
    e.preventDefault();
    startDragging();
  }

  function transferLine(up: number, down: number): string {
    const parts: string[] = [];
    if (up) parts.push(`Uploading ${plural(up, "file", "files")}`);
    if (down) parts.push(`Downloading ${plural(down, "file", "files")}`);
    return parts.join(" · ");
  }
</script>

<div class="popover" class:flyout>
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <header class="head" onmousedown={dragHeader}>
    <StateBadge {visual} />
    <div class="head-text">
      <h1 class="headline">{visual.headline}</h1>
      {#if setUp && status?.account}
        <p class="caption secondary truncate">
          <span title={status.account}>{status.account}</span>{#if status.last_sync_ms && status.state === "up_to_date"}{` · checked ${relative(status.last_sync_ms, drive.now)}`}{/if}
        </p>
      {/if}
    </div>
    {#if drive.connection.mock}
      <span class="mock caption" title="The app is showing made-up data from the development mock.">Mock data</span>
    {/if}
    <UpdateButton />
    <button
      class="btn subtle icon-only close"
      onclick={() => api.closeMain()}
      aria-label={drive.settings?.close_action === "quit" ? "Quit Drive" : "Close"}
      title={drive.settings?.close_action === "quit" ? "Quit Drive" : "Close (Drive keeps syncing in the tray)"}
    >
      <Icon name="x" size={16} />
    </button>
  </header>

  <main class="body">
    {#if panel === "transfers"}
      <section class="transfers-panel" aria-labelledby="transfers-panel-title">
        <div class="section-head top">
          <button class="btn subtle icon-only" onclick={showHome} aria-label="Back to status">
            <Icon name="chevron-left" size={16} />
          </button>
          <div class="panel-title">
            <h2 id="transfers-panel-title" class="body-strong" tabindex="-1" bind:this={transfersHeading}>Transfers</h2>
            {#if summary}
              <p class="caption secondary">{summary.title}</p>
              {#if summary.detail}
                <p class="caption secondary">{summary.detail}</p>
              {/if}
            {/if}
          </div>
        </div>
        {#if actionError}
          <p class="error-text" role="alert"><Icon name="alert" size={16} />{actionError}</p>
        {/if}
        {#if !transfers || transfers.items.length === 0}
          <p class="secondary empty">Nothing is uploading or downloading.</p>
        {:else}
          {#each groupsOf(transfers) as group (group.key)}
            <section aria-labelledby="group-{group.key}">
              <h3 id="group-{group.key}" class="caption secondary section-label">
                {group.label} <span class="numeric">· {count(group.items.length)}</span>
              </h3>
              <ul class="card transfer-list" role="list">
                {#each group.items as item (item.id)}
                  <TransferRow {item} now={clock} {fileManager} onaction={transferAction} full />
                {/each}
              </ul>
            </section>
          {/each}
          {#if transfers.total_count > transfers.items.length}
            <p class="caption secondary panel-note">
              Showing the first {count(transfers.items.length)} of {count(transfers.total_count)}. The rest appear as these
              finish.
            </p>
          {/if}
        {/if}
      </section>
    {:else if panel === "issues"}
      <section class="issues" aria-labelledby="issues-title">
        <div class="section-head">
          <button class="btn subtle icon-only" onclick={showHome} aria-label="Back to status">
            <Icon name="chevron-left" size={16} />
          </button>
          <h2 id="issues-title" class="body-strong" tabindex="-1" bind:this={issuesHeading}>Needs your attention</h2>
        </div>
        {#if drive.issues.length === 0}
          <p class="secondary empty">Nothing needs your attention.</p>
        {/if}
        <ul class="issue-list" role="list">
          {#each drive.issues as issue (issue.id)}
            <li class="card issue">
              <Icon name={issue.kind === "mass_delete" || issue.kind === "disk_full" ? "warning" : "alert"} size={18} />
              <div class="issue-text">
                <h3 class="body-strong">{ISSUE_TITLE[issue.kind]}</h3>
                <p class="selectable">{issue.message}</p>
                {#if issuePath(issue)}
                  <p class="caption secondary path selectable">{issuePath(issue)}</p>
                {/if}
                {#if issue.actions.length}
                  <div class="issue-actions">
                    {#each issue.actions as action}
                      <button
                        class="btn"
                        class:danger={action === "confirm_delete"}
                        class:primary={action === "sign_in"}
                        aria-disabled={busy !== null}
                        onclick={() => act(issue, action)}
                      >
                        {busy === `${issue.id}:${action}` ? "Working…" : ACTION_LABEL[action]}
                      </button>
                    {/each}
                  </div>
                {/if}
              </div>
            </li>
          {/each}
        </ul>
      </section>
    {:else}
      {#if drive.connection.state === "unavailable"}
        <div class="callout danger" role="alert">
          <Icon name="alert" size={18} />
          <p>Drive keeps trying to start in the background. If this doesn't fix itself, restart your computer.</p>
          <div class="actions">
            <button class="btn" onclick={() => api.reconnect()}>Try again</button>
          </div>
        </div>
      {:else if !connected}
        <p class="secondary empty" role="status">Connecting to Drive…</p>
      {:else if status?.state === "not_set_up"}
        <div class="setup">
          <Icon name="lock" size={32} />
          <h2 class="subtitle">Set up Drive</h2>
          <p class="secondary">
            Connect your uloz.to account to keep a folder on this PC in sync. Files are encrypted before they leave your
            device.
          </p>
          <button class="btn primary" onclick={() => api.showWindow("onboarding")}>Set up Drive</button>
        </div>
      {:else if status}
        {#if status.state === "signed_out"}
          <div class="callout warning">
            <Icon name="user" size={18} />
            <p>uloz.to no longer accepts the saved sign-in. Your files are safe, and syncing resumes after you sign in.</p>
            <div class="actions">
              <button class="btn primary" onclick={() => api.showWindow("onboarding")}>Sign in</button>
            </div>
          </div>
        {/if}

        {#if status.pending_delete_confirmation}
          {@const n = status.pending_delete_confirmation}
          <div class="callout warning" role="region" aria-labelledby="massdelete-title">
            <Icon name="warning" size={18} />
            <div>
              <p class="body-strong" id="massdelete-title">Delete {plural(n, "item", "items")} from the cloud?</p>
              <p>
                {plural(n, "item was", "items were")} deleted from your Drive folder. Drive is waiting for you before it deletes
                them from uloz.to too.
              </p>
            </div>
            <div class="actions">
              <button
                class="btn"
                aria-disabled={busy !== null || !massDelete}
                onclick={() => massDelete && act(massDelete, "restore_deleted")}
              >
                {busy === `${massDelete?.id}:restore_deleted` ? "Restoring…" : "Restore files"}
              </button>
              <button
                class="btn danger"
                aria-disabled={busy !== null || !massDelete}
                onclick={() => massDelete && act(massDelete, "confirm_delete")}
              >
                {busy === `${massDelete?.id}:confirm_delete` ? "Deleting…" : "Delete from cloud"}
              </button>
            </div>
          </div>
        {/if}

        {#if issueCount > 0}
          <button class="attention" onclick={showIssues} bind:this={issuesButton}>
            <Icon name="alert" size={18} />
            <span>{plural(issueCount, "item needs", "items need")} attention</span>
            <Icon name="chevron-right" size={16} />
          </button>
        {/if}

        {#if actionError}
          <p class="error-text" role="alert"><Icon name="alert" size={16} />{actionError}</p>
        {/if}

        {#if transfers && summary}
          {@const rows = ordered(transfers.items).slice(0, ROWS)}
          <section class="card block transfers" aria-labelledby="transfers-title">
            <div class="transfers-head">
              <h2 id="transfers-title" class="body-strong" tabindex="-1" bind:this={transfersTitle}>{summary.title}</h2>
              {#if summary.detail}
                <p class="caption secondary">{summary.detail}</p>
              {/if}
            </div>
            <ul class="transfer-list" role="list">
              {#each rows as item (item.id)}
                <TransferRow {item} now={clock} {fileManager} onaction={transferAction} />
              {/each}
            </ul>
            <!-- Also when every row fits but a name is shortened: the full list shows whole names. -->
            {#if transfers.total_count > rows.length || rows.some((r) => basename(r.path).length > 32)}
              <button class="btn subtle show-all" onclick={showTransfers} bind:this={transfersButton}>
                <span>Show all<span class="visually-hidden"> transfers</span> <span class="numeric">({count(transfers.total_count)})</span></span>
                <Icon name="chevron-right" size={16} />
              </button>
            {/if}
          </section>
        {:else if (!transfers && transferring) || status.pending > 0}
          <section class="card block" aria-label="Transfers">
            {#if transferring && !transfers}
              {@const total = status.transfer_bytes_total}
              {@const done = Math.min(status.transfer_bytes_done, total)}
              <div class="row">
                <span>{transferLine(status.uploading, status.downloading)}</span>
                {#if total > 0}
                  <span class="caption secondary numeric">{bytes(done)} of {bytes(total)}</span>
                {/if}
              </div>
              {#if total > 0}
                <div
                  class="meter"
                  role="progressbar"
                  aria-label="Transfer progress"
                  aria-valuemin="0"
                  aria-valuemax="100"
                  aria-valuenow={Math.round((done / total) * 100)}
                >
                  <span style:width="{(done / total) * 100}%"></span>
                </div>
              {/if}
            {:else}
              <p>{plural(status.pending, "change is", "changes are")} waiting to sync.</p>
            {/if}
          </section>
        {/if}

        {#if backupRows.length}
          <section class="card block backups" aria-label="Backups">
            <ul class="backup-list" role="list">
              {#each backupRows as b (b.id)}
                {@const share = progressOf(b)}
                <li class="backup-row">
                  {#if b.state === "problem"}
                    <p class="with-icon problem">
                      <Icon name="warning" size={16} /><span><span class="body-strong">{b.name}</span> can't be backed up</span>
                    </p>
                    {#if b.detail}<p class="caption secondary indent">{b.detail}</p>{/if}
                  {:else if b.state === "scanning"}
                    <p class="with-icon"><Icon name="backup" size={16} />Getting {b.name} ready to back up</p>
                    {#if b.detail}<p class="caption secondary indent numeric">{b.detail}</p>{/if}
                  {:else}
                    <div class="row">
                      <span class="with-icon truncate"><Icon name="backup" size={16} />Backing up {b.name}</span>
                      <span class="caption secondary numeric">{count(b.done_files)} of {plural(b.files, "file", "files")}</span>
                    </div>
                    <div
                      class="meter"
                      role="progressbar"
                      aria-label="Backing up {b.name}"
                      aria-valuemin="0"
                      aria-valuemax="100"
                      aria-valuenow={Math.round(share * 100)}
                      aria-valuetext="{count(b.done_files)} of {plural(b.files, 'file', 'files')}, {bytes(b.done_bytes)} of {bytes(b.bytes)}"
                    >
                      <span style:width="{share * 100}%"></span>
                    </div>
                  {/if}
                </li>
              {/each}
            </ul>
            <button class="btn subtle show-all" onclick={() => api.showWindow("settings", "backups")}>
              <span>Manage backups</span>
              <Icon name="chevron-right" size={16} />
            </button>
          </section>
        {/if}

        {#if offer}
          <section class="card block offer" aria-labelledby="offer-title" aria-describedby="offer-desc">
            <Icon name="backup" size={18} />
            <div class="offer-text">
              <h2 id="offer-title" class="body-strong">{offer.title}</h2>
              <p id="offer-desc" class="caption secondary">
                {offer.size !== null ? `${bytes(offer.size)} on this ${device}. ` : ""}Drive can keep a copy in the cloud
                without moving them.
              </p>
              <div class="offer-actions">
                <button class="btn" onclick={() => api.showWindow("settings", "backups")}>Back up…</button>
                <button class="btn subtle" aria-disabled={busy === "offer"} onclick={dismissOffer}>Not now</button>
              </div>
            </div>
          </section>
        {/if}

        <section class="card block storage" aria-label="Storage">
          <div class="row">
            <span class="with-icon"><Icon name="storage" size={16} />On this PC</span>
            <span class="caption secondary numeric">{bytes(status.cache_bytes)} of {bytes(status.cache_limit_bytes)}</span>
          </div>
          <div
            class="meter"
            role="meter"
            aria-label="Downloaded files"
            aria-valuemin="0"
            aria-valuemax={status.cache_limit_bytes}
            aria-valuenow={Math.min(status.cache_bytes, status.cache_limit_bytes)}
            aria-valuetext="{bytes(status.cache_bytes)} of {bytes(status.cache_limit_bytes)}"
          >
            <span style:width="{Math.min(100, (status.cache_bytes / Math.max(1, status.cache_limit_bytes)) * 100)}%"></span>
          </div>
          {#if status.pinned_bytes > 0}
            <p class="caption secondary">Plus {bytes(status.pinned_bytes)} you chose to keep on this device</p>
          {/if}
          <div class="row cloud-row">
            <span class="with-icon"><Icon name="cloud" size={16} />In the cloud</span>
            <span class="caption secondary numeric">
              {bytes(status.cloud_bytes)} · {plural(status.cloud_files, "file", "files")}
            </span>
          </div>
        </section>

        <section class="recent" aria-labelledby="recent-title">
          <h2 id="recent-title" class="caption secondary section-label" tabindex="-1" bind:this={recentHeading}>
            Recent activity
          </h2>
          {#if drive.activity.length === 0}
            <p class="secondary empty">Changes to your Drive folder show up here.</p>
          {:else}
            <ul class="activity" role="list">
              {#each drive.activity as item (item.id)}
                {@const kind = ACTIVITY[item.kind]}
                <li>
                  <button
                    class="activity-row"
                    title="Show {activityPath(item)} in {fileManager}"
                    onclick={() => run(`reveal:${item.id}`, () => api.revealItem(item.path, item.backup ?? null))}
                  >
                    <Icon name={item.backup && item.kind === "uploaded" ? "backup" : kind.icon} size={18} class="kind {item.kind}" />
                    <span class="activity-text">
                      <span class="truncate name">{basename(item.path)}</span>
                      <span class="truncate caption secondary">
                        {activityCaption(item)} · {relative(item.at_ms, drive.now)}
                      </span>
                    </span>
                  </button>
                </li>
              {/each}
            </ul>
          {/if}
        </section>
      {/if}
    {/if}
    <p class="visually-hidden" role="status">{announce}</p>
  </main>

  <footer class="foot">
    <button class="btn subtle foot-btn" disabled={!setUp || !status?.folder} onclick={() => run("open", api.openFolder)}>
      <Icon name="folder" />
      <span>Open folder</span>
    </button>
    <button
      class="btn subtle foot-btn"
      disabled={!canPause}
      aria-disabled={busy === "pause"}
      onclick={() => run("pause", paused ? api.resume : api.pause)}
    >
      <Icon name={paused ? "play" : "pause"} />
      <span>{paused ? "Resume" : "Pause"}</span>
    </button>
    <button class="btn subtle foot-btn" onclick={() => api.showWindow("settings")}>
      <Icon name="gear" />
      <span>Settings</span>
    </button>
  </footer>
</div>

<style>
  .popover {
    display: grid;
    grid-template-rows: auto minmax(0, 1fr) auto;
    height: 100%;
  }
  .flyout {
    /* The OS draws the flyout's border and rounded corners. */
    overflow: hidden;
  }

  .head {
    position: relative;
    display: flex;
    align-items: center;
    gap: var(--s3);
    padding: var(--s4) var(--s3) var(--s3) var(--s4);
    cursor: default;
    user-select: none;
  }
  .close {
    align-self: flex-start;
    margin-top: calc(-1 * var(--s1));
    flex: none;
  }
  .head-text {
    display: grid;
    min-width: 0;
    flex: 1;
  }
  .headline {
    font-size: 0.9375rem;
    line-height: 1.33;
    font-weight: 600;
    overflow-wrap: anywhere;
  }
  .mock {
    align-self: flex-start;
    padding: 0 var(--s2);
    border-radius: 999px;
    background: var(--warning-soft);
    color: var(--warning-text);
    white-space: nowrap;
  }

  .body {
    display: flex;
    flex-direction: column;
    gap: var(--s2);
    padding: 0 var(--s3) var(--s3);
    overflow-y: auto;
    overscroll-behavior: contain;
    scrollbar-width: thin;
  }

  .block {
    display: grid;
    gap: var(--s2);
    padding: var(--s3);
  }
  .row {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    gap: var(--s3);
  }
  .row > .caption {
    flex: none;
    white-space: nowrap;
  }
  .with-icon {
    display: inline-flex;
    align-items: center;
    gap: var(--s2);
  }
  .with-icon :global(.icon) {
    color: var(--text-2);
    align-self: center;
  }
  .storage .caption.secondary:not(.numeric) {
    padding-inline-start: 24px;
  }
  .cloud-row {
    margin-block-start: var(--s1);
  }

  .backups {
    gap: var(--s3);
  }
  .backup-list {
    display: grid;
    gap: var(--s3);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .backup-row {
    display: grid;
    gap: var(--s2);
  }
  .backup-row > p.with-icon {
    display: flex;
  }
  .backup-row .with-icon :global(.icon) {
    align-self: flex-start;
    margin-block-start: 2px;
  }
  .problem :global(.icon) {
    color: var(--warning-icon);
  }
  .indent {
    margin-block-start: calc(-1 * var(--s1));
    padding-inline-start: 24px;
  }

  .offer {
    grid-template-columns: auto minmax(0, 1fr);
    gap: var(--s3);
  }
  .offer > :global(.icon) {
    color: var(--accent-text);
    margin-block-start: 1px;
  }
  .offer-text {
    display: grid;
    gap: 2px;
  }
  .offer-actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--s2);
    margin-block-start: var(--s2);
  }

  .attention {
    display: flex;
    align-items: center;
    gap: var(--s3);
    width: 100%;
    min-height: 40px;
    padding: var(--s2) var(--s3);
    border: 1px solid var(--stroke-card);
    border-radius: var(--radius-card);
    background: var(--danger-soft);
    color: var(--text);
    font: inherit;
    text-align: start;
    transition: background-color 100ms ease-out;
  }
  .attention > :global(.icon:first-child) {
    color: var(--danger-text);
  }
  .attention > span {
    flex: 1;
    font-weight: 600;
  }
  @media (hover: hover) {
    .attention:hover {
      background: color-mix(in srgb, var(--danger-soft), var(--text) 5%);
    }
  }

  .section-label {
    margin: var(--s2) var(--s1) var(--s1);
    font-weight: 600;
  }

  .transfers {
    gap: var(--s3);
  }
  .transfers-head {
    display: grid;
    gap: 2px;
  }
  .transfers-head h2 {
    overflow-wrap: anywhere;
  }
  .transfer-list {
    display: grid;
    gap: var(--s4);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  ul.card.transfer-list {
    padding: var(--s3);
  }
  .show-all {
    justify-self: stretch;
    justify-content: space-between;
    margin: calc(-1 * var(--s1)) calc(-1 * var(--s2)) calc(-1 * var(--s2));
    padding-inline: var(--s2);
    color: var(--accent-text);
  }
  .show-all :global(.icon) {
    color: var(--text-2);
  }
  .transfers-panel {
    display: grid;
    gap: var(--s1);
  }
  .section-head.top {
    align-items: start;
  }
  .panel-title {
    display: grid;
    min-width: 0;
  }
  .panel-note {
    padding-inline: var(--s1);
  }
  .section-head {
    display: flex;
    align-items: center;
    gap: var(--s1);
    margin-block-end: var(--s1);
  }
  .icon-only {
    min-width: 32px;
    padding: 0;
  }
  .empty {
    padding: var(--s4) var(--s1);
  }

  .activity,
  .issue-list {
    display: grid;
    gap: 2px;
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .issue-list {
    gap: var(--s2);
  }
  .activity-row {
    display: flex;
    align-items: center;
    gap: var(--s3);
    width: 100%;
    min-height: 44px;
    padding: var(--s1) var(--s2);
    border: 0;
    border-radius: var(--radius-control);
    background: transparent;
    color: inherit;
    font: inherit;
    text-align: start;
    transition: background-color 100ms ease-out;
  }
  @media (hover: hover) {
    .activity-row:hover {
      background: var(--fill-hover);
    }
  }
  .activity-row:active {
    background: var(--fill-press);
  }
  .activity-row :global(.kind) {
    color: var(--text-2);
  }
  .activity-row :global(.kind.conflict),
  .activity-row :global(.kind.failed) {
    color: var(--warning-icon);
  }
  .activity-text {
    display: grid;
    min-width: 0;
  }

  .issue {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr);
    gap: var(--s3);
    padding: var(--s3);
  }
  .issue > :global(.icon) {
    color: var(--danger-text);
    margin-block-start: 1px;
  }
  .issue-text {
    display: grid;
    gap: var(--s1);
  }
  .path {
    overflow-wrap: anywhere;
  }
  .issue-actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--s2);
    margin-block-start: var(--s1);
  }

  .setup {
    display: grid;
    justify-items: center;
    gap: var(--s3);
    padding: var(--s6) var(--s4);
    text-align: center;
  }
  .setup > :global(.icon) {
    color: var(--accent-text);
  }

  .foot {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: var(--s1);
    padding: var(--s2) var(--s3);
    border-block-start: 1px solid var(--stroke-divider);
    background: var(--surface);
  }
  .foot-btn {
    flex-direction: column;
    gap: 2px;
    min-height: 52px;
    padding: var(--s1);
    font-size: var(--text-caption);
    line-height: var(--lh-caption);
  }
  .foot-btn:has(:global(.icon:first-child)) {
    padding-inline-start: var(--s1);
  }
  .foot-btn:disabled {
    opacity: 1;
  }
</style>
