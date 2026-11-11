<script lang="ts">
  import { onMount } from "svelte";
  import { env, followWindowTheme } from "./lib/api";
  import Popover from "./views/Popover.svelte";
  import Settings from "./views/Settings.svelte";
  import Onboarding from "./views/Onboarding.svelte";

  const { view, material, platform } = env();

  const root = document.documentElement;
  root.dataset.material = material;
  root.dataset.platform = platform;
  root.dataset.view = view;
  document.title = view === "settings" ? "Drive settings" : view === "onboarding" ? "Set up Drive" : "Drive";

  onMount(() => {
    followWindowTheme((theme) => {
      if (theme) root.dataset.theme = theme;
      else delete root.dataset.theme;
    });
  });
</script>

{#if view === "settings"}
  <Settings />
{:else if view === "onboarding"}
  <Onboarding />
{:else}
  <Popover />
{/if}
