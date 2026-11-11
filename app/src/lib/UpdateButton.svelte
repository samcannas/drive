<script lang="ts">
  // Popover header: an update button while an update is on offer. Hovering or focusing it
  // shows what's new; clicking asks before Drive updates and restarts.
  import { onMount } from "svelte";
  import { fade } from "svelte/transition";
  import { api, env, errorOf } from "./api";
  import { drive, updateVersion } from "./drive.svelte";
  import { bytes } from "./format";
  import Icon from "./Icon.svelte";
  import ReleaseNotes from "./ReleaseNotes.svelte";
  import type { Release } from "./types";

  const windows = env().platform === "windows";

  let current = $state<string | null>(null);
  let wrapper = $state<HTMLDivElement>();
  let button = $state<HTMLButtonElement>();
  let card = $state<HTMLDivElement>();
  let dialog = $state<HTMLDialogElement>();
  let hovered = $state(false);
  let focused = $state(false);
  /** Escape hides the card until the pointer or focus leaves. */
  let dismissed = $state(false);
  let dialogOpen = $state(false);
  let requested = $state(false);
  let error = $state<string | null>(null);
  let leaveTimer: ReturnType<typeof setTimeout> | undefined;

  const u = $derived(drive.update);
  const version = $derived(updateVersion(u));
  const shown = $derived(version !== null);
  const cardOpen = $derived(shown && (hovered || focused) && !dismissed && !dialogOpen);
  const ready = $derived(u?.state === "ready");
  const busy = $derived(requested || u?.state === "downloading" || u?.state === "installing");
  const shownError = $derived(error ?? (u?.state === "error" ? u.message : null));
  const releases = $derived<Release[]>(
    drive.changelog.length
      ? drive.changelog
      : u && (u.state === "available" || u.state === "ready")
        ? [{ version: u.version, date: u.date, url: null, notes: u.notes }]
        : [],
  );
  const cardTitle = $derived(
    u?.state === "ready"
      ? "Update ready to install"
      : u?.state === "downloading"
        ? "Downloading update"
        : u?.state === "installing"
          ? "Installing update"
          : "Update available",
  );

  onMount(() => {
    api.appInfo().then((i) => (current = i.version)).catch(() => {});
  });

  // The update went away (the mock "restarted", or a newer check found nothing).
  $effect(() => {
    if (!shown && dialog?.open) dialog.close();
  });

  function enter(e: PointerEvent) {
    if (e.pointerType === "touch") return;
    clearTimeout(leaveTimer);
    hovered = true;
  }
  function leave(e: PointerEvent) {
    if (e.pointerType === "touch") return;
    // A short grace period lets the pointer cross the gap between the button and the card.
    leaveTimer = setTimeout(() => {
      hovered = false;
      if (!focused) dismissed = false;
    }, 150);
  }
  function focusIn(e: FocusEvent) {
    if ((e.target as HTMLElement).matches(":focus-visible")) focused = true;
  }
  function focusOut(e: FocusEvent) {
    if (wrapper?.contains(e.relatedTarget as Node | null)) return;
    focused = false;
    if (!hovered) dismissed = false;
  }
  function key(e: KeyboardEvent) {
    if (e.key === "Escape" && cardOpen) {
      // Hide the card only; the popover stays open. Focus inside the card goes back to the button.
      e.preventDefault();
      if (card?.contains(document.activeElement)) button?.focus();
      dismissed = true;
    }
  }

  function openDialog() {
    error = null;
    dialogOpen = true;
    dialog?.showModal();
  }

  async function install() {
    if (busy) return;
    requested = true;
    error = null;
    try {
      // Drive restarts once the update is installed; this only returns in the mock or on failure.
      await api.installUpdate();
    } catch (e) {
      error = errorOf(e).message;
    } finally {
      requested = false;
    }
  }
</script>

