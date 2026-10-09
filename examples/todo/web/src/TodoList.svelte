<script lang="ts">
  import { useAction, useConnectionState, useMutation, useQuery } from "@ferrix/svelte";
  import { api, type Doc } from "./lib/ferrix";

  type Filter = "all" | "open" | "done";
  let filter = $state<Filter>("all");
  let text = $state("");
  let error = $state<string | null>(null);

  const todos = useQuery(api.todos.list, () => (filter === "all" ? {} : { done: filter === "done" }));
  const remaining = useQuery(api.todos.remaining);
  const connection = useConnectionState();

  // Optimistic: the new todo appears immediately (in the unfiltered list), before the server confirms it.
  const add = useMutation(api.todos.add).withOptimisticUpdate((store, args) => {
    const current = store.getQuery(api.todos.list, {});
    if (current === undefined) return;
    const temp = {
      _id: `optimistic-${crypto.randomUUID()}`,
      _creationTime: Date.now(),
      text: args.text.trim(),
      done: false,
    } as Doc<"todos">;
    store.setQuery(api.todos.list, {}, [temp, ...current]);
  });
  const setDone = useMutation(api.todos.set_done);
  const remove = useMutation(api.todos.remove);
  const clearCompleted = useMutation(api.todos.clear_completed);
  const addSamples = useAction(api.todos.add_samples);

  async function run(promise: Promise<unknown>) {
    error = null;
    try {
      await promise;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  function submit(event: SubmitEvent) {
    event.preventDefault();
    const value = text;
    text = "";
    run(add({ text: value }));
  }
</script>

<form onsubmit={submit}>
  <input bind:value={text} placeholder="What needs doing?" aria-label="New todo" />
  <button type="submit">Add</button>
</form>

<div class="toolbar">
  {#each ["all", "open", "done"] as const as f (f)}
    <button class:active={filter === f} onclick={() => (filter = f)}>{f}</button>
  {/each}
  <span class="spacer"></span>
  <button onclick={() => run(addSamples({ count: 3 }))}>+3 samples</button>
  <button onclick={() => run(clearCompleted())}>Clear completed</button>
</div>

{#if error}<p class="error" role="alert">{error}</p>{/if}

{#if todos.isLoading}
  <p>Loading…</p>
{:else if todos.error}
  <p class="error">{todos.error.message}</p>
{:else}
  <ul data-testid="todos">
    {#each todos.data ?? [] as todo (todo._id)}
      <li class:done={todo.done}>
        <label>
          <input
            type="checkbox"
            checked={todo.done}
            onchange={(e) => run(setDone({ id: todo._id, done: e.currentTarget.checked }))}
          />
          <span>{todo.text}</span>
        </label>
        <button class="remove" aria-label="Delete" onclick={() => run(remove({ id: todo._id }))}>×</button>
      </li>
    {:else}
      <li class="empty">Nothing here.</li>
    {/each}
  </ul>
{/if}

<footer>
  <span data-testid="remaining">{remaining.data ?? "…"} open</span>
  <span class="status" class:offline={!connection.current.isWebSocketConnected}>
    {connection.current.isWebSocketConnected ? "live" : "offline"}
  </span>
</footer>
