<script lang="ts">
  import Icon from "./Icon.svelte";
  import { rankCommands, type Command } from "../lib/commands.js";

  /** Ctrl+K / ⌘K: jump to a page or a table, or run a command, by typing a few letters. */
  let { commands, onclose }: { commands: Command[]; onclose: () => void } = $props();

  let query = $state("");
  let active = $state(0);
  let input: HTMLInputElement | undefined = $state();
  let list: HTMLUListElement | undefined = $state();
  const results = $derived(rankCommands(query, commands, 40));

  $effect(() => {
    input?.focus();
  });
  $effect(() => {
    void query;
    active = 0;
  });
  $effect(() => {
    list?.querySelector(`[data-index="${active}"]`)?.scrollIntoView({ block: "nearest" });
  });

  function choose(c: Command | undefined) {
    if (!c) return;
    onclose();
    c.run();
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      active = Math.min(results.length - 1, active + 1);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      active = Math.max(0, active - 1);
    } else if (e.key === "Enter") {
      e.preventDefault();
      choose(results[active]);
    } else if (e.key === "Escape") {
      e.preventDefault();
      onclose();
    }
  }
</script>

<div class="backdrop" role="presentation" onclick={onclose}>
  <div class="palette" role="dialog" aria-modal="true" aria-label="Command palette" tabindex="-1" onclick={(e) => e.stopPropagation()} onkeydown={() => {}}>
    <div class="search">
      <Icon name="search" size={18} />
      <input
        bind:this={input}
        bind:value={query}
        {onkeydown}
        placeholder="Go to a page or table, or run a command…"
        aria-label="Command"
        role="combobox"
        aria-expanded="true"
        aria-controls="palette-list"
        aria-activedescendant={results[active] ? `cmd-${active}` : undefined}
        autocomplete="off"
        spellcheck="false"
      />
    </div>
    <ul id="palette-list" role="listbox" bind:this={list}>
      {#each results as c, i (c.id)}
        <li
          id={`cmd-${i}`}
          role="option"
          aria-selected={i === active}
          class:active={i === active}
          data-index={i}
          onclick={() => choose(c)}
          onkeydown={() => {}}
          onpointermove={() => (active = i)}
        >
          <span class="label">{c.label}</span>
          <span class="group">{c.group}</span>
          {#if c.shortcut}<kbd>{c.shortcut}</kbd>{/if}
        </li>
      {:else}
        <li class="none">No matches</li>
      {/each}
    </ul>
    <div class="foot"><span><kbd>↑</kbd> <kbd>↓</kbd> to move</span><span><kbd>↵</kbd> to open</span><span><kbd>Esc</kbd> to close</span></div>
  </div>
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgb(0 0 0 / 0.3);
    display: flex;
    justify-content: center;
    align-items: flex-start;
    padding-top: 12vh;
    z-index: 200;
  }
  .palette {
    width: min(560px, calc(100vw - 32px));
    background: var(--panel);
    border: 1px solid var(--border-strong);
    border-radius: 10px;
    box-shadow: var(--shadow);
    overflow: hidden;
    display: flex;
    flex-direction: column;
  }
  .search {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 0 16px;
    border-bottom: 1px solid var(--border);
    color: var(--muted);
  }
  input {
    flex: 1;
    border: none;
    border-radius: 0;
    padding: 15px 0;
    font-size: 16px;
    background: transparent;
    color: var(--text);
  }
  input:focus {
    outline: none;
    box-shadow: none;
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 6px;
    max-height: 340px;
    overflow: auto;
  }
  li {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 7px 10px;
    border-radius: var(--radius);
    cursor: pointer;
  }
  li.active {
    background: var(--accent-soft);
    color: var(--accent-text);
  }
  li.active .group {
    color: inherit;
    opacity: 0.8;
  }
  .label {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .group {
    font-size: 12px;
    color: var(--muted);
  }
  li.none {
    color: var(--muted);
    cursor: default;
  }
  .foot {
    display: flex;
    gap: 16px;
    font-size: 12px;
    color: var(--muted);
    padding: 8px 16px;
    border-top: 1px solid var(--border);
    background: var(--panel-2);
  }
</style>