{#if shown}
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div
    class="update"
    bind:this={wrapper}
    onpointerenter={enter}
    onpointerleave={leave}
    onfocusin={focusIn}
    onfocusout={focusOut}
    onkeydown={key}
  >
    <button
      class="btn subtle icon-only update-btn"
      bind:this={button}
      aria-haspopup="dialog"
      aria-label={ready ? `Restart to update Drive to ${version}` : `Update Drive to ${version}`}
      onclick={openDialog}
    >
      <Icon name="arrow-up-circle" size={16} />
      <span class="dot" aria-hidden="true"></span>
    </button>

    {#if cardOpen}
      <!-- Focusable so the notes can be scrolled from the keyboard (Tab from the button). -->
      <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
      <div
        class="update-card"
        bind:this={card}
        data-no-drag
        role="region"
        aria-labelledby="update-card-title"
        tabindex="0"
        in:fade={{ duration: 100 }}
      >
        <div class="card-head">
          <h2 id="update-card-title" class="body-strong">{cardTitle}</h2>
          {#if current}
            <p class="caption secondary numeric">You have version {current}</p>
          {/if}
        </div>
        {#if releases.length}
          <ReleaseNotes {releases} />
        {/if}
      </div>
    {/if}
  </div>
{/if}

<dialog
  bind:this={dialog}
  class="update-dialog"
  aria-labelledby="update-title"
  aria-describedby="update-body"
  onclose={() => (dialogOpen = false)}
>
  <div class="dialog-body">
    <h2 id="update-title" class="subtitle">Update Drive to {version}?</h2>
    <div id="update-body" class="dialog-text">
      <p>Drive restarts to finish updating. Syncing resumes on its own once it's back.</p>
      {#if windows}
        <p class="caption secondary">A small progress window appears while the update installs.</p>
      {/if}
    </div>
    <div class="progress">
      <!-- Only the phase is live; the byte count would be re-announced several times a second. -->
      <p class="caption numeric">
        <span role="status">{u?.state === "downloading"
            ? "Downloading…"
            : u?.state === "installing"
              ? "Installing… Drive restarts in a moment."
              : requested
                ? "Getting the update ready…"
                : ""}</span>{#if u?.state === "downloading" && u.total_bytes}
          {` ${bytes(u.done_bytes)} of ${bytes(u.total_bytes)}`}
        {/if}
      </p>
      {#if u?.state === "downloading"}
        {@const total = u.total_bytes ?? 0}
        {#if total > 0}
          <div
            class="meter"
            role="progressbar"
            aria-label="Download progress"
            aria-valuemin="0"
            aria-valuemax="100"
            aria-valuenow={Math.round((Math.min(u.done_bytes, total) / total) * 100)}
          >
            <span style:width="{(Math.min(u.done_bytes, total) / total) * 100}%"></span>
          </div>
        {/if}
      {/if}
    </div>
    {#if shownError && !busy}
      <p class="error-text" role="alert"><Icon name="alert" size={14} />{shownError}</p>
    {/if}
  </div>
  <div class="dialog-actions">
    <button class="btn primary" aria-disabled={busy} onclick={install}>
      {busy ? "Updating…" : shownError ? "Try again" : "Update"}
    </button>
    <!-- svelte-ignore a11y_autofocus -->
    <button class="btn" onclick={() => dialog?.close()} autofocus>{busy ? "Close" : "Cancel"}</button>
  </div>
</dialog>

<style>
  .update {
    align-self: flex-start;
    margin-top: calc(-1 * var(--s1));
    flex: none;
  }
  .update-btn {
    position: relative;
    min-width: 32px;
    padding: 0;
    color: var(--accent-text);
  }
  .dot {
    position: absolute;
    /* In the button's corner, clear of the icon's ring. */
    inset-block-start: 3px;
    inset-inline-end: 3px;
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--accent);
    pointer-events: none;
  }

  /* Anchored to the header (position: relative in Popover.svelte), below the buttons. */
  .update-card {
    position: absolute;
    z-index: 10;
    inset-block-start: calc(100% - var(--s1));
    inset-inline: var(--s3);
    display: grid;
    gap: var(--s3);
    max-height: min(360px, calc(100vh - 96px));
    padding: var(--s3) var(--s4) var(--s4);
    overflow-y: auto;
    overscroll-behavior: contain;
    scrollbar-width: thin;
    border: 1px solid var(--stroke-card);
    border-radius: var(--radius-card);
    background: var(--bg-overlay);
    box-shadow: var(--shadow-dialog);
    cursor: default;
    user-select: text;
  }
  .card-head {
    display: grid;
    gap: 2px;
  }

  .dialog-body {
    display: grid;
    gap: var(--s3);
    padding: var(--s6) var(--s6) var(--s4);
  }
  .dialog-text {
    display: grid;
    gap: var(--s2);
  }
  .progress {
    display: grid;
    gap: var(--s2);
  }
  /* Stays rendered (an empty live region announces reliably); takes no room while idle. */
  .progress:not(:has(span:not(:empty))) {
    margin-block-start: calc(-1 * var(--s3));
  }
  .dialog-actions {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: var(--s2);
    padding: var(--s4) var(--s6) var(--s6);
  }
</style>
