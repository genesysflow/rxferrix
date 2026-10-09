<script lang="ts">
  import { setupFerrix } from "@ferrix/svelte";
  import TodoList from "./TodoList.svelte";

  // Same origin: Vite proxies /api (including the /api/sync WebSocket) to the Ferrix server.
  setupFerrix(window.location.origin);
</script>

<main>
  <h1>Todos</h1>
  <TodoList />
  <p class="hint">Open this page in a second tab: changes show up instantly in both.</p>
</main>
