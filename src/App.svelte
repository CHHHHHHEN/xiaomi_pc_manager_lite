<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import TitleBar from "./lib/TitleBar.svelte";
  import EcStatus from "./lib/EcStatus.svelte";
  import BatteryCare from "./lib/BatteryCare.svelte";
  import PerformanceMode from "./lib/PerformanceMode.svelte";
  import SettingsPanel from "./lib/SettingsPanel.svelte";
  import "./styles/global.css";

  interface StatusResponse {
    backend: string;
    available: boolean;
    battery_care_enabled: boolean;
    charge_limit: number;
    performance_mode: number;
  }

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
      const s = await invoke<StatusResponse>("get_status");
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

  function getNextPerfMode(current: number): number {
    const modes = [0x02, 0x09, 0x03, 0x04, 0x0A]; // Quiet, Smart, Fast, Extreme, Eco
    const idx = modes.indexOf(current);
    return modes[(idx + 1) % modes.length];
  }

  onMount(() => {
    loadStatus();

    const unlisteners: Array<() => void> = [];

    listen("tray-toggle-battery-care", () => {
      handleBatteryToggle(!batteryCareEnabled);
    }).then((u) => unlisteners.push(u));

    listen<string>("tray-set-perf-mode", (e) => {
      const modeName = e.payload.toLowerCase();
      const modeMap: Record<string, number> = {
        eco: 0x0A, quiet: 0x02, smart: 0x09, fast: 0x03, extreme: 0x04,
      };
      if (modeName in modeMap) {
        handlePerfModeChange(modeMap[modeName]);
      }
    }).then((u) => unlisteners.push(u));

    listen("hotkey-toggle-battery-care", () => {
      handleBatteryToggle(!batteryCareEnabled);
    }).then((u) => unlisteners.push(u));

    listen("hotkey-cycle-perf-mode", () => {
      handlePerfModeChange(getNextPerfMode(perfMode));
    }).then((u) => unlisteners.push(u));

    return () => unlisteners.forEach((u) => u());
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
