<script lang="ts">
  import { onMount, tick } from "svelte";
  import { api, env, errorOf, startDragging } from "../lib/api";
  import { connect, drive, refreshIssues } from "../lib/drive.svelte";
  import { basename, bytes, count, dirname, plural, relative } from "../lib/format";
  import Icon from "../lib/Icon.svelte";
  import StateBadge from "../lib/StateBadge.svelte";
  import UpdateButton from "../lib/UpdateButton.svelte";
  import { ACTION_LABEL, ACTIVITY, ISSUE_TITLE, stateVisual } from "../lib/status";
  import type { Issue, IssueAction } from "../lib/types";

  const flyout = env().platform === "windows";

  let panel = $state<"home" | "issues">("home");
  let busy = $state<string | null>(null);
  let actionError = $state<string | null>(null);
  let issuesHeading = $state<HTMLHeadingElement>();
  let issuesButton = $state<HTMLButtonElement>();

  const status = $derived(drive.status);
  const connected = $derived(drive.connection.state === "connected");
  const visual = $derived(stateVisual(status, drive.connection));
  const setUp = $derived(connected && status !== null && status.state !== "not_set_up");
  const paused = $derived(status?.state === "paused");
  const canPause = $derived(setUp && status !== null && !["signed_out", "not_set_up"].includes(status.state));
  const massDelete = $derived(drive.issues.find((i) => i.kind === "mass_delete") ?? null);
  const issueCount = $derived(status?.issues ?? drive.issues.length);
  const transferring = $derived(status !== null && status.uploading + status.downloading > 0);

  onMount(() => {
    connect({ activity: true, issues: true, settings: true });
    if (flyout) {
      const onKey = (e: KeyboardEvent) => {
        // An open dialog handles Escape itself.
        if (e.key === "Escape" && !e.defaultPrevented && !document.querySelector("dialog[open]")) {
          if (panel === "issues") showHome();
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

  async function showHome() {
    panel = "home";
    await tick();
    issuesButton?.focus();
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
    {#if panel === "issues"}
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
                {#if issue.path}
                  <p class="caption secondary path selectable">{issue.path}</p>
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

        {#if transferring || status.pending > 0}
          <section class="card block" aria-label="Transfers">
            {#if transferring}
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
          <h2 id="recent-title" class="caption secondary section-label">Recent activity</h2>
          {#if drive.activity.length === 0}
            <p class="secondary empty">Changes to your Drive folder show up here.</p>
          {:else}
            <ul class="activity" role="list">
              {#each drive.activity as item (item.id)}
                {@const kind = ACTIVITY[item.kind]}
                {@const dir = dirname(item.path)}
                <li>
                  <button
                    class="activity-row"
                    title="Show {item.path} in {flyout ? 'File Explorer' : 'the file manager'}"
                    onclick={() => run(`reveal:${item.id}`, () => api.revealItem(item.path))}
                  >
                    <Icon name={kind.icon} size={18} class="kind {item.kind}" />
                    <span class="activity-text">
                      <span class="truncate name">{basename(item.path)}</span>
                      <span class="truncate caption secondary">
                        {kind.verb}{dir ? ` in ${dir}` : ""} · {relative(item.at_ms, drive.now)}
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
