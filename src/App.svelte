<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onMount } from "svelte";
  import TitleBar from "./lib/TitleBar.svelte";
  import EcStatus from "./lib/EcStatus.svelte";
  import BatteryCare from "./lib/BatteryCare.svelte";
  import PerformanceMode from "./lib/PerformanceMode.svelte";
  import SettingsPanel from "./lib/SettingsPanel.svelte";
  import "./styles/global.css";

  let backend = $state("");
  let available = $state(false);
  let batteryCareEnabled = $state(false);
  let chargeLimit = $state(80);
  let perfMode = $state(0x09);
  let autoApply = $state(true);
  let autoReapply = $state(true);
  let backendPref = $state("Auto");

  async function loadStatus() {
    try {
      const s: any = await invoke("get_status");
      backend = s.backend;
      available = s.available;
      batteryCareEnabled = s.battery_care_enabled;
      chargeLimit = s.charge_limit;
      perfMode = s.performance_mode;
    } catch (e) {
      console.error("get_status failed", e);
    }
  }

  async function handleBatteryToggle(v: boolean) {
    batteryCareEnabled = v;
    try {
      await invoke("set_battery_care", { enabled: v });
    } catch (e) {
      console.error(e);
      await loadStatus();
    }
  }

  async function handleLimitChange(v: number) {
    chargeLimit = v;
    try {
      await invoke("set_charge_limit", { percent: v });
    } catch (e) {
      console.error(e);
      await loadStatus();
    }
  }

  async function handlePerfModeChange(v: number) {
    perfMode = v;
    try {
      await invoke("set_performance_mode", { mode: v });
    } catch (e) {
      console.error(e);
      await loadStatus();
    }
  }

  async function handleSaveConfig() {
    try {
      await invoke("save_config", {
        config: {
          battery_care_enabled: batteryCareEnabled,
          battery_charge_limit: chargeLimit,
          performance_mode: perfMode,
          auto_apply_on_startup: autoApply,
          auto_reapply_on_power_change: autoReapply,
          backend: backendPref,
          window_visible: true,
        },
      });
    } catch (e) {
      console.error(e);
    }
  }

  onMount(() => {
    loadStatus();
  });
</script>

<TitleBar />
<EcStatus {backend} {available} />
<BatteryCare
  enabled={batteryCareEnabled}
  chargeLimit={chargeLimit}
  onToggle={handleBatteryToggle}
  onLimitChange={handleLimitChange}
/>
<PerformanceMode mode={perfMode} onChange={handlePerfModeChange} />
<SettingsPanel
  autoApply={autoApply}
  autoReapply={autoReapply}
  backendPreference={backendPref}
  onAutoApplyChange={(v) => { autoApply = v; handleSaveConfig(); }}
  onAutoReapplyChange={(v) => { autoReapply = v; handleSaveConfig(); }}
  onBackendChange={(v) => { backendPref = v; handleSaveConfig(); }}
/>
