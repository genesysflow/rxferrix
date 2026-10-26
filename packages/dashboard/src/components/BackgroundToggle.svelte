<script lang="ts">
  import { background } from "../lib/background.svelte.js";
</script>

<!-- The shared "hide background jobs" switch of the execution log's views (lib/background.svelte.ts). -->
<label
  class="inline"
  title="Leave out scheduled jobs, crons and workflow steps (and the functions they call), to see only what clients ran. Applies to every page."
>
  <input type="checkbox" data-testid="hide-background" bind:checked={background.hidden} /> hide background jobs
</label>
