# Remaining Works

## ✅ 已完成

### Rust 后端
- [x] `ec/backend.rs` — 添加 `is_available()` 方法
- [x] `ec/winring0.rs` — 删除重复 import，添加高电平方法（get_battery_care_enabled, set_charge_limit, get_performance_mode, set_performance_mode）
- [x] `ec/wmi.rs` — 完整 WMI COM 实现（CoInitializeEx → IWbemLocator → ConnectServer(root\wmi) → GetObject(MICommonInterface) → ExecMethod(MiInterface, 32-byte buffer)）
- [x] `ec/battery.rs` — BatteryStatus 结构体、WMI rawCode ⇔ 百分比映射、nearest_wmi_percent
- [x] `ec/performance.rs` — PerfMode 枚举（Eco/Quiet/Smart/Fast/Extreme）+ name/from_ec_value/is_valid
- [x] `ec/config.rs` — 添加 performance_mode、auto_apply_on_startup、auto_reapply_on_power_change 字段
- [x] `commands.rs` — 7 个 Tauri Command（get_backend_name, get_status, set_battery_care, set_charge_limit, set_performance_mode, get_config, save_config）
- [x] `lib.rs` — 注册所有命令，启动时 apply_startup_config + 从 EC 读取当前值同步到 config
- [x] `ec/mod.rs` — 添加 battery、performance、commands 模块导出

### 前端
- [x] `src/styles/global.css` — Fluent Design 风格全局样式
- [x] `src/lib/TitleBar.svelte` — 自定义标题栏（最小化/关闭按钮 + drag-region）
- [x] `src/lib/EcStatus.svelte` — 后端名称 + 在线状态指示灯
- [x] `src/lib/BatteryCare.svelte` — 电池养护开关 + 充电阈值滑条
- [x] `src/lib/PerformanceMode.svelte` — 5 种性能模式卡片选择
- [x] `src/lib/SettingsPanel.svelte` — 开机自启/电源重应用/后端选择
- [x] `src/App.svelte` — 组合所有组件，统一状态管理

### 验证
- [x] `cargo check -p xiaomi-pc-manager-lite` — 通过
- [x] `npx svelte-check` — 0 errors, 0 warnings
- [x] `pnpm run build` — Vite production build 成功

---

## ⏳ 待完成

### 中优先级 ✅ 已完成

#### tray.rs — 系统托盘
- [x] 托盘图标 + 右键菜单
- [x] 菜单项：电池养护开关、性能模式切换、显示/隐藏窗口、退出
- [x] Tauri v2 `TrayIconBuilder` API

#### hotkey.rs — 全局快捷键
- [x] Windows `RegisterHotKey` API（已添加 `Win32_UI_Input_KeyboardAndMouse` feature）
- [x] `Ctrl+Alt+B` 切换电池养护（发送 hotkey-toggle-battery-care 事件到前端）
- [x] `Ctrl+Alt+P` 循环切换性能模式（发送 hotkey-cycle-perf-mode 事件到前端）

#### power_event.rs — 电源监听
- [x] 消息窗口监听 `WM_POWERBROADCAST` / `PBT_APMPOWERSTATUSCHANGE`
- [x] 监听电源状态变化 → 自动重应用配置（auto_reapply_on_power_change）
- [x] 集成 `tray::sync_tray_state` 同步托盘状态

#### WinRing0 DLL/SYS 嵌入 + 运行时释放
- [x] 使用 `rust-embed` 将 `WinRing0x64.dll` 嵌入 exe
- [x] 启动时释放到临时目录（`%TEMP%\XiaomiPcManagerLite\bin\`）
- [x] 进程退出时清理（`embed::cleanup_temp()`）

---

## WMI 注意事项（Windows 测试时才可验证）

- [ ] `IWbemServices::ExecMethod` 的实际签名和参数需在 Windows 上验证
- [ ] MiInterface 的 32 字节命令/响应格式需要 Windows 验证
- [ ] `SafeArrayCreateVector` + `VARIANT` 构造需要 Windows 验证
- [ ] WMI `root\wmi` 命名空间和 `MICommonInterface` 类需要确认存在
- [ ] `CoSetProxyBlanket` 参数可能需要根据系统调整
- [ ] 启动时 `CoInitializeEx` 在 `WmiBackend::new()` 中调用，没有对应的 `CoUninitialize`

---

## PerfMode 映射表

| 模式 | EC 值 (0x68) | WMI fun2 |
|------|-------------|----------|
| Eco | `0x0A` | `0x0800` |
| Quiet | `0x02` | `0x0800` |
| Smart | `0x09` | `0x0800` |
| Fast | `0x03` | `0x0800` |
| Extreme | `0x04` | `0x0800` |

## WMI MiInterface 命令格式（参考）

- 32 字节输入缓冲区（little-endian）
  - `[0-1]` 命令: `0xFA00` (read), `0xFB00` (write)
  - `[2-3]` fun2: `0x0800` (perf mode), `0x1000` (battery)
  - `[4-5]` rawCode
  - `[6-7]` value (write only)
- 32 字节输出缓冲区: `[4-7]` 结果数据

### WMI 充电限制 rawCode 映射

| rawCode | 充电限制 |
|---------|---------|
| `0` | 100% |
| `1` | 80% |
| `4` | 90% |
| `5` | 70% |
| `6` | 60% |
| `7` | 50% |
| `8` | 40% |
