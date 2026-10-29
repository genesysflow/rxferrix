# @rxferrix/svelte

Svelte 5 bindings for [RxFerrix](https://genesysflow.github.io/rxferrix/): live queries, mutations, pagination, auth, and connection state. The package requires a Svelte 5 build pipeline because its reactive modules use runes.

## Set up the client

Install `@rxferrix/svelte` in your frontend. In a browser-rendered root component:

```svelte
<script lang="ts">
  import { setupRxFerrix } from "@rxferrix/svelte";
  setupRxFerrix(window.location.origin);
</script>
```

The URL should reach the backend, or a frontend server proxying `/api`, including the WebSocket. Passing a URL creates a client that closes with this component. Passing an existing client leaves cleanup to its owner. Server-rendered apps need to create and manage clients for their own lifecycle rather than using `window` on the server.

## Read and change data

With a generated `api` and the starter task functions, a descendant component can use:

```svelte
<script lang="ts">
  import { useMutation, useQuery } from "@rxferrix/svelte";
  import { api } from "./rxferrix";

  const tasks = useQuery(api.tasks.list);
  const add = useMutation(api.tasks.add);
  let error = $state<string | null>(null);

  async function addTask() {
    try {
      await add({ text: "Buy milk" });
      error = null;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }
</script>

<button onclick={addTask}>Add task</button>
{#if error}<p>{error}</p>{/if}
{#if tasks.isLoading}
  <p>Loading…</p>
{:else if tasks.error}
  <p>{tasks.error.message}</p>
{:else}
  {#each tasks.data ?? [] as task (task._id)}
    <p>{task.text}</p>
  {/each}
{/if}
```

For query arguments, pass a function: `useQuery(ref, () => ({ channel }))`. Reactive reads inside it update the subscription; return `"skip"` to disable it. Read query fields directly in reactive code instead of destructuring their current values.

## Available bindings

| Binding | Result or purpose |
| --- | --- |
| `setupRxFerrix(urlOrClient, options?)` | Put a client in component context. |
| `getRxFerrixClient()` | Read that client. |
| `useQuery(ref, argsThunk?, options?)` | Reactive `data`, `error`, `isLoading`, and `isStale`. |
| `usePaginatedQuery(ref, argsThunk, options)` | Live pages with `results`, `status`, `error`, and `loadMore`. |
| `useMutation(ref)` | Callable mutation with `.withOptimisticUpdate(...)`. |
| `useAction(ref)` | Callable action. |
| `useConnectionState()` | Reactive connection state under `.current`. |
| `useAuth()` | Reactive auth state and sign-in/out actions. |
| `useCurrentUser()` | Reactive current-user result. |

Call bindings during component initialization when using context. Bindings accepting `{ client }` can use an explicit client. Query options include `keepPreviousData: true` for retaining results while new arguments load.

See the [client guide](https://genesysflow.github.io/rxferrix/guide/clients) for pagination, optimistic updates, and reconnect behavior.

## Work on this package

From the framework root, run `bun run build` for all client packages. Inside this package, `bun run build` runs `svelte-package`, emitting `.svelte.js` files and declarations to `dist`. The consuming app's Svelte compiler compiles the remaining runes.

`bun run test` runs Vitest with jsdom and the Svelte Vite plugin. Tests mount components and drive the fake server from `@rxferrix/client/testing`. See the repository's [contributor guide](../../CONTRIBUTING.md).
