<script lang="ts" module>
  /** Outline icons on a 24px grid, drawn with the same 1.75px stroke so the sidebar reads as one set. */
  const ICONS = {
    overview: '<rect x="3.5" y="3.5" width="7" height="9" rx="1.5"/><rect x="13.5" y="3.5" width="7" height="5" rx="1.5"/><rect x="13.5" y="11.5" width="7" height="9" rx="1.5"/><rect x="3.5" y="15.5" width="7" height="5" rx="1.5"/>',
    data: '<ellipse cx="12" cy="5.5" rx="7.5" ry="2.5"/><path d="M4.5 5.5v13c0 1.4 3.4 2.5 7.5 2.5s7.5-1.1 7.5-2.5v-13"/><path d="M4.5 12c0 1.4 3.4 2.5 7.5 2.5s7.5-1.1 7.5-2.5"/>',
    functions: '<path d="m8.5 7-5 5 5 5"/><path d="m15.5 7 5 5-5 5"/><path d="m13.5 4.5-3 15"/>',
    logs: '<path d="M8.5 6.5h12M8.5 12h12M8.5 17.5h12"/><path d="M3.5 6.5h.01M3.5 12h.01M3.5 17.5h.01" stroke-width="2.5"/>',
    users: '<circle cx="9" cy="8" r="3.5"/><path d="M2.5 20c0-3.6 2.9-6 6.5-6s6.5 2.4 6.5 6"/><path d="M16 4.6a3.5 3.5 0 0 1 0 6.8"/><path d="M18.5 14.4c1.8.8 3 2.8 3 5.6"/>',
    files: '<path d="M14 3.5H7a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2v-10z"/><path d="M14 3.5v5h5"/>',
    scheduler: '<circle cx="12" cy="12" r="8.5"/><path d="M12 7.5V12l3 2"/>',
    workflows: '<rect x="3.5" y="3.5" width="6" height="6" rx="1.5"/><rect x="14.5" y="14.5" width="6" height="6" rx="1.5"/><path d="M6.5 9.5v3a2 2 0 0 0 2 2h6"/>',
    migrations: '<path d="M4 8h15.5"/><path d="m16 4.5 3.5 3.5-3.5 3.5"/><path d="M20 16H4.5"/><path d="m8 12.5-3.5 3.5 3.5 3.5"/>',
    backups: '<rect x="3" y="4" width="18" height="5" rx="1.5"/><path d="M5 9v9a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V9"/><path d="M10 13h4"/>',
    analytics: '<path d="M4 20h16"/><path d="M7 16.5v-5M12 16.5V6M17 16.5v-8"/>',
    environment: '<path d="M8 3.5H7a2 2 0 0 0-2 2v4a2.5 2.5 0 0 1-2 2.5 2.5 2.5 0 0 1 2 2.5v4a2 2 0 0 0 2 2h1"/><path d="M16 3.5h1a2 2 0 0 1 2 2v4a2.5 2.5 0 0 0 2 2.5 2.5 2.5 0 0 0-2 2.5v4a2 2 0 0 1-2 2h-1"/>',
    settings: '<path d="M4 6.5h9M17 6.5h3M4 12h3M11 12h9M4 17.5h11M19 17.5h1"/><circle cx="15" cy="6.5" r="2"/><circle cx="9" cy="12" r="2"/><circle cx="17" cy="17.5" r="2"/>',
    audit: '<path d="M3.5 12a8.5 8.5 0 1 0 2.5-6"/><path d="M3.5 4v4.5H8"/><path d="M12 8v4.5l3 1.5"/>',
    account: '<path d="M12 3 4.5 6v5.5c0 4.5 3.2 8.2 7.5 9.5 4.3-1.3 7.5-5 7.5-9.5V6z"/><path d="m9 12 2 2 4-4"/>',
    sun: '<circle cx="12" cy="12" r="4"/><path d="M12 2.5v2M12 19.5v2M4.6 4.6l1.4 1.4M18 18l1.4 1.4M2.5 12h2M19.5 12h2M4.6 19.4 6 18M18 6l1.4-1.4"/>',
    moon: '<path d="M20 14.5A8 8 0 1 1 9.5 4a6.5 6.5 0 0 0 10.5 10.5z"/>',
    system: '<rect x="3" y="4.5" width="18" height="12" rx="2"/><path d="M8.5 20h7M12 16.5V20"/>',
    search: '<circle cx="11" cy="11" r="6.5"/><path d="m20 20-4.4-4.4"/>',
    close: '<path d="M6 6l12 12M18 6 6 18"/>',
    menu: '<path d="M4 6.5h16M4 12h16M4 17.5h16"/>',
    download: '<path d="M12 4v11"/><path d="m7.5 11 4.5 4.5 4.5-4.5"/><path d="M5 19.5h14"/>',
  } as const;

  export type IconName = keyof typeof ICONS;
</script>

<script lang="ts">
  let { name, size = 16 }: { name: IconName; size?: number } = $props();
</script>

<svg
  class="icon"
  width={size}
  height={size}
  viewBox="0 0 24 24"
  fill="none"
  stroke="currentColor"
  stroke-width="1.75"
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
  focusable="false">{@html ICONS[name]}</svg
>

<style>
  .icon {
    flex: none;
    display: block;
  }
</style>
