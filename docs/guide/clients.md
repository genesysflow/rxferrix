# Connect a client

`@rxferrix/client` handles subscriptions, reconnects, and requests. `@rxferrix/react` and `@rxferrix/svelte` add framework bindings. All use the function references generated from your backend.

The examples use a `tasks` module with `list`, `add`, and `set_done` functions. The [functions guide](/guide/functions) defines those functions; the project generator includes a simpler unfiltered `list`.

## Set up React

Install `@rxferrix/react` and create one client outside component rendering:

```tsx
import { createRoot } from "react-dom/client";
import { RxFerrixClient, RxFerrixProvider } from "@rxferrix/react";
import { App } from "./App";

const client = new RxFerrixClient(window.location.origin);

createRoot(document.getElementById("root")!).render(
  <RxFerrixProvider client={client}>
    <App />
  </RxFerrixProvider>,
);
```

The generated app already does this. `window.location.origin` works when Vite proxies `/api` during development and when the backend serves the built frontend. For a separate backend origin, pass that URL and configure the server's CORS origins.

The provider makes the client available to descendant hooks. If you own the client lifetime, call `client.close()` when the app no longer needs it.

## Read data in React

```tsx
import { useQuery, useQueryWithStatus } from "@rxferrix/react";
import { api } from "./rxferrix";

const tasks = useQuery(api.tasks.list, { done: false });
const { data, error, isLoading } = useQueryWithStatus(api.tasks.list, {});
```

Use one of these hooks for a given view. `useQuery` returns `undefined` while loading and throws a query error for an error boundary. `useQueryWithStatus` returns the error as a value for inline rendering.

Arguments are compared by value, so a fresh inline object does not resubscribe. Pass `"skip"` as the arguments when required inputs are unavailable. Changing arguments normally shows loading until the new result arrives. `useQueryKeepingPrevious` retains the previous result and marks it with `isStale` while the next result loads.

## Set up Svelte 5

Install `@rxferrix/svelte`. Call `setupRxFerrix` during component initialization, usually in the root component:

```svelte
<script lang="ts">
  import { setupRxFerrix } from "@rxferrix/svelte";
  setupRxFerrix(window.location.origin);
</script>
```

A URL creates a client that closes when the component is destroyed. Passing an existing client leaves its lifetime to you. This browser setup uses `window`; a server-rendered application needs to manage client creation for its own lifecycle.

In a component using the client:

```svelte
<script lang="ts">
  import { useQuery } from "@rxferrix/svelte";
  import { api } from "./rxferrix";

  let done = $state(false);
  const tasks = useQuery(api.tasks.list, () => ({ done }));
</script>

{#if tasks.isLoading}
  <p>Loading…</p>
{:else if tasks.error}
  <p>{tasks.error.message}</p>
{:else}
  <ul>
    {#each tasks.data ?? [] as task (task._id)}
      <li>{task.text}</li>
    {/each}
  </ul>
{/if}
```

Svelte arguments are a function so reactive inputs are tracked. Return `"skip"` to disable the subscription. Read `tasks.data`, `tasks.error`, `tasks.isLoading`, and `tasks.isStale` directly in reactive code; destructuring their current values loses that reactive access. Use `{ keepPreviousData: true }` as the third argument to retain previous results while arguments change.

## Call mutations and actions

React and Svelte expose `useMutation` and `useAction`:

```ts
const add = useMutation(api.tasks.add);
await add({ text: "Buy milk" });
```

Call the hook where your framework expects it, then invoke the returned function from an event handler. Catch rejected promises to display failures. A mutation promise resolves after the client's subscribed results reach the mutation's commit. Actions resolve when their work finishes.

Subscriptions update automatically after a mutation. You do not call a refetch method. Results are read-only: use a copy before sorting or changing an array.

## Add an optimistic update

Show a change before the server confirms it by updating matching local query results. For example, inside a React component:

```tsx
const setDone = useMutation(api.tasks.set_done).withOptimisticUpdate((store, args) => {
  const current = store.getQuery(api.tasks.list, {});
  if (current !== undefined) {
    store.setQuery(api.tasks.list, {}, current.map((task) =>
      task._id === args.id ? { ...task, done: args.done } : task
    ));
  }
});
```

This updates the unfiltered `list` subscription with `{}` arguments. If you show filtered lists too, update those matching cached queries with the appropriate membership rules.

The client replays optimistic updates over every server result until the mutation is visible or fails. Build new arrays and objects. For optimistic inserts, generate temporary IDs and timestamps once per invocation, outside the replayed closure.

## Paginate a list

For the `page` query in the [functions guide](/guide/functions#paginate-a-growing-list), a React component can use:

```tsx
const { results, status, loadMore } = usePaginatedQuery(
  api.tasks.page,
  {},
  { initialNumItems: 20 },
);

// In the component's returned JSX:
<button disabled={status !== "CanLoadMore"} onClick={() => loadMore(20)}>
  Load more
</button>
```

Import `usePaginatedQuery` from your binding. React also has `usePaginatedQueryWithStatus` for inline page errors. Svelte's hook returns a reactive object with `results`, `status`, `error`, and `loadMore`; pass its arguments as a function and read its fields directly.

Loaded pages remain live. `insertAtTop` and `getAllPages` support optimistic changes to paginated results; the [chat example](https://github.com/genesysflow/rxferrix/tree/master/examples/chat-react) uses both.

## Use the core client

Without React or Svelte, install `@rxferrix/client`:

```ts
import { RxFerrixClient } from "@rxferrix/client";
import { api } from "./rxferrix";

const client = new RxFerrixClient("http://127.0.0.1:8090");
const watch = client.watchQuery(api.tasks.list, {});
const unsubscribe = watch.onUpdate(() => {
  console.log(watch.localQueryState());
});

await client.mutation(api.tasks.add, { text: "Buy milk" });
const tasks = await client.query(api.tasks.list, {});

unsubscribe();
client.close();
```

`watchQuery` starts a subscription when you register a listener. `query` waits for one result and unsubscribes. `localQueryState()` represents loading, success, or error without throwing; `localQueryResult()` returns the current value or throws its error.

The core client uses a WebSocket even for one-shot `query` calls. For direct HTTP requests, use the [HTTP API reference](/PROTOCOL#http-api-non-realtime).

## Auth, connection state, and retries

React has `useRxFerrixAuth`, `useAuthActions`, and `useCurrentUser`; Svelte has `useAuth` and `useCurrentUser`. See [Auth](/guide/auth) for setup and external token providers.

`useConnectionState()` returns connection/request state in React. Svelte returns an object whose `.current` is reactive. Use this to show connection loss or pending work.

Request failures are `RxFerrixError` objects. Expected server errors preserve the `UserError` code, message, and data. Client errors include `ConnectionLost`, `ClientClosed`, `OutcomeUnknown`, and `too_many_queries`.

Unanswered mutations are retried only within the supported window and identity. An `OutcomeUnknown` means the mutation may already have committed. Actions interrupted by connection loss reject with `ConnectionLost` and are not automatically retried. See [reconnect behavior](/guide/things-to-know#reconnects-and-retries) before adding application retries.
