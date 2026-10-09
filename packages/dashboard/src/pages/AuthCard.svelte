<script lang="ts">
  import type { Snippet } from "svelte";

  let { title, subtitle = "", children }: { title: string; subtitle?: string; children: Snippet } = $props();
</script>

<div class="wrap">
  <div class="card box">
    <div class="brand"><span class="logo">F</span> ferrix</div>
    <h1>{title}</h1>
    {#if subtitle}<p class="muted sub">{subtitle}</p>{/if}
    {@render children()}
  </div>
</div>

<style>
  .wrap {
    min-height: 100%;
    display: grid;
    place-items: center;
    padding: 24px;
  }
  .box {
    width: 100%;
    max-width: 380px;
    padding: 28px;
    box-shadow: var(--shadow);
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 8px;
    font-weight: 700;
    margin-bottom: 8px;
  }
  .logo {
    display: inline-grid;
    place-items: center;
    width: 24px;
    height: 24px;
    border-radius: 6px;
    background: var(--accent);
    color: white;
    font-size: 13px;
  }
  .sub {
    margin: 0 0 6px;
    font-size: 13px;
  }
  :global(.box form) {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  :global(.box label) {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: 13px;
    color: var(--muted);
  }
  :global(.box input) {
    padding: 8px 10px;
  }
  :global(.box button.primary) {
    justify-content: center;
    padding: 8px;
    margin-top: 4px;
  }
</style>
