<script lang="ts" module>
  // 20×20 outline icons drawn with currentColor. Static markup only.
  const CLOUD = "M5.5 15.5h9a3.5 3.5 0 0 0 .6-6.95 5 5 0 0 0-9.6-.3A3.63 3.63 0 0 0 5.5 15.5Z";
  const FOLDER =
    "M3 6.5A1.5 1.5 0 0 1 4.5 5h3.4l1.6 1.6h6A1.5 1.5 0 0 1 17 8.1v6.4a1.5 1.5 0 0 1-1.5 1.5h-11A1.5 1.5 0 0 1 3 14.5Z";
  const COPY =
    "M7 7.5A1.5 1.5 0 0 1 8.5 6h6A1.5 1.5 0 0 1 16 7.5v7a1.5 1.5 0 0 1-1.5 1.5h-6A1.5 1.5 0 0 1 7 14.5ZM13 6v-.5A1.5 1.5 0 0 0 11.5 4h-6A1.5 1.5 0 0 0 4 5.5v7A1.5 1.5 0 0 0 5.5 14H7";
  const GEAR =
    "M15.52 7.92 17.29 8.22v3.56l-1.77.3-.15.35 1.04 1.46-2.52 2.52-1.46-1.04-.35.15-.3 1.77H8.22l-.3-1.77-.35-.15-1.46 1.04-2.52-2.52 1.04-1.46-.15-.35-1.77-.3V8.22l1.77-.3.15-.35-1.04-1.46 2.52-2.52 1.46 1.04.35-.15.3-1.77h3.56l.3 1.77.35.15 1.46-1.04 2.52 2.52-1.04 1.46Z";

  const p = (d: string) => `<path d="${d}"/>`;

  export const ICONS = {
    cloud: p(CLOUD),
    "cloud-check": p(CLOUD) + p("M8 11.25l1.5 1.5 3-3"),
    "cloud-off": p(CLOUD) + p("M3 3l14 14"),
    sync: p("M15.5 8.2A6 6 0 0 0 4.7 7M4.5 11.8A6 6 0 0 0 15.3 13M15.5 4.2v4h-4M4.5 15.8v-4h4"),
    pause: p("M7.5 5v10M12.5 5v10"),
    play: p("M7 4.9v10.2a.6.6 0 0 0 .92.5l7.7-5.1a.6.6 0 0 0 0-1L7.92 4.4A.6.6 0 0 0 7 4.9Z"),
    folder: p(FOLDER),
    "folder-plus": p(FOLDER) + p("M10 9v4.5M7.75 11.25h4.5"),
    gear: p(GEAR) + '<circle cx="10" cy="10" r="2.5"/>',
    upload: p("M10 13.5V4M6 8l4-4 4 4M4 16h12"),
    download: p("M10 4v9.5M6 9.5l4 4 4-4M4 16h12"),
    move: p("M4 10h11.5M11.5 6l4 4-4 4"),
    trash: p("M4 6h12M8 6V4.5h4V6M5.5 6l.7 9.6a1 1 0 0 0 1 .9h5.6a1 1 0 0 0 1-.9L14.5 6"),
    copy: p(COPY),
    alert: '<circle cx="10" cy="10" r="7"/>' + p("M10 6.5v4.25M10 13.5v.01"),
    warning: p("M8.7 4.2a1.5 1.5 0 0 1 2.6 0l5.6 9.7a1.5 1.5 0 0 1-1.3 2.25H4.4a1.5 1.5 0 0 1-1.3-2.25ZM10 8v3.5M10 13.75v.01"),
    info: '<circle cx="10" cy="10" r="7"/>' + p("M10 9v4.5M10 6.5v.01"),
    check: p("M5 10.5l3.25 3.25L15 7"),
    "check-circle": '<circle cx="10" cy="10" r="7"/>' + p("M7 10.25l2 2 4-4"),
    "check-circle-filled":
      '<circle cx="10" cy="10" r="7.5" fill="currentColor" stroke="none"/>' +
      '<path d="M7 10.25l2 2 4-4" stroke="var(--icon-knockout, #fff)"/>',
    x: p("M5.5 5.5l9 9M14.5 5.5l-9 9"),
    lock: p("M6.5 9V7a3.5 3.5 0 0 1 7 0v2") + '<rect x="4.5" y="9" width="11" height="8" rx="1.75"/>',
    user: '<circle cx="10" cy="7" r="3"/>' + p("M4.5 16.5c.8-2.7 3-4.25 5.5-4.25s4.7 1.55 5.5 4.25"),
    external: p("M11.5 4H16v4.5M16 4l-6.5 6.5M14 11.5v3a1.5 1.5 0 0 1-1.5 1.5h-7A1.5 1.5 0 0 1 4 14.5v-7A1.5 1.5 0 0 1 5.5 6h3"),
    "chevron-left": p("M12 5l-5 5 5 5"),
    "chevron-right": p("M8 5l5 5-5 5"),
    retry: p("M15.5 10a5.5 5.5 0 1 1-1.7-4M15.5 4v3.5H12"),
    shield: p("M10 3 4.5 5.25v4.3c0 3.4 2.3 5.95 5.5 6.95 3.2-1 5.5-3.55 5.5-6.95v-4.3Z"),
    storage: p("M3 11.5 5 5.5h10l2 6v3a1.5 1.5 0 0 1-1.5 1.5h-11A1.5 1.5 0 0 1 3 14.5ZM3 11.5h14M13.5 13.75h.01"),
    network: p("M7 16V4.5M4 7.5l3-3 3 3M13 4v11.5M10 12.5l3 3 3-3"),
    eye: p("M2.75 10S5.5 4.75 10 4.75 17.25 10 17.25 10 14.5 15.25 10 15.25 2.75 10 2.75 10Z") + '<circle cx="10" cy="10" r="2.25"/>',
    "eye-off": p("M2.75 10S5.5 4.75 10 4.75 17.25 10 17.25 10 14.5 15.25 10 15.25 2.75 10 2.75 10Z") + p("M3.5 3.5l13 13"),
    key: '<circle cx="7" cy="12.5" r="3"/>' + p("M9.2 10.3 15.5 4M13.25 6.25l1.75 1.75M11.75 7.75l1.25 1.25"),
  } as const;

  export type IconName = keyof typeof ICONS;
</script>

<script lang="ts">
  let { name, size = 20, class: cls = "" }: { name: IconName; size?: number; class?: string } = $props();
</script>

<svg
  class="icon {cls}"
  width={size}
  height={size}
  viewBox="0 0 20 20"
  fill="none"
  stroke="currentColor"
  stroke-width="1.5"
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
  focusable="false"
>
  {@html ICONS[name]}
</svg>
