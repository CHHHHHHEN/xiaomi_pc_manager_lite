<script lang="ts">
  import { getCurrentWindow } from "@tauri-apps/api/window";

  let win = $state(getCurrentWindow());
</script>

<div class="titlebar" data-tauri-drag-region>
  <span class="title">Xiaomi PC Manager Lite</span>
  <div class="controls">
    <button class="control-btn" aria-label="Minimize" onclick={() => win.minimize()}>
      <svg width="12" height="12" viewBox="0 0 12 12"><rect y="5" width="12" height="1.5" fill="currentColor"/></svg>
    </button>
    <button class="control-btn" aria-label="Close" onclick={() => win.close()}>
      <svg width="12" height="12" viewBox="0 0 12 12">
        <path d="M1 1l10 10M11 1L1 11" stroke="currentColor" stroke-width="1.5" fill="none"/>
      </svg>
    </button>
  </div>
</div>

<style>
  .titlebar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    height: 36px;
    padding: 0 12px;
    background: var(--surface);
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }
  .title {
    font-size: 13px;
    font-weight: 600;
    color: var(--text);
  }
  .controls {
    display: flex;
    gap: 4px;
  }
  .control-btn {
    width: 32px;
    height: 24px;
    display: flex;
    align-items: center;
    justify-content: center;
    border-radius: 4px;
    color: var(--text-secondary);
  }
  .control-btn:hover {
    background: var(--surface-hover);
    color: var(--text);
  }
</style>
