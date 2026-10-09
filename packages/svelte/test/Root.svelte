<script lang="ts">
  import type { Component } from "svelte";
  import { setupFerrix } from "../src/lib/index.js";
  import type { FerrixClient } from "@ferrix/client";

  let { client, child, props = {} }: { client: FerrixClient; child: Component<any>; props?: Record<string, unknown> } =
    $props();

  // svelte-ignore state_referenced_locally
  setupFerrix(client);
  const Child = $derived(child);
</script>

<Child {...props} />
