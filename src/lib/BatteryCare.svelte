<script lang="ts">
  let {
    enabled = false,
    chargeLimit = 80,
    onToggle = (_v: boolean) => {},
    onLimitChange = (_v: number) => {},
  }: {
    enabled: boolean;
    chargeLimit: number;
    onToggle: (v: boolean) => void;
    onLimitChange: (v: number) => void;
  } = $props();
</script>

<div class="section">
  <div class="section-header">
    <h2>电池养护</h2>
    <label class="switch">
      <input type="checkbox" checked={enabled} onchange={(e) => onToggle((e.target as HTMLInputElement).checked)} />
      <span class="slider"></span>
    </label>
  </div>

  <div class="slider-row">
    <span class="label">充电上限</span>
    <span class="value">{chargeLimit}%</span>
  </div>
  <input
    type="range"
    min="40"
    max="100"
    step="10"
    value={chargeLimit}
    disabled={!enabled}
    oninput={(e) => onLimitChange(Number((e.target as HTMLInputElement).value))}
  />

  <div class="hint">建议日常使用设置在 80%，可延长电池寿命</div>
</div>

<style>
  .section {
    padding: 16px;
    background: var(--surface);
    border-bottom: 1px solid var(--border);
  }
  .section-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 12px;
  }
  h2 {
    font-size: 14px;
    font-weight: 600;
  }
  .switch {
    position: relative;
    display: inline-block;
    width: 40px;
    height: 22px;
  }
  .switch input {
    opacity: 0;
    width: 0;
    height: 0;
  }
  .slider {
    position: absolute;
    cursor: pointer;
    inset: 0;
    background: var(--border);
    border-radius: 11px;
    transition: var(--transition);
  }
  .slider::before {
    content: "";
    position: absolute;
    width: 18px;
    height: 18px;
    left: 2px;
    bottom: 2px;
    background: white;
    border-radius: 50%;
    transition: var(--transition);
  }
  .switch input:checked + .slider {
    background: var(--accent);
  }
  .switch input:checked + .slider::before {
    transform: translateX(18px);
  }
  .switch input:disabled + .slider {
    opacity: 0.5;
  }
  .slider-row {
    display: flex;
    justify-content: space-between;
    margin-bottom: 8px;
  }
  .label {
    font-size: 13px;
    color: var(--text-secondary);
  }
  .value {
    font-size: 13px;
    font-weight: 600;
    color: var(--accent);
  }
  .hint {
    margin-top: 8px;
    font-size: 11px;
    color: var(--text-secondary);
  }
</style>
