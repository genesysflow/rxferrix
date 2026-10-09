<script lang="ts">
  import { FerrixError, uploadFile } from "@ferrix/client";
  import { useMutation, usePaginatedQuery, useQuery } from "@ferrix/svelte";
  import { storage, type FileSummary } from "../lib/api.js";
  import { dashboardContext } from "../lib/context.js";
  import { formatBytes, formatTime, previewKind, sameOriginPath } from "../lib/format.js";
  import { href, router, type Route } from "../lib/router.svelte.js";
  import Empty from "../components/Empty.svelte";

  /** The Files page: stored files (live), previews, download, delete, and upload by picker or drag and drop. */
  let { route }: { route: Route } = $props();
  const ctx = dashboardContext();
  const files = usePaginatedQuery(storage.files, () => ({}), { initialNumItems: 50 });
  const deleteFile = useMutation(storage.deleteFile);
  const generateUploadUrl = useMutation(storage.generateUploadUrl);

  const selectedId = $derived(route.params.get("file"));
  // The selected file, live (it may not be on a loaded page, e.g. when following an id from the data grid).
  const selectedQuery = useQuery(storage.file, () => (selectedId ? { id: selectedId } : "skip"));
  const selected = $derived(selectedQuery.data ?? null);

  /** `/api/storage/<id>` on this origin (the dashboard's CSP only allows same-origin requests). */
  const fileUrl = (f: FileSummary, download = false) => sameOriginPath(f.url) + (download ? "?download=1" : "");
  const label = (f: FileSummary) => f.filename ?? f._id;

  function select(id: string | null) {
    router.navigate(href("files", [], { file: id }), { replace: true });
  }

  async function remove(f: FileSummary) {
    if (!confirm(`Delete ${label(f)}? Documents that reference it keep the dangling id.`)) return;
    try {
      await deleteFile({ id: f._id });
      ctx.toast(`Deleted ${label(f)}`);
      if (selectedId === f._id) select(null);
    } catch (e) {
      ctx.toast(e instanceof FerrixError ? e.message : String(e), "error");
    }
  }

  // Uploads in progress.
  interface Upload {
    key: number;
    name: string;
    loaded: number;
    total: number;
  }
  let uploads = $state<Upload[]>([]);
  let nextKey = 0;
  let dragging = $state(false);
  let picker: HTMLInputElement | undefined = $state();

  async function upload(list: FileList | File[]) {
    for (const file of Array.from(list)) {
      const entry: Upload = { key: nextKey++, name: file.name, loaded: 0, total: file.size };
      uploads = [...uploads, entry];
      try {
        const url = await generateUploadUrl({});
        const id = await uploadFile(sameOriginPath(url), file, {
          onProgress: (p) => {
            uploads = uploads.map((u) => (u.key === entry.key ? { ...u, loaded: p.loaded, total: p.total } : u));
          },
        });
        ctx.toast(`Uploaded ${file.name}`);
        select(id);
      } catch (e) {
        ctx.toast(`${file.name}: ${e instanceof Error ? e.message : String(e)}`, "error");
      } finally {
        uploads = uploads.filter((u) => u.key !== entry.key);
      }
    }
  }

  function ondrop(e: DragEvent) {
    e.preventDefault();
    dragging = false;
    if (e.dataTransfer?.files.length) void upload(e.dataTransfer.files);
  }

  function ondragover(e: DragEvent) {
    if (!e.dataTransfer?.types.includes("Files")) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "copy";
    dragging = true;
  }

  // A text preview: the first 64 KB, fetched with a Range request.
  const TEXT_PREVIEW_BYTES = 64 * 1024;
  let text = $state<{ id: string; body: string; truncated: boolean } | null>(null);
  $effect(() => {
    const f = selected;
    if (!f || previewKind(f.contentType) !== "text") return;
    let cancelled = false;
    void fetch(fileUrl(f), { headers: { Range: `bytes=0-${TEXT_PREVIEW_BYTES - 1}` } })
      .then((r) => (r.ok ? r.text() : Promise.reject(new Error(`HTTP ${r.status}`))))
      .then((body) => {
        if (!cancelled) text = { id: f._id, body, truncated: f.size > TEXT_PREVIEW_BYTES };
      })
      .catch((e: unknown) => {
        if (!cancelled) text = { id: f._id, body: `Preview failed: ${String(e)}`, truncated: false };
      });
    return () => {
      cancelled = true;
    };
  });
