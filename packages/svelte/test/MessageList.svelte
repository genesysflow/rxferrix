<script lang="ts">
  import { usePaginatedQuery } from "../src/lib/index.js";
  import { api } from "./api.js";

  let { channel }: { channel: string | null } = $props();

  const messages = usePaginatedQuery(api.messages.list, () => (channel === null ? "skip" : { channel }), {
    initialNumItems: 2,
  });
</script>

<p class="status">
  {messages.status}
  {messages.isLoading ? "loading" : "idle"}{messages.error ? ` error: ${messages.error.message}` : ""}
</p>
<ul>
  {#each messages.results as m (m._id)}
    <li>{m.body}</li>
  {/each}
</ul>
<button onclick={() => messages.loadMore(3)}>more</button>
