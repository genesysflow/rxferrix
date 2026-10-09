<script lang="ts">
  import { useMutation, useQuery } from "../src/lib/index.js";
  import { api, type Todo } from "./api.js";

  let { onResult }: { onResult?: (p: Promise<unknown>) => void } = $props();

  const todos = useQuery(api.todos.list);
  const add = useMutation(api.todos.add).withOptimisticUpdate((store, args) => {
    const cur = store.getQuery(api.todos.list, {});
    if (cur) store.setQuery(api.todos.list, {}, [...cur, { _id: "tmp" as Todo["_id"], text: args.text }]);
  });
</script>

<ul>
  {#each todos.data ?? [] as t (t._id)}
    <li>{t.text}</li>
  {/each}
</ul>
<button onclick={() => onResult?.(add({ text: "new" }).catch((e: unknown) => e))}>add</button>
