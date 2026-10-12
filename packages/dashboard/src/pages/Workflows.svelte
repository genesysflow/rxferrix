<script lang="ts">
  import { RxFerrixError } from "@rxferrix/client";
  import { useMutation, usePaginatedQuery, useQuery } from "@rxferrix/svelte";
  import { workflows, type StepState, type Workflow, type WorkflowState, type WorkflowStep } from "../lib/api.js";
  import { dashboardContext } from "../lib/context.js";
  import { formatRelative, formatSpan, formatTime, pretty } from "../lib/format.js";
  import { href, router, type Route } from "../lib/router.svelte.js";
  import Empty from "../components/Empty.svelte";
  import IdLink from "../components/IdLink.svelte";

  let { route }: { route: Route } = $props();
  const ctx = dashboardContext();

  const filters: { state: WorkflowState | null; label: string }[] = [
    { state: null, label: "All" },
    { state: "running", label: "Running" },
    { state: "success", label: "Success" },
    { state: "failed", label: "Failed" },
    { state: "canceled", label: "Canceled" },
  ];

  const filterState = $derived.by((): WorkflowState | null => {
    const s = route.params.get("state");
    return filters.some((f) => f.state === s) ? (s as WorkflowState) : null;
  });
  /** The expanded workflow (`?id=`), e.g. linked from a step's job on the Scheduler page. */
  const open = $derived(route.params.get("id"));
  const list = usePaginatedQuery(workflows.list, () => ({ state: filterState }), { initialNumItems: 50 });
  const steps = useQuery(workflows.steps, () => (open ? { id: open } : "skip"));
  const cancelWorkflow = useMutation(workflows.cancelWorkflow);

  let now = $state(Date.now());
  $effect(() => {
    const timer = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(timer);
  });

  function navigate(state: WorkflowState | null, id: string | null) {
    router.navigate(href("workflows", [], { state, id }));
  }

  async function cancel(wf: Workflow) {
    if (!confirm(`Cancel the workflow ${wf.name}? Its running steps are canceled; what its steps already did stays done.`)) return;
    try {
      const outcome = await cancelWorkflow({ id: wf._id });
      ctx.toast(outcome === "canceled" ? "Workflow canceled" : "The workflow had already finished", outcome === "canceled" ? "info" : "error");
    } catch (e) {
      ctx.toast(e instanceof RxFerrixError ? e.message : String(e), "error");
    }
  }

  function took(start: number, end: number | null): string {
    return formatSpan((end ?? now) - start);
  }

  function stepDetail(step: WorkflowStep): string {
    if (step.kind === "sleep") {
      const ms = (step.args as { ms?: number } | null)?.ms ?? 0;
      return `for ${formatSpan(ms)}`;
    }
    if (step.kind === "event") return "waiting for the event";
    return pretty(step.args);
  }

  const stepLabels: Record<StepState, string> = { running: "running", success: "success", failed: "failed", canceled: "canceled" };
</script>

