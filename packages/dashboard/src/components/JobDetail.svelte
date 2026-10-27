<script lang="ts">
  import type { ScheduledJob } from "../lib/api.js";
  import { formatTime, pretty, prettyArgs } from "../lib/format.js";
  import { href, link } from "../lib/router.svelte.js";
  import IdLink from "./IdLink.svelte";

  /** A scheduled job's args, outcome and timeline. `inHistory`: the job is in the job history, not a document. */
  let { job, inHistory = false }: { job: ScheduledJob; inHistory?: boolean } = $props();
</script>

<div class="detail-grid">
  <div>
    <h3>Args</h3>
    <pre>{pretty(job.args)}</pre>
  </div>
  {#if job.error}
    <div>
      <h3>Error</h3>
      <pre class="error-text">{job.error}</pre>
    </div>
  {/if}
  {#if job.result !== null}
    <div>
      <h3>Result</h3>
      <pre>{prettyArgs(job.result)}</pre>
    </div>
  {/if}
  <div class="times">
    <h3>Timeline</h3>
    <dl>
      <dt>Scheduled</dt><dd>{formatTime(job.scheduledAt)}</dd>
      <dt>Due</dt><dd>{formatTime(job.runAt)}</dd>
      <dt>Started</dt><dd>{job.startedAt ? formatTime(job.startedAt) : "—"}</dd>
      <dt>Finished</dt><dd>{job.completedAt ? formatTime(job.completedAt) : "—"}</dd>
      {#if job.pool}<dt>Workpool</dt><dd class="mono">{job.pool}</dd>{/if}
      {#if job.onComplete}<dt>On complete</dt><dd class="mono">{job.onComplete.path}</dd>{/if}
      {#if job.origin?.startsWith("workflow:")}
        <dt>Workflow</dt>
        <dd><a href={href("workflows", [], { id: job.origin.slice("workflow:".length) })} onclick={link}>{job.origin.slice("workflow:".length)}</a></dd>
      {/if}
      {#if inHistory}
        <dt>Id</dt><dd class="mono">{job._id}</dd>
      {:else}
        <dt>Document</dt><dd><IdLink id={job._id} /></dd>
      {/if}
    </dl>
    <a href={href("logs")} onclick={link}>Execution logs</a>
  </div>
</div>

<style>
  .detail-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
    gap: 16px;
  }
  .detail-grid pre {
    max-height: 240px;
    overflow: auto;
    white-space: pre-wrap;
    word-break: break-word;
  }
  dl {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 2px 12px;
    margin: 0 0 8px;
  }
  dt {
    color: var(--muted);
  }
  dd {
    margin: 0;
  }
</style>
