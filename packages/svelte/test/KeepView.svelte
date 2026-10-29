<script lang="ts">
  import { useQuery } from "../src/lib/index.js";
  import { api } from "./api.js";

  let { id }: { id: string } = $props();

  const todo = useQuery(api.todos.get, () => ({ id }), { keepPreviousData: true });
</script>

{#if todo.isLoading}
  <p>loading</p>
{:else}
  <p>todo: {todo.data?.text ?? "null"}{todo.isStale ? " (stale)" : ""}</p>
{/if}
