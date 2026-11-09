<script lang="ts">
  import { withParams } from "../lib/format.js";
  import Icon from "./Icon.svelte";

  /**
   * A 36px list thumbnail of a stored image (`src` is a `thumb=…` URL) that recovers from a failed load. Generating a
   * thumbnail is rate limited per client IP (429 with `Retry-After`), and a page of images nobody has viewed yet can
   * ask for more at once than the burst allows. An `<img>` can't see the status, so a failed load is retried a few
   * times with jittered backoff (adding an `r` parameter the server ignores, so the browser asks again), then the
   * file icon is shown. Retries are few because a broken image fails the same way every time.
   */
  let { src }: { src: string } = $props();

  const RETRIES = 4;
  /** Failed loads so far, and the source they belong to: a new source (e.g. a re-signed URL) starts at 0. */
  let tries = $state({ src: "", n: 0 });
  const attempt = $derived(tries.src === src ? tries.n : 0);
  const failed = $derived(attempt > RETRIES);
  const url = $derived(attempt === 0 ? src : withParams(src, { r: attempt }));
  let timer: ReturnType<typeof setTimeout> | undefined;
  $effect(() => () => clearTimeout(timer));

  function retry() {
    clearTimeout(timer);
    const failedSrc = src;
    const n = attempt + 1;
    if (n > RETRIES) {
      tries = { src: failedSrc, n };
      return;
    }
    const delay = 2000 * 2 ** attempt * (0.5 + Math.random());
    timer = setTimeout(() => (tries = { src: failedSrc, n }), delay);
  }
</script>

{#if failed}
  <span class="icon" aria-hidden="true"><Icon name="files" size={18} /></span>
{:else}
  <img src={url} alt="" loading="lazy" width="36" height="36" data-testid="thumb" onerror={retry} />
{/if}

<style>
  img {
    display: block;
    width: 36px;
    height: 36px;
    object-fit: cover;
    border-radius: 4px;
    background: var(--panel-2);
  }
  .icon {
    display: inline-grid;
    place-items: center;
    width: 36px;
    height: 36px;
    border-radius: 4px;
    background: var(--panel-2);
    color: var(--faint);
  }
</style>
