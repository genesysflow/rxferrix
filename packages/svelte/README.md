# @ferrix/svelte

Svelte 5 (runes) bindings for Ferrix.

```svelte
<!-- +layout.svelte -->
<script lang="ts">
  import { setupFerrix } from "@ferrix/svelte";
  setupFerrix(import.meta.env.VITE_FERRIX_URL); // or setupFerrix(existingClient)
  let { children } = $props();
</script>
{@render children()}
```

```svelte
<script lang="ts">
  import { useQuery, useMutation } from "@ferrix/svelte";
  import { api } from "$lib/ferrix/api";
  let { channel } = $props();

  const messages = useQuery(api.messages.list, () => ({ channel })); // or () => "skip"
  const send = useMutation(api.messages.send);
</script>
```

API: `setupFerrix(url | client, options?)`, `getFerrixClient()`, `useQuery(ref, argsThunk?, { client? })` →
`{ data, error, isLoading }`, `useMutation(ref)` (callable, `.withOptimisticUpdate(fn)`), `useAction(ref)`,
`useConnectionState()` → `{ current }`. Hooks read the client from context, so call them during component
initialization, or pass `{ client }` explicitly.

## Packaging

The runes code lives in `src/lib/*.svelte.ts`. `bun run build` runs `@sveltejs/package`
(`svelte-package -i src/lib -o dist`), which strips the TypeScript and emits `dist/*.svelte.js` with `.d.ts` files.
The `$derived` runes stay in place: the consumer's Svelte compiler (via `@sveltejs/vite-plugin-svelte` or SvelteKit)
compiles `dist/*.svelte.js` like any other Svelte module. `package.json` exposes `dist/index.js` under both the `svelte`
and `default` export conditions, with types under `types`. So the package only works in a Svelte 5 build pipeline,
which is the only place it makes sense.

## Tests

`bun run test` (vitest + jsdom + `@sveltejs/vite-plugin-svelte`, resolving Svelte's `browser` build). The tests mount
components with `mount`/`flushSync` and drive a fake sync server from `@ferrix/client/testing`.
