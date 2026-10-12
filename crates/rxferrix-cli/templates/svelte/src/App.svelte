<script lang="ts">
  import { setupRxFerrix, useMutation, useQuery } from "@rxferrix/svelte";
  import { api } from "./rxferrix";

  // Same origin: Vite proxies /api (including the /api/sync WebSocket) to the RxFerrix server.
  setupRxFerrix(window.location.origin);

  // Live: updates whenever a mutation (from any tab or client) changes the result.
  const tasks = useQuery(api.tasks.list);
  const add = useMutation(api.tasks.add);
  const setDone = useMutation(api.tasks.set_done);
  const remove = useMutation(api.tasks.remove);
  let text = $state("");
  let error = $state<string | null>(null);

  async function run(promise: Promise<unknown>) {
    error = null;
    try {
      await promise;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  function submit(e: SubmitEvent) {
    e.preventDefault();
    void run(add({ text }).then(() => (text = "")));
  }
</script>

<main>
  <h1>Tasks</h1>
  <form onsubmit={submit}>
    <input type="text" bind:value={text} placeholder="What needs doing?" />
    <button type="submit">Add</button>
  </form>
  {#if error}
    <p class="error">{error}</p>
  {/if}
  {#if tasks.isLoading}
    <p class="hint">Loading…</p>
  {:else if tasks.error}
    <p class="error">{tasks.error.message}</p>
  {:else}
    <ul>
      {#each tasks.data ?? [] as task (task._id)}
        <li class:done={task.done}>
          <input
            type="checkbox"
            checked={task.done}
            onchange={(e) => run(setDone({ id: task._id, done: e.currentTarget.checked }))}
          />
          <span>{task.text}</span>
          <button onclick={() => run(remove({ id: task._id }))} aria-label="Delete">✕</button>
        </li>
      {/each}
    </ul>
  {/if}
  <p class="hint">Open this page in a second tab: changes show up instantly in both.</p>
</main>
