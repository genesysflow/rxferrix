<script lang="ts">
  import { useQuery } from "../src/lib/index.js";
  import { api } from "./api.js";

  let { id }: { id: string | null } = $props();

  const todo = useQuery(api.todos.get, () => (id === null ? "skip" : { id }));
</script>

{#if todo.isLoading}
  <p>loading</p>
{:else if todo.error}
  <p>error: {todo.error.message} ({todo.error.code})</p>
{:else if todo.data === undefined}
  <p>skipped</p>
{:else}
  <p>todo: {todo.data?.text ?? "null"}</p>
{/if}