</script>

<div
  class="page"
  role="region"
  aria-label="Files"
  {ondragover}
  ondragleave={(e) => {
    if (e.currentTarget === e.target) dragging = false;
  }}
  {ondrop}
>
  <header class="page-header">
    <h1>Files</h1>
    <span class="muted">{files.results.length}{files.status === "CanLoadMore" ? "+" : ""} files</span>
    <span class="spacer"></span>
    <span class="faint hint">Drop files anywhere on this page</span>
    <button class="primary" onclick={() => picker?.click()}>Upload…</button>
    <input
      bind:this={picker}
      type="file"
      multiple
      hidden
      aria-label="Upload files"
      onchange={(e) => {
        const input = e.currentTarget;
        if (input.files?.length) void upload(Array.from(input.files));
        input.value = "";
      }}
    />
  </header>

  {#if uploads.length}
    <ul class="uploads" aria-label="Uploads in progress">
      {#each uploads as u (u.key)}
        <li>
          <span class="name">{u.name}</span>
          <progress max={u.total || 1} value={u.loaded}></progress>
          <span class="faint">{formatBytes(u.loaded)} / {formatBytes(u.total)}</span>
        </li>
      {/each}
    </ul>
  {/if}

  <div class="layout" class:with-panel={selected !== null}>
    <div class="body" data-testid="files">
      {#if files.error}
        <div class="alert" role="alert">{files.error.message}</div>
      {:else if files.isLoading && files.results.length === 0}
        <Empty title="Loading…" />
      {:else if files.results.length === 0}
        <Empty
          title="No files yet"
          hint="Files appear here when they are uploaded through an upload URL (ctx.storage.generate_upload_url) or stored by an action. Drop a file here to upload one."
        >
          <button onclick={() => picker?.click()}>Upload a file</button>
        </Empty>
      {:else}
        <table class="grid">
          <thead>
            <tr><th></th><th>Name</th><th>Type</th><th>Size</th><th>Created</th><th>By</th><th></th></tr>
          </thead>
          <tbody>
            {#each files.results as f (f._id)}
              <tr
                onclick={() => select(selectedId === f._id ? null : f._id)}
                class:selected={selectedId === f._id}
                aria-selected={selectedId === f._id}
              >
                <td class="thumb">
                  {#if previewKind(f.contentType) === "image"}
                    <img src={fileUrl(f)} alt="" loading="lazy" />
                  {:else}
                    <span class="icon" aria-hidden="true">▤</span>
                  {/if}
                </td>
                <td class="name" title={f._id}>{label(f)}</td>
                <td class="mono faint">{f.contentType ?? "—"}</td>
                <td>{formatBytes(f.size)}</td>
                <td class="faint">{formatTime(f._creationTime)}</td>
                <td class="mono faint by" title={f.createdBy ?? ""}>{f.createdBy ?? ""}</td>
                <td onclick={(e) => e.stopPropagation()}>
                  <div class="actions">
                    <a href={fileUrl(f, true)} download={f.filename ?? ""}>Download</a>
                    <button class="small danger" onclick={() => remove(f)}>Delete</button>
                  </div>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
        <div class="more">
          {#if files.status === "CanLoadMore"}<button onclick={() => files.loadMore(50)}>Load more</button>{/if}
        </div>
      {/if}
    </div>

    {#if selected}
      {@const f = selected}
      {@const kind = previewKind(f.contentType)}
      <aside class="panel" aria-label="File details">
        <header>
          <h2 title={label(f)}>{label(f)}</h2>
          <button class="small ghost" onclick={() => select(null)} aria-label="Close">✕</button>
        </header>
        <div class="preview">
          {#if kind === "image"}
            <img src={fileUrl(f)} alt={label(f)} />
          {:else if kind === "video"}
            <!-- svelte-ignore a11y_media_has_caption -->
            <video src={fileUrl(f)} controls preload="metadata"></video>
          {:else if kind === "audio"}
            <audio src={fileUrl(f)} controls preload="metadata"></audio>
          {:else if kind === "text"}
            {#if text?.id === f._id}
              <pre data-testid="text-preview">{text.body}{text.truncated ? "\n…" : ""}</pre>
            {:else}
              <span class="faint">Loading preview…</span>
            {/if}
          {:else}
            <span class="faint">No preview for {f.contentType ?? "files without a content type"}</span>
          {/if}
        </div>
        <dl>
          <dt>Id</dt>
          <dd class="mono">{f._id}</dd>
          <dt>Type</dt>
          <dd class="mono">{f.contentType ?? "—"}</dd>
          <dt>Size</dt>
          <dd>{formatBytes(f.size)} <span class="faint">({f.size.toLocaleString()} bytes)</span></dd>
          <dt>SHA-256</dt>
          <dd class="mono hash">{f.sha256}</dd>
          <dt>Created</dt>
          <dd>{formatTime(f._creationTime)}</dd>
          <dt>Created by</dt>
          <dd class="mono">{f.createdBy ?? "—"}</dd>
          <dt>URL</dt>
          <dd class="mono url">{f.url}</dd>
        </dl>
        <div class="row">
          <a href={fileUrl(f)} target="_blank" rel="noopener noreferrer">Open</a>
          <a href={fileUrl(f, true)} download={f.filename ?? ""}>Download</a>
          <span class="spacer"></span>
          <button class="small danger" onclick={() => remove(f)}>Delete</button>
        </div>
      </aside>
    {:else if selectedId && selectedQuery.data === null}
      <aside class="panel" aria-label="File details">
        <header>
          <h2>Not found</h2>
          <button class="small ghost" onclick={() => select(null)} aria-label="Close">✕</button>
        </header>
        <p class="muted">No file <span class="mono">{selectedId}</span>; it may have been deleted.</p>
      </aside>
    {/if}
  </div>

  {#if dragging}
    <div class="drop" aria-hidden="true"><div>Drop to upload</div></div>
  {/if}
</div>

<style>
  .page {
    position: relative;
  }
  .hint {
    font-size: 12.5px;
  }
  .layout {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-columns: 1fr;
  }
  .layout.with-panel {
    grid-template-columns: 1fr minmax(320px, 400px);
  }
  .body {
    overflow: auto;
    min-width: 0;
  }
  .thumb {
    width: 52px;
  }
  .thumb img {
    display: block;
    width: 36px;
    height: 36px;
    object-fit: cover;
    border-radius: 4px;
    background: var(--panel-2);
  }
  .thumb .icon {
    display: inline-grid;
    place-items: center;
    width: 36px;
    height: 36px;
    border-radius: 4px;
    background: var(--panel-2);
    color: var(--faint);
  }
  td.name {
    font-weight: 500;
    max-width: 260px;
  }
  .actions {
    display: flex;
    gap: 10px;
    align-items: center;
  }
  td.by {
    max-width: 140px;
  }
  tbody td {
    vertical-align: middle;
  }
  .more {
    padding: 12px 20px;
  }
  .uploads {
    list-style: none;
    margin: 0;
    padding: 8px 20px;
    border-bottom: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .uploads li {
    display: grid;
    grid-template-columns: minmax(120px, 280px) 1fr auto;
    gap: 12px;
    align-items: center;
  }
  .uploads .name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  progress {
    width: 100%;
    accent-color: var(--accent);
  }
  .panel {
    border-left: 1px solid var(--border);
    background: var(--panel);
    overflow: auto;
    padding: 14px 16px;
    display: flex;
    flex-direction: column;
    gap: 14px;
    min-width: 0;
  }
  .panel header {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .panel h2 {
    flex: 1;
    margin: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .preview {
    display: flex;
    justify-content: center;
    align-items: center;
    min-height: 120px;
    background: var(--panel-2);
    border-radius: var(--radius);
    padding: 8px;
  }
  .preview img,
  .preview video {
    max-width: 100%;
    max-height: 320px;
    border-radius: 4px;
  }
  .preview audio {
    width: 100%;
  }
  .preview pre {
    margin: 0;
    width: 100%;
    max-height: 320px;
    overflow: auto;
    font-size: 12px;
    white-space: pre-wrap;
    word-break: break-word;
  }
  dl {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 6px 12px;
    margin: 0;
    font-size: 13px;
  }
  dt {
    color: var(--muted);
  }
  dd {
    margin: 0;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .hash,
  .url {
    font-size: 11.5px;
  }
  .drop {
    position: absolute;
    inset: 0;
    display: grid;
    place-items: center;
    background: color-mix(in srgb, var(--accent-soft) 80%, transparent);
    border: 2px dashed var(--accent);
    pointer-events: none;
    z-index: 10;
  }
  .drop div {
    font-size: 18px;
    font-weight: 600;
    color: var(--accent-text);
  }
</style>