<div class="page">
  <header class="page-header">
    <h1>Workflows</h1>
    <span class="spacer"></span>
    <div class="filters" role="group" aria-label="Filter workflows by state">
      {#each filters as f (f.label)}
        <button class="small" class:active={filterState === f.state} aria-pressed={filterState === f.state} onclick={() => navigate(f.state, open)}>
          {f.label}
        </button>
      {/each}
    </div>
  </header>
  <div class="body">
    {#if list.error}
      <div class="alert" role="alert">{list.error.message}</div>
    {:else if list.isLoading && list.results.length === 0}
      <Empty title="Loading…" />
    {:else if list.results.length === 0}
      <Empty
        title={filterState ? `No ${filterState} workflows` : "No workflows"}
        hint="Workflows appear here when a function calls ctx.workflows.start::<w>(args) for a #[workflow]."
      />
    {:else}
      <table class="grid" data-testid="workflows">
        <thead>
          <tr><th>Workflow</th><th>State</th><th>Steps</th><th>Started</th><th>Took</th><th>Origin</th><th></th></tr>
        </thead>
        <tbody>
          {#each list.results as wf (wf._id)}
            <tr onclick={() => navigate(filterState, open === wf._id ? null : wf._id)} class:selected={open === wf._id} aria-expanded={open === wf._id}>
              <td class="mono">{wf.name}</td>
              <td><span class="badge state {wf.state}">{wf.state}</span></td>
              <td>{wf.steps}</td>
              <td title={formatTime(wf.startedAt)}>{formatRelative(wf.startedAt, now)}</td>
              <td>{took(wf.startedAt, wf.completedAt)}</td>
              <td class="mono faint">{wf.origin ?? ""}</td>
              <td class="actions" onclick={(e) => e.stopPropagation()}>
                {#if wf.state === "running"}<button class="small danger" onclick={() => cancel(wf)}>Cancel</button>{/if}
              </td>
            </tr>
            {#if open === wf._id}
              <tr class="detail">
                <td colspan="7">
                  <div class="detail-grid">
                    <div>
                      <h3>Args</h3>
                      <pre>{pretty(wf.args)}</pre>
                    </div>
                    {#if wf.error}
                      <div>
                        <h3>{wf.state === "canceled" ? "Canceled" : "Error"}</h3>
                        <pre class="error-text">{wf.error}</pre>
                      </div>
                    {/if}
                    {#if wf.state === "success"}
                      <div>
                        <h3>Result</h3>
                        <pre>{pretty(wf.result)}</pre>
                      </div>
                    {/if}
                    {#if wf.events.length > 0}
                      <div>
                        <h3>Events not waited for yet</h3>
                        <pre>{wf.events.map((e) => `${e.name}: ${JSON.stringify(e.value)}`).join("\n")}</pre>
                      </div>
                    {/if}
                    <div class="times">
                      <h3>Details</h3>
                      <dl>
                        <dt>Started</dt><dd>{formatTime(wf.startedAt)}</dd>
                        <dt>Finished</dt><dd>{wf.completedAt ? formatTime(wf.completedAt) : "—"}</dd>
                        {#if wf.onComplete}<dt>On complete</dt><dd class="mono">{wf.onComplete.path}</dd>{/if}
                        <dt>Document</dt><dd><IdLink id={wf._id} /></dd>
                      </dl>
                    </div>
                  </div>
                  <h3>Steps</h3>
                  {#if steps.error}
                    <div class="alert" role="alert">{steps.error.message}</div>
                  {:else if steps.data && steps.data.length === 0}
                    <p class="muted">No steps yet.</p>
                  {:else if steps.data}
                    <table class="grid steps" data-testid="workflow-steps">
                      <thead>
                        <tr><th>#</th><th>Step</th><th>State</th><th>Args</th><th>Outcome</th><th>Took</th><th>Job</th></tr>
                      </thead>
                      <tbody>
                        {#each steps.data as step (step._id)}
                          <tr>
                            <td class="faint">{step.index}</td>
                            <td><span class="badge {step.kind}">{step.kind}</span> <span class="mono">{step.kind === "sleep" ? "" : step.name}</span></td>
                            <td><span class="badge state {step.state}">{stepLabels[step.state]}</span></td>
                            <td><pre class="inline">{stepDetail(step)}</pre></td>
                            <td>
                              {#if step.error}<pre class="inline error-text">{step.error}</pre>
                              {:else if step.state === "success" && step.kind !== "sleep"}<pre class="inline">{pretty(step.result)}</pre>{/if}
                            </td>
                            <td>{took(step.startedAt, step.completedAt)}</td>
                            <td>{#if step.job}<IdLink id={step.job} />{/if}</td>
                          </tr>
                        {/each}
                      </tbody>
                    </table>
                  {/if}
                </td>
              </tr>
            {/if}
          {/each}
        </tbody>
      </table>
      <div class="more">
        {#if list.status === "CanLoadMore"}<button onclick={() => list.loadMore(50)}>Load more</button>{/if}
      </div>
    {/if}
  </div>
</div>

<style>
  .body {
    flex: 1;
    overflow: auto;
    padding-bottom: 24px;
  }
  .filters {
    display: flex;
    gap: 4px;
  }
  .filters button.active {
    background: var(--accent-soft);
    color: var(--accent-text);
    border-color: transparent;
    font-weight: 600;
  }
  .badge.state.running {
    color: var(--warn);
    background: var(--warn-soft);
    border-color: transparent;
  }
  .badge.state.failed {
    color: var(--err);
    background: var(--err-soft);
    border-color: transparent;
  }
  .badge.state.canceled {
    color: var(--muted);
  }
  tr.detail {
    cursor: default;
    background: var(--panel-2);
  }
  tr.detail td {
    max-width: none;
    padding: 8px 16px 14px;
  }
  .detail-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
    gap: 16px;
    margin-bottom: 8px;
  }
  .detail-grid pre {
    max-height: 240px;
    overflow: auto;
    white-space: pre-wrap;
    word-break: break-word;
  }
  table.steps {
    background: var(--panel);
  }
  table.steps tbody tr {
    cursor: default;
  }
  pre.inline {
    margin: 0;
    max-height: 120px;
    max-width: 360px;
    overflow: auto;
    white-space: pre-wrap;
    word-break: break-word;
    font-size: 12px;
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
  .more {
    padding: 12px 20px;
  }
</style>
