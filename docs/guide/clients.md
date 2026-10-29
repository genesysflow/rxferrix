# Clients: React and Svelte

`@rxferrix/client` is the framework-agnostic core; `@rxferrix/react` and `@rxferrix/svelte` wrap it in hooks. Both use
the `api` object that `serve --dev` (or `rxferrix codegen`) generates from your functions, so arguments and results
are typed.

## Setup

React: one client for the app, given to a provider.

```tsx
import { RxFerrixClient, RxFerrixProvider } from "@rxferrix/react";

const client = new RxFerrixClient(window.location.origin);
createRoot(root).render(
  <RxFerrixProvider client={client}>
    <App />
  </RxFerrixProvider>,
);
```

Svelte 5: `setupRxFerrix(url)` in the root component (it closes the client when that component is destroyed).

```svelte
<script lang="ts">
  import { setupRxFerrix } from "@rxferrix/svelte";
  setupRxFerrix(window.location.origin);
</script>
```

## Queries

```tsx
const todos = useQuery(api.todos.list, { done: false });       // undefined while loading, then always current
const user = useQuery(api.users.get, id ? { id } : "skip");     // "skip": subscribe to nothing
const { data, error, isLoading } = useQueryWithStatus(api.todos.list, {});  // errors as a value, not thrown
```

`useQuery` throws a failed query's error, for an error boundary. Arguments are compared by value, so an inline object
doesn't resubscribe. Changing them shows loading until the new result comes; `useQueryKeepingPrevious` keeps the
previous result meanwhile (with `isStale`).

In Svelte, `useQuery` takes a function for the args (re-evaluated reactively) and returns an object whose fields
you read in the template:

```svelte
<script lang="ts">
  let { channel } = $props();
  const messages = useQuery(api.messages.list, () => ({ channel }), { keepPreviousData: true });
</script>
{#if messages.isLoading}Loading…{:else if messages.error}{messages.error.message}{:else}…{/if}
```

Results are read-only: copy before sorting or changing them (`[...todos].sort(…)`).

## Mutations and actions

```tsx
const add = useMutation(api.todos.add);
await add({ text: "Buy milk" });            // resolves once this client sees the write in its query results
const run = useAction(api.ai.summarize);
```

A mutation runs exactly once, even when the connection drops and the client sends it again. If it was sent more than
9 minutes earlier, or the signed-in user changed meanwhile, it is not sent again and rejects with code
`OutcomeUnknown` instead: it may or may not have run. An action in flight when the connection drops rejects with
`ConnectionLost` (actions are never retried).

**Optimistic updates** show the result before the server answers; they are replayed over every server change until
the mutation's write arrives (or it fails):

```tsx
const add = useMutation(api.todos.add).withOptimisticUpdate((store, args) => {
  const current = store.getQuery(api.todos.list, {});
  if (current) store.setQuery(api.todos.list, {}, [{ _id: tempId, _creationTime: now, ...args, done: false }, ...current]);
});
```

Create temporary ids and timestamps outside the closure, and build new arrays instead of pushing into `current`.

## Paginated queries

For a query that takes `paginationOpts` and returns a `PaginationResult` (see [Functions](/guide/functions)):

```tsx
const { results, status, loadMore } = usePaginatedQuery(api.messages.list, { channel }, { initialNumItems: 20 });
<button disabled={status !== "CanLoadMore"} onClick={() => loadMore(20)}>Load more</button>
```

Every page stays live. `insertAtTop` / `getAllPages` help with optimistic updates of paginated queries.

## Errors

Failures are `RxFerrixError`s. A `UserError` from your function keeps its `code`, `message` and `data`; anything else
is `"Server Error"` outside `--dev`. Client-side codes: `ConnectionLost`, `ClientClosed`, `OutcomeUnknown`,
`too_many_queries` (more than 1024 live queries on one connection).

## Auth

`useRxFerrixAuth()` (React) and `useAuth()` (Svelte) give the auth state and actions (`signIn`, `signUp`, `signOut`,
magic links, OAuth, two-factor), and `useCurrentUser()` the signed-in user's document. See [Auth](/guide/auth).

## Connection state

`useConnectionState()` reports whether the socket is connected and whether requests are waiting, e.g. for an
"offline" banner or an "unsaved changes" warning.
