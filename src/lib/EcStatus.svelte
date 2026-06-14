<script lang="ts">
  let { backend = "", available = false }: { backend: string; available: boolean } = $props();
</script>

<div class="status">
  <span class="dot" class:online={available} class:offline={!available}></span>
  <span class="label">{backend}</span>
</div>

<style>
  .status {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 16px;
    background: var(--surface);
    border-bottom: 1px solid var(--border);
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .online { background: var(--success); }
  .offline { background: var(--danger); }
  .label {
    font-size: 12px;
    color: var(--text-secondary);
  }
</style>
