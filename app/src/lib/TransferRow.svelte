<script lang="ts">
  // One upload or download: name, a thin progress bar, what it's doing, and its actions.
  import { basename } from "./format";
  import Icon from "./Icon.svelte";
  import RowMenu from "./RowMenu.svelte";
  import { actionLabel, actionsOf, caption, iconOf, isMoving, resumes, type RowAction } from "./transfers";
  import type { TransferItem } from "./types";

  let {
    item,
    now,
    fileManager,
    onaction,
    full = false,
  }: {
    item: TransferItem;
    now: number;
    fileManager: string;
    onaction: (item: TransferItem, a: RowAction) => void;
    /** Show the whole name, wrapped (the full list); otherwise it's shortened in the middle. */
    full?: boolean;
  } = $props();

  const name = $derived(basename(item.path));
  // Middle ellipsis: the start of the name gives way first, so the end and the extension stay.
  const split = $derived.by(() => {
    const ext = /\.[^.\s]{1,8}$/.exec(name)?.[0] ?? "";
    let keep = Math.min(name.length, ext.length + 6);
    // Start the kept end at a word when one begins close by: "… 4K).mp4", not "… h, 4K).mp4".
    const space = name.slice(name.length - keep, name.length - keep + 4).lastIndexOf(" ");
    if (space >= 0) keep -= space + 1;
    return name.length > 20 ? [name.slice(0, name.length - keep), name.slice(name.length - keep)] : [name, ""];
  });
  const fraction = $derived(
    item.state === "verifying" ? 1 : item.bytes_total > 0 ? Math.min(1, item.bytes_done / item.bytes_total) : 0,
  );
  const actions = $derived(actionsOf(item).map((id) => ({ id, label: actionLabel(id, fileManager) })));
  const verb = $derived(item.direction === "upload" ? "Upload" : "Download");
</script>

<li class="transfer" data-transfer-id={item.id}>
  <Icon name={iconOf(item)} size={16} class="t-icon {isMoving(item.state) ? 'moving' : ''}" />
  <div class="t-text">
    <p class="t-name" class:full title={item.path}>
      <span class="visually-hidden">{verb}: {item.path}</span>
      <span class="start" aria-hidden="true">{split[0]}</span><span class="end" aria-hidden="true">{split[1]}</span>
    </p>
    <div
      class="meter thin"
      class:muted={!isMoving(item.state)}
      class:indeterminate={item.state === "preparing"}
      aria-hidden="true"
    >
      <span style:width="{fraction * 100}%"></span>
    </div>
    <p class="caption secondary numeric t-caption">{caption(item, now)}</p>
    {#if resumes(item)}
      <p class="caption secondary t-hint">
        <Icon name="check" size={12} />Picks up where it left off
      </p>
    {/if}
  </div>
  {#if actions.length}
    <RowMenu label="Actions for {name}" items={actions} onselect={(a) => onaction(item, a)} />
  {/if}
</li>

<style>
  .transfer {
    /* Contains the visually hidden full path, which would otherwise extend the page. */
    position: relative;
    display: grid;
    grid-template-columns: auto minmax(0, 1fr) auto;
    align-items: start;
    gap: var(--s3);
  }
  .transfer :global(.t-icon) {
    margin-block-start: 2px;
    color: var(--text-2);
  }
  .transfer :global(.t-icon.moving) {
    color: var(--accent-text);
  }
  .t-text {
    display: grid;
    gap: var(--s1);
    min-width: 0;
  }
  .t-name {
    display: flex;
    min-width: 0;
    text-wrap: nowrap;
  }
  .t-name.full {
    display: block;
    text-wrap: wrap;
    overflow-wrap: anywhere;
  }
  .t-name.full > span {
    white-space: normal;
  }
  .start {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: pre;
    min-width: 0;
  }
  .end {
    flex: none;
    white-space: pre;
  }
  .t-caption {
    overflow-wrap: anywhere;
  }
  .t-hint {
    display: flex;
    align-items: center;
    gap: var(--s1);
  }
  .t-hint :global(.icon) {
    color: var(--success-text);
  }
  :global(.transfer .more) {
    margin-block-start: -6px;
    margin-inline-end: -6px;
  }

  .thin {
    height: 3px;
    margin-block: 2px;
  }
  .thin.muted > span {
    background: var(--text-2);
  }
  .thin.indeterminate > span {
    width: 0;
  }
  @media (prefers-reduced-motion: no-preference) {
    /* Progress arrives about once a second: glide between updates instead of jumping. */
    .thin > span {
      transition: width 1s linear;
    }
    .thin.indeterminate > span {
      width: 30% !important;
      animation: sweep 1.4s cubic-bezier(0.2, 0, 0, 1) infinite;
    }
  }
  @keyframes sweep {
    from {
      translate: -100% 0;
    }
    to {
      translate: 340% 0;
    }
  }
  @media (forced-colors: active) {
    .thin.muted > span {
      background: GrayText;
    }
  }
</style>
