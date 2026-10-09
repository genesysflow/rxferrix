<script lang="ts">
  import { useQuery } from "@ferrix/svelte";
  import { api, type Id } from "./lib/ferrix";

  // The serving URL comes from a query (ctx.storage.get_url), so it disappears live when the file is deleted.
  let { image, alt }: { image: Id<"_storage">; alt: string } = $props();
  const url = useQuery(api.todos.image_url, () => ({ image }));
</script>

{#if url.data}
  <a href={url.data} target="_blank" rel="noreferrer"><img class="thumb" src={url.data} {alt} /></a>
{/if}
