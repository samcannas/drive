<script lang="ts">
  import Icon from "./Icon.svelte";
  import type { StateVisual } from "./status";

  let { visual, size = 40 }: { visual: StateVisual; size?: number } = $props();
</script>

<span class="badge {visual.tone}" class:active={visual.active} style:--size="{size}px" aria-hidden="true">
  <Icon name={visual.icon} size={Math.round(size * 0.55)} />
</span>

<style>
  .badge {
    display: inline-grid;
    place-items: center;
    flex: none;
    width: var(--size);
    height: var(--size);
    border-radius: 50%;
  }
  .success {
    background: var(--success-soft);
    color: var(--success-text);
  }
  .accent {
    background: var(--accent-soft);
    color: var(--accent-text);
  }
  .neutral {
    background: var(--neutral-soft);
    color: var(--text-2);
  }
  .warning {
    background: var(--warning-soft);
    color: var(--warning-icon);
  }
  .danger {
    background: var(--danger-soft);
    color: var(--danger-text);
  }
  /* Slow turn only while bytes move; the headline says the same thing in words. */
  @media (prefers-reduced-motion: no-preference) {
    .active :global(.icon) {
      animation: turn 2.4s linear infinite;
    }
  }
  @keyframes turn {
    to {
      rotate: 360deg;
    }
  }
</style>
