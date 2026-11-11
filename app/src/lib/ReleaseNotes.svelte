<script lang="ts">
  // The changes in each newer release: "0.1.2 · 2 days ago" and its notes.
  import { drive } from "./drive.svelte";
  import { relative } from "./format";
  import type { Release } from "./types";

  let { releases }: { releases: Release[] } = $props();

  function when(date: string | null): string | null {
    const ms = date ? Date.parse(date) : NaN;
    return Number.isNaN(ms) ? null : relative(ms, drive.now);
  }
</script>

<ol class="releases" role="list">
  {#each releases as r (r.version)}
    {@const date = when(r.date)}
    <li class="release">
      <h3 class="release-head">
        <span class="body-strong numeric">{r.version}</span>
        {#if date}<span class="caption secondary">· {date}</span>{/if}
      </h3>
      {#if r.notes.length}
        <ul class="notes">
          {#each r.notes as note}
            <li>{note}</li>
          {/each}
        </ul>
      {/if}
    </li>
  {/each}
</ol>

<style>
  .releases {
    display: grid;
    gap: var(--s3);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .release {
    display: grid;
    gap: var(--s1);
  }
  .release-head {
    display: flex;
    align-items: baseline;
    flex-wrap: wrap;
    gap: var(--s1);
  }
  .notes {
    display: grid;
    gap: 2px;
    margin: 0;
    padding-inline-start: var(--s5);
  }
  .notes li {
    text-wrap: pretty;
  }
  .notes li::marker {
    color: var(--text-2);
  }
</style>
