<script lang="ts">
  let {
    autoApply = true,
    autoReapply = true,
    backendPreference = "Auto",
    onAutoApplyChange = (_v: boolean) => {},
    onAutoReapplyChange = (_v: boolean) => {},
    onBackendChange = (_v: string) => {},
  }: {
    autoApply: boolean;
    autoReapply: boolean;
    backendPreference: string;
    onAutoApplyChange: (v: boolean) => void;
    onAutoReapplyChange: (v: boolean) => void;
    onBackendChange: (v: string) => void;
  } = $props();

  let expanded = $state(false);
</script>

<div class="section">
  <button class="toggle-btn" onclick={() => (expanded = !expanded)}>
    <h2>设置</h2>
    <span class="arrow" class:open={expanded}>▸</span>
  </button>

  {#if expanded}
    <div class="settings">
      <label class="row">
        <span>启动时自动应用设置</span>
        <input type="checkbox" checked={autoApply} onchange={(e) => onAutoApplyChange((e.target as HTMLInputElement).checked)} />
      </label>

      <label class="row">
        <span>电源切换时自动重应用</span>
        <input type="checkbox" checked={autoReapply} onchange={(e) => onAutoReapplyChange((e.target as HTMLInputElement).checked)} />
      </label>

      <div class="row">
        <span>EC 后端</span>
        <select
          value={backendPreference}
          onchange={(e) => onBackendChange((e.target as HTMLSelectElement).value)}
        >
          <option value="Auto">自动</option>
          <option value="Wmi">WMI</option>
          <option value="WinRing0">WinRing0</option>
        </select>
      </div>
    </div>
  {/if}
</div>

<style>
  .section {
    padding: 16px;
    background: var(--surface);
  }
  .toggle-btn {
    display: flex;
    align-items: center;
    justify-content: space-between;
    width: 100%;
  }
  h2 {
    font-size: 14px;
    font-weight: 600;
  }
  .arrow {
    font-size: 12px;
    transition: transform var(--transition);
    color: var(--text-secondary);
  }
  .arrow.open {
    transform: rotate(90deg);
  }
  .settings {
    margin-top: 12px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    font-size: 13px;
  }
  .row select {
    font-family: inherit;
    font-size: 13px;
    padding: 4px 8px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg);
    color: var(--text);
  }
</style>
