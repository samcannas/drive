<script lang="ts" module>
  let seq = 0;
</script>

<script lang="ts" generics="T extends string">
  // An overflow (…) menu button, following the ARIA menu button pattern: Enter, Space or
  // Down opens on the first item, Up on the last; arrows, Home and End move; Escape closes and
  // returns focus to the button. The menu is fixed-positioned so a scrolling list never clips it.
  import { tick } from "svelte";
  import Icon from "./Icon.svelte";

  let {
    label,
    items,
    onselect,
  }: { label: string; items: { id: T; label: string }[]; onselect: (id: T) => void } = $props();

  const id = `row-menu-${++seq}`;
  let open = $state(false);
  let button = $state<HTMLButtonElement>();
  let menu = $state<HTMLDivElement>();
  let top = $state(0);
  let left = $state(0);

  function entries(): HTMLButtonElement[] {
    return menu ? [...menu.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')] : [];
  }

  async function show(focus: "first" | "last") {
    open = true;
    await tick();
    place();
    const els = entries();
    (focus === "first" ? els[0] : els[els.length - 1])?.focus();
    window.addEventListener("pointerdown", onOutside, true);
    window.addEventListener("resize", onDismiss);
    window.addEventListener("blur", onDismiss);
    document.addEventListener("scroll", onDismiss, true);
  }

  function hide(restoreFocus: boolean) {
    if (!open) return;
    open = false;
    window.removeEventListener("pointerdown", onOutside, true);
    window.removeEventListener("resize", onDismiss);
    window.removeEventListener("blur", onDismiss);
    document.removeEventListener("scroll", onDismiss, true);
    if (restoreFocus) button?.focus();
  }

  const onDismiss = () => hide(false);
  // A row can finish while its menu is open: drop the window listeners with it.
  $effect(() => () => hide(false));
  function onOutside(e: PointerEvent) {
    const t = e.target as Node;
    if (!menu?.contains(t) && !button?.contains(t)) hide(false);
  }

  // Below the button, end-aligned with it; above when there's no room below.
  function place() {
    if (!button || !menu) return;
    const b = button.getBoundingClientRect();
    const m = menu.getBoundingClientRect();
    const gap = 4;
    const edge = 8;
    top = b.bottom + gap + m.height > innerHeight - edge ? Math.max(edge, b.top - gap - m.height) : b.bottom + gap;
    left = Math.min(Math.max(edge, b.right - m.width), innerWidth - m.width - edge);
  }

  function onButtonKey(e: KeyboardEvent) {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      show(e.key === "ArrowDown" ? "first" : "last");
    }
  }

  function onMenuKey(e: KeyboardEvent) {
    const els = entries();
    const i = els.indexOf(document.activeElement as HTMLButtonElement);
    const go = (n: number) => {
      e.preventDefault();
      els[(n + els.length) % els.length]?.focus();
    };
    switch (e.key) {
      case "ArrowDown":
        return go(i + 1);
      case "ArrowUp":
        return go(i - 1);
      case "Home":
        return go(0);
      case "End":
        return go(els.length - 1);
      case "Escape":
        // Also keeps the popover's own Escape handler from closing the window.
        e.preventDefault();
        e.stopPropagation();
        return hide(true);
      case "Tab":
        return hide(false);
    }
  }

  function onFocusOut(e: FocusEvent) {
    const next = e.relatedTarget as Node | null;
    if (next && !menu?.contains(next) && next !== button) hide(false);
  }

  function choose(item: T) {
    hide(true);
    onselect(item);
  }
</script>

<button
  bind:this={button}
  class="btn subtle icon-only more"
  aria-label={label}
  title={label}
  aria-haspopup="menu"
  aria-expanded={open}
  aria-controls={open ? id : undefined}
  onclick={() => (open ? hide(true) : show("first"))}
  onkeydown={onButtonKey}
>
  <Icon name="more" size={16} />
</button>
{#if open}
  <!-- svelte-ignore a11y_interactive_supports_focus -->
  <div
    bind:this={menu}
    {id}
    class="menu"
    role="menu"
    aria-label={label}
    style:top="{top}px"
    style:left="{left}px"
    onkeydown={onMenuKey}
    onfocusout={onFocusOut}
  >
    {#each items as item (item.id)}
      <button class="menu-item" role="menuitem" tabindex="-1" onclick={() => choose(item.id)}>{item.label}</button>
    {/each}
  </div>
{/if}

<style>
  .more {
    min-width: 32px;
    padding: 0;
    color: var(--text-2);
  }
  .more[aria-expanded="true"] {
    background: var(--fill-press);
    color: var(--text);
  }
  .menu {
    position: fixed;
    z-index: 10;
    display: grid;
    min-width: 168px;
    max-width: calc(100vw - 16px);
    padding: var(--s1);
    /* Outer radius = item radius (4) + padding (4). */
    border: 1px solid var(--stroke-card);
    border-radius: var(--radius-card);
    background: var(--bg-overlay);
    box-shadow: 0 8px 16px rgb(0 0 0 / 0.14), 0 0 2px rgb(0 0 0 / 0.12);
    overscroll-behavior: contain;
  }
  .menu-item {
    display: flex;
    align-items: center;
    min-height: 32px;
    padding: 0 var(--s3);
    border: 0;
    border-radius: var(--radius-control);
    background: transparent;
    color: var(--text);
    font: inherit;
    text-align: start;
    white-space: nowrap;
    transition: background-color 100ms ease-out;
  }
  @media (hover: hover) {
    .menu-item:hover {
      background: var(--fill-hover);
    }
  }
  .menu-item:active {
    background: var(--fill-press);
  }
  .menu-item:focus-visible {
    outline-offset: -2px;
  }
  @media (forced-colors: active) {
    .menu {
      border-color: CanvasText;
    }
  }
</style>
