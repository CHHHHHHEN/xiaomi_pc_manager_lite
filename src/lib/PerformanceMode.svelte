<script lang="ts">
  let {
    mode = 0x09,
    onChange = (_v: number) => {},
  }: {
    mode: number;
    onChange: (v: number) => void;
  } = $props();

  const modes = [
    { value: 0x0A, label: "Eco", icon: "EC", desc: "节能" },
    { value: 0x02, label: "Quiet", icon: "Q", desc: "静音" },
    { value: 0x09, label: "Smart", icon: "S", desc: "智能" },
    { value: 0x03, label: "Fast", icon: "F", desc: "极速" },
    { value: 0x04, label: "Extreme", icon: "X", desc: "狂暴" },
  ];
</script>

<div class="section">
  <h2>性能模式</h2>
  <div class="grid">
    {#each modes as m}
      <button
        class="card"
        class:active={mode === m.value}
        onclick={() => onChange(m.value)}
      >
        <span class="icon">{m.icon}</span>
        <span class="label">{m.label}</span>
        <span class="desc">{m.desc}</span>
      </button>
    {/each}
  </div>
</div>

<style>
  .section {
    padding: 16px;
    background: var(--surface);
    border-bottom: 1px solid var(--border);
  }
  h2 {
    font-size: 14px;
    font-weight: 600;
    margin-bottom: 12px;
  }
  .grid {
    display: grid;
    grid-template-columns: repeat(5, 1fr);
    gap: 8px;
  }
  .card {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 4px;
    padding: 12px 4px;
    border-radius: var(--radius);
    background: var(--bg);
    transition: var(--transition);
    border: 2px solid transparent;
  }
  .card:hover {
    background: var(--surface-hover);
  }
  .card.active {
    border-color: var(--accent);
    background: #e8f0fe;
  }
  .icon {
    font-size: 18px;
    font-weight: 700;
    width: 32px;
    height: 32px;
    display: flex;
    align-items: center;
    justify-content: center;
    border-radius: 50%;
    background: var(--surface);
    color: var(--accent);
    border: 1px solid var(--border);
  }
  .card.active .icon {
    background: var(--accent);
    color: white;
    border-color: var(--accent);
  }
  .label {
    font-size: 11px;
    font-weight: 600;
  }
  .desc {
    font-size: 10px;
    color: var(--text-secondary);
  }
</style>
