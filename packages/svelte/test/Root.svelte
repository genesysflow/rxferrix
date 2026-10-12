<script lang="ts">
  import type { Component } from "svelte";
  import { setupRxFerrix } from "../src/lib/index.js";
  import type { RxFerrixClient } from "@rxferrix/client";

  let { client, child, props = {} }: { client: RxFerrixClient; child: Component<any>; props?: Record<string, unknown> } =
    $props();

  // svelte-ignore state_referenced_locally
  setupRxFerrix(client);
  const Child = $derived(child);
</script>

<Child {...props} />
