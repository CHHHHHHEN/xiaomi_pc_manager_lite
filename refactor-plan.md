# Xiaomi PC Manager Lite — Rust + Tauri 重构计划

## 1. 项目概览

| 项目 | 说明 |
|------|------|
| 当前状态 | C++ Win32 单文件 (903 行)，WinRing0 驱动 EC 通信 |
| 目标状态 | Rust + Tauri v2 重写，双后端：WinRing0 直读 + WMI MIInterface |
| 核心功能 | 电池养护开关/阈值、性能模式切换、系统托盘、全局快捷键、配置持久化 |
| 分发形式 | **单一 exe**（嵌入 WinRing0 DLL/SYS，无需安装） |
| 测试硬件 | Redmibook Pro 14 2025 (Ultra 7 255H) — EC 端口 `0x62/0x66` |
| 作者 | CHHHHHHEN |

---

## 2. 技术栈

| 层级 | 技术 | 用途 |
|------|------|------|
| **语言** | Rust (edition 2024) | 后端核心逻辑 |
| **桌面框架** | Tauri v2 | 窗口管理 + WebView2 前端 |
| **前端** | Svelte 5 (或 React/Vue) | GUI 界面 |
| **系统托盘** | `tray-icon` (tauri-apps 维护) | 托盘图标 + 右键菜单 |
| **全局快捷键** | `global-hotkey` (tauri-apps 维护) | `Ctrl+Alt+B` / `Ctrl+Alt+P` |
| **EC 后端 A** | `libloading` + WinRing0 DLL | 直接 I/O 端口 0x62/0x66 读写 EC |
| **EC 后端 B** | `windows` crate + COM (WMI) | 通过 `MICommonInterface.MiInterface` 方法控制 |
| **配置序列化** | `serde` + `toml` | 配置文件读写 |
| **Win32 API** | `windows` crate | WS_EX_LAYERED 透明度、WMI COM、电源事件等 |
| **文件嵌入** | `rust-embed` | 将 WinRing0 DLL/SYS 嵌入 exe |
| **安装包** | 无（单 exe 分发） | 通过 `cargo tauri build --bundles none` 生成 |
| **可选压缩** | `upx` | UPX --lzma 压缩最终 exe |
| **构建** | Cargo + Tauri CLI | 开发/发布构建 |

### 前端选择

推荐 **Svelte 5**（runes 语法），原因：
- 编译后体积极小（~5KB runtime），适合系统工具
- 无虚拟 DOM，性能高
- 单文件组件（Svelte 文件 = HTML+CSS+JS）维护简单
- 学习曲线低

备选：React (生态最大)、Vue (中文文档好)、Solid (性能极致)

---

## 3. 架构设计

```
┌─────────────────────────────────────────────────────────────┐
│                     Tauri WebView2 窗口                       │
│  ┌───────────────────────────────────────────────────────┐  │
│  │                    Svelte 前端                         │  │
│  │  ┌──────────┐  ┌──────────┐  ┌──────────────────┐   │  │
│  │  │ 电池养护  │  │ 性能模式  │  │ EC 状态面板      │   │  │
│  │  │ 面板      │  │ 面板      │  │ (只读状态显示)   │   │  │
│  │  └─────┬────┘  └────┬─────┘  └────────┬─────────┘   │  │
│  │        └────────────┼─────────────────┘              │  │
│  │                     │ Tauri invoke                    │  │
│  │                     ▼                                 │  │
│  └─────────────────────┼─────────────────────────────────┘  │
└────────────────────────┼────────────────────────────────────┘
                         │
┌────────────────────────┼────────────────────────────────────┐
│              Tauri Rust 后端 (src-tauri)                     │
│                         │                                    │
│  ┌──────────────────────┴────────────────────────────┐      │
│  │              Tauri Commands (API 层)               │      │
│  │  get_status / set_battery_care / set_perf_mode     │      │
│  │  get_config / save_config                          │      │
│  └─────┬──────────────┬────────────────┬──────────────┘      │
│        │              │                │                     │
│  ┌─────▼─────────┐  ┌─▼────────┐  ┌───▼───────────┐         │
│  │  EC Abstraction│  │ Config   │  │ Tray Manager  │         │
│  │  (trait)       │  │ (serde + │  │ (tray-icon)   │         │
│  │                │  │  toml)   │  │               │         │
│  └──────┬─────────┘  └──────────┘  └───────────────┘         │
│         │                                                     │
│    ┌────┴────┐                                      │
│    │         │                                                    │
│  ┌──▼─────┐ ┌▼──────────┐                                        │
│  │WinRing0│ │WMI Backend│  ← 二选一：运行时自动检测或配置指定      │
│  │Backend │ │(MIInter-  │                                        │
│  │(FFI)   │ │ face)     │                                        │
│  └──┬─────┘ └──┬────────┘                                        │
│     │          │                                                  │
│     ▼          ▼                                                  │
│  I/O ports  WMI COM       │
│  0x62/0x66  \\.\root\wmi                                        │
│             MICommonInterface                                    │
│                                                                  │
│  ┌───────────────────────────────────────────────────┐           │
│  │  plugin: tray-icon (系统托盘)                       │           │
│  │  plugin: global-hotkey (Ctrl+Alt+B/P)             │           │
│  └───────────────────────────────────────────────────┘           │
└──────────────────────────────────────────────────────────────────┘
```

### 数据流

```
用户操作 → Svelte 组件 → tauri::invoke() → Rust Command
    → EC Abstraction trait (自动选择后端)
      ├── [后端 A] WinRing0Backend → WinRing0 DLL → I/O 端口 0x62/0x66
      └── [后端 B] WmiBackend → WMI COM → \\.\root\wmi:MICommonInterface
    → 返回结果 → 前端更新 UI
```

**系统事件流**：
```
电源变更 → WM_POWERBROADCAST (通过 windows crate 监听)
    → Rust 检测 → invoke 通知前端 → 自动重应用设置
```

**后端选择策略**：
1. 启动时先尝试 WMI 后端（`MICommonInterface` 检测）
2. WMI 可用则优先使用 WMI（无需内核驱动签名）
3. WMI 不可用则回退到 WinRing0（需管理员 + 驱动签名）
4. 配置文件可强制指定后端：`backend = "winring0" | "wmi"`

---

## 4. 项目目录结构

```
xiaomi_pc_manager_lite/
│
├── src/                              # ← 前端 (Svelte)
│   ├── App.svelte                    # 根组件
│   ├── main.ts                       # 前端入口
│   ├── lib/
│   │   ├── BatteryCare.svelte        # 电池养护面板
│   │   ├── PerformanceMode.svelte    # 性能模式面板
│   │   ├── EcStatus.svelte           # EC 状态面板
│   │   ├── TitleBar.svelte           # 自定义标题栏
│   │   └── SettingsPanel.svelte      # 设置面板 (开机自启/电源变动)
│   ├── stores/
│   │   └── app.ts                    # 前端状态管理
│   └── styles/
│       └── global.css                # 全局样式
│
├── src-tauri/                        # ← Rust 后端
│   ├── Cargo.toml
│   ├── tauri.conf.json               # Tauri 配置
│   ├── capabilities/
│   │   └── default.json
│   ├── icons/                        # 应用图标
│   │   ├── icon.ico
│   │   └── icon.png
│   └── src/
│       ├── main.rs                   # Tauri 入口 + setup
│       ├── commands.rs               # Tauri Command 定义 (API 层)
│       ├── ec/
│       │   ├── mod.rs                # EC 模块入口 + trait 定义
│       │   ├── backend.rs            # EcBackend trait (抽象接口)
│       │   ├── winring0.rs           # 后端 A: WinRing0 FFI 绑定
│       │   ├── wmi.rs                # 后端 B: WMI MIInterface 实现
│       │   ├── battery.rs            # 电池养护逻辑 (泛型于 trait)
│       │   └── performance.rs        # 性能模式逻辑 (泛型于 trait)
│       ├── config.rs                 # 配置读写 (serde + toml)
│       ├── tray.rs                   # 系统托盘管理
│       ├── hotkey.rs                 # 全局快捷键
│       ├── power_event.rs            # 电源状态变化监听
│       └── error.rs                  # 统一错误类型
│
├── WinRing0/                         # ← 编译时嵌入到 exe 的资源
│   ├── WinRing0x64.dll               # 预编译 DLL (64位) → 嵌入为资源
│   ├── WinRing0.dll                  # 预编译 DLL (32位) → 嵌入为资源
│   ├── WinRing0x64.sys               # 驱动程序 (64位) → 嵌入为资源
│   └── WinRing0.sys                  # 驱动程序 (32位) → 嵌入为资源
│
├── wix/                              # MSI 安装包配置
│   └── main.wxs
│
├── package.json                      # 前端依赖
├── svelte.config.js
├── vite.config.ts
├── tsconfig.json
├── refactor-plan.md                  # 本文件
└── README.md                         # 更新后的 README
```

---

## 5. 模块详细设计

### 5.1 Rust 后端模块

#### `ec/backend.rs` — EcBackend trait（抽象层核心）

两种后端的共同接口。Tauri Command 层只依赖这个 trait，不关心具体后端。

```rust
/// 抽象的 EC 通信后端
pub trait EcBackend: Send + Sync {
    /// 读取 EC 寄存器
    fn read_ec(&self, address: u16) -> Result<u8, EcError>;
    /// 写入 EC 寄存器
    fn write_ec(&self, address: u16, value: u8) -> Result<(), EcError>;
    /// 后端是否可用
    fn is_available(&self) -> bool;
    /// 后端名称（用于日志和 UI 显示）
    fn name(&self) -> &'static str;
}
```

#### `ec/winring0.rs` — 后端 A：WinRing0 FFI 绑定

```rust
// 功能：动态加载 WinRing0 DLL，通过 I/O 端口 0x62/0x66 直接操作 EC
// 安全策略：unsafe 封装在模块内，对外暴露 safe 方法

pub struct WinRing0Backend {
    lib: Library,          // libloading::Library
    initialize_ols: unsafe extern "system" fn() -> BOOL,
    deinitialize_ols: unsafe extern "system" fn(),
    read_io_port_byte: unsafe extern "system" fn(port: u16) -> u8,
    write_io_port_byte: unsafe extern "system" fn(port: u16, value: u8),
    initialized: bool,
}

impl EcBackend for WinRing0Backend {
    fn read_ec(&self, address: u16) -> Result<u8, EcError>;
    fn write_ec(&self, address: u16, value: u8) -> Result<(), EcError>;
    fn is_available(&self) -> bool;
    fn name(&self) -> &'static str { "WinRing0 (I/O Port)" }
}
```

**EC 端口常量**：
- `EC_CMD_PORT = 0x66`
- `EC_DATA_PORT = 0x62`
- `EC_READ_CMD = 0x80`
- `EC_WRITE_CMD = 0x81`

**读取 EC 流程**：
```
WaitECReady → WriteIoPortByte(0x66, 0x80) → WaitECReady → WriteIoPortByte(0x62, addr)
→ WaitECReady → ReadIoPortByte(0x62) → 返回
```

**写入 EC 流程**：
```
WaitECReady → WriteIoPortByte(0x66, 0x81) → WaitECReady → WriteIoPortByte(0x62, addr)
→ WaitECReady → WriteIoPortByte(0x62, value)
```

#### `ec/wmi.rs` — 后端 B：WMI MIInterface 实现

```rust
// 功能：通过 Xiaomi 厂商定义的 WMI MICommonInterface.MiInterface 方法控制 EC
// 参考自 Meow-Box 项目 (BatteryControlService.cs)
// 无需内核驱动，但需管理员权限

pub struct WmiBackend {
    // WMI COM 通过 windows crate 的 ManagementObject 实现
    // 无需直接使用 IWbemLocator，用 windows crate 的 WMI API
    service: WmiService,   // 由 windows crate 管理
}

impl EcBackend for WmiBackend {
    fn read_ec(&self, address: u16) -> Result<u8, EcError>;
    fn write_ec(&self, address: u16, value: u8) -> Result<(), EcError>;
    fn is_available(&self) -> bool;
    fn name(&self) -> &'static str { "WMI (MICommonInterface)" }
}
```

**WMI 协议细节**（源自 Meow-Box 的 `BatteryControlService.cs`）：

| 项 | 值 |
|------|------|
| WMI 命名空间 | `\\.\root\wmi` |
| WMI 类 | `MICommonInterface` |
| WMI 方法 | `MiInterface` |
| 实例筛选 | `InstanceName` 包含 `"MIFS"` |
| 输入/输出 | 32 字节缓冲区 (byte[32]) |

**命令格式（32 字节缓冲区）**：

| 偏移 | 类型 | 字段 | 说明 |
|------|------|------|------|
| 0-1 | u16 | `fun1` | 命令组: `0xFA00`=查询, `0xFB00`=设置 |
| 2-3 | u16 | `fun2` | 子命令: `0x0800`=性能模式, `0x1000`=电池充电 |
| 4-5 | u16 | `fun3` | 参数选择器: `0x0000`=(性能模式), `0x0002`=(充电控制) |
| 6-9 | u32 | `fun4` | 参数值 (原始 EC 值) |
| 10-31 | - | 填充 | 全零 |

**响应格式（32 字节缓冲区）**：

| 偏移 | 类型 | 字段 | 说明 |
|------|------|------|------|
| 0-1 | u16 | 状态 | `0x0000`=成功 |
| 2-3 | u16 | 回显 | 返回 fun1 值 |
| 4-5 | u16 | `data0` | 数据 (读模式时包含 EC 值) |
| 6-9 | u32 | `data1` | 数据 (读模式时包含扩展值) |

**具体命令**：

| 操作 | fun1 | fun2 | fun3 | fun4 |
|------|------|------|------|------|
| 查询性能模式 | 0xFA00 | 0x0800 | 0x0000 | 0 |
| 设置性能模式 | 0xFB00 | 0x0800 | 0x0000 | rawCode |
| 查询充电限制 | 0xFA00 | 0x1000 | 0x0002 | 0 |
| 设置充电限制 | 0xFB00 | 0x1000 | 0x0002 | rawCode |

**充电限制编码映射**（Meow-Box `BatteryControlModels.cs`）：

| 前端显示值 | rawCode |
|-----------|---------|
| 100% (关闭) | 0 |
| 80% | 1 |
| 90% | 4 |
| 70% | 5 |
| 60% | 6 |
| 50% | 7 |
| 40% | 8 |

> 注意：WMI 后端的充电限制 rawCode 与 EC 直接值不同，需要在驱动层做映射转换。
> 而 WinRing0 后端直接写百分比值到 EC 地址 `0xA7`。

#### `ec/battery.rs` — 电池养护逻辑（后端无关）

```rust
pub struct BatteryCare {
    pub enabled: bool,
    pub charge_limit: u8,  // 0-100
}

// 两种后端共用的常量：
// 通用逻辑层：读/写都映射到后端 trait 方法
impl BatteryCare {
    pub fn read(backend: &dyn EcBackend) -> Result<Self, EcError>;
    pub fn apply(&self, backend: &dyn EcBackend) -> Result<(), EcError>;
}

// 内部实现：
// WinRing0 路径：直接读取 EC 地址 0xA4/0xA7
// WMI 路径：通过 MiInterface 的查询/设置命令 + rawCode 映射
fn read_via_winring0(dev: &dyn EcBackend) -> (bool, u8);
fn read_via_wmi(dev: &dyn EcBackend) -> (bool, u8);
```

#### `ec/performance.rs` — 性能模式逻辑（后端无关）

```rust
const EC_PERFORMANCE_MODE_ADDR: u16 = 0x68;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum PerfMode {
    Eco    = 0x0A,
    Quiet  = 0x02,
    Smart  = 0x09,
    Fast   = 0x03,
    Extreme = 0x04,
}

impl PerfMode {
    pub fn from_ec_value(v: u8) -> Option<Self>;
    pub fn to_ec_value(self) -> u8;
    pub fn read(backend: &dyn EcBackend) -> Result<Self, EcError>;
    pub fn apply(self, backend: &dyn EcBackend) -> Result<(), EcError>;
}

// WinRing0 路径：直接读写 EC 地址 0x68
// WMI 路径：MIInterface(fun1=0xFA00/0xFB00, fun2=0x0800, ...)
```

#### `ec/mod.rs` — 后端工厂

```rust
pub enum BackendKind {
    WinRing0,
    Wmi,
}

/// 根据可用性和配置自动选择后端
pub fn create_backend(preferred: Option<BackendKind>) -> Result<Box<dyn EcBackend>, EcError> {
    // 1. 如果配置指定了后端，尝试该后端
    if let Some(kind) = preferred {
        match kind {
            BackendKind::Wmi => {
                if let Ok(wmi) = WmiBackend::new() {
                    return Ok(Box::new(wmi));
                }
            }
            BackendKind::WinRing0 => {
                if let Ok(wr) = WinRing0Backend::new() {
                    return Ok(Box::new(wr));
                }
            }
        }
    }
    // 2. 默认顺序：WMI → WinRing0
    if let Ok(wmi) = WmiBackend::new() {
        return Ok(Box::new(wmi));
    }
    WinRing0Backend::new().map(|wr| Box::new(wr) as Box<dyn EcBackend>)
}
```

#### `config.rs` — 配置管理

```rust
// 配置文件路径: %APPDATA%/XiaomiPcManagerLite/config.toml
// (取代原来的 exe 目录 INI 文件)

#[derive(Debug, Serialize, Deserialize)]
pub struct AppConfig {
    pub battery_care_enabled: bool,
    pub battery_charge_limit: u8,          // 0-100
    pub performance_mode: PerfMode,
    pub auto_apply_on_startup: bool,
    pub auto_reapply_on_power_change: bool,
    pub window_visible: bool,
    pub backend: BackendPreference,        // "auto" | "winring0" | "wmi"
}

#[derive(Debug, Serialize, Deserialize)]
pub enum BackendPreference {
    Auto,
    WinRing0,
    Wmi,
}

impl AppConfig {
    pub fn load() -> Self;       // 从文件加载，不存在则返回默认值
    pub fn save(&self);          // 写入文件
    pub fn default() -> Self;    // 出厂默认值
}
```

#### `commands.rs` — Tauri API 层

```rust
#[tauri::command]
fn get_status(state: State<AppState>) -> Result<DeviceStatus, String>;
// 返回: { battery: { enabled, charge_limit }, perf_mode, ec_accessible }

#[tauri::command]
fn set_battery_care(state: State<AppState>, enabled: bool, charge_limit: u8) -> Result<(), String>;

#[tauri::command]
fn set_performance_mode(state: State<AppState>, mode: PerfMode) -> Result<(), String>;

#[tauri::command]
fn get_config(state: State<AppState>) -> Result<AppConfig, String>;

#[tauri::command]
fn save_config(state: State<AppState>, config: AppConfig) -> Result<(), String>;
```

#### `tray.rs` — 系统托盘

使用 `tray-icon` crate 实现：
- 托盘图标（从 `icons/` 加载）
- 左键单击：显示/隐藏主窗口
- 右键菜单：
  - 显示主窗口
  - 电池养护 → 启用 / 关闭
  - 性能模式 → 省电 / 安静 / 智能 / 极速 / 极致
  - ─────
  - 退出

#### `hotkey.rs` — 全局快捷键

使用 `global-hotkey` crate：
- `Ctrl+Alt+B`：切换电池养护（开/关）
- `Ctrl+Alt+P`：切换自定义性能模式（当前模式 ↔ 自定义模式，再按切回）

#### `power_event.rs` — 电源监听

使用 `windows` crate 监听电源事件（或通过 Tauri 窗口的 Win32 子类化）：
- `PBT_APMPOWERSTATUSCHANGE` → 延迟 3s 后自动重应用设置
- 如果 `auto_reapply_on_power_change = true`

#### `error.rs` — 统一错误类型

涵盖两种后端的错误：

```rust
#[derive(Debug, thiserror::Error)]
pub enum EcError {
    // WinRing0 后端错误
    #[error("WinRing0 DLL 加载失败: {0}")]
    DllLoad(String),
    #[error("WinRing0 初始化失败")]
    InitFailed,
    #[error("EC I/O 通信超时 (地址: {0:#x})")]
    IoTimeout(u16),

    // WMI 后端错误
    #[error("WMI 连接失败: {0}")]
    WmiConnect(String),
    #[error("WMI MICommonInterface 未找到")]
    WmiInterfaceNotFound,
    #[error("WMI MiInterface 调用返回错误: 状态={0}")]
    WmiCallFailed(u16),

    // 通用 EC 错误
    #[error("EC 读取失败 (地址: {0:#x})")]
    ReadFailed(u16),
    #[error("EC 写入失败 (地址: {0:#x})")]
    WriteFailed(u16),
    #[error("后端不可用: {0}")]
    BackendUnavailable(String),
    #[error("不支持的后端类型")]
    UnsupportedBackend,
}
```

### 5.2 前端组件

#### `App.svelte` — 根组件

- 自定义标题栏（可拖拽区域 + 最小化/关闭按钮）
- Fluent Design 风格的浅色背景
- 窗口透明度管理（通过 Tauri 命令设置 `WS_EX_LAYERED`）
- 响应式布局

#### `BatteryCare.svelte`

- 开关切换（Tauri Switch 风格）
- 滑块 + 数字输入：充电阈值 0-100%
- 保存时调用 `set_battery_care`

#### `PerformanceMode.svelte`

- 单选按钮组 / 卡片选择（5 种模式）
- 切换时应用并反馈状态

#### `EcStatus.svelte`

- 只读显示当前 EC 中的实际值
- "刷新状态" 按钮
- 自动定时刷新（可选）

#### `SettingsPanel.svelte`

- 开机自启开关（调用 Tauri `process::restart` + 注册表）
- 电源变动时自动重应用开关

### 5.3 前端状态管理 (`stores/app.ts`)

```typescript
interface AppState {
    batteryCare: {
        enabled: boolean;
        chargeLimit: number;
    };
    perfMode: 'eco' | 'quiet' | 'smart' | 'fast' | 'extreme';
    ecAccessible: boolean;
    backendName: string;      // 当前使用的后端: "WMI (MICommonInterface)" | "WinRing0 (I/O Port)"
    config: AppConfig;
}

// 通过 Tauri invoke 获取/更新状态
```

---

## 6. 与原项目的功能对照表

| 原功能 | C++ 实现方式 | Rust 实现方式 |
|--------|-------------|-------------|
| **EC 读写** | `ReadEC()` / `WriteEC()` 裸函数 + 全局变量 | `EcDriver` 结构体 + RAII |
| **电池养护设置** | `SetBatteryCare()` 读写 EC `0xA4`/`0xA7` | `BatteryCare::apply()` |
| **性能模式** | `SetPerformanceMode()` 写 EC `0x68` | `PerfMode::apply()` |
| **GUI 控件** | `CreateWindow()` 裸 Win32 | Svelte 组件 + CSS |
| **自定义标题栏** | `WS_POPUP` + `WM_LBUTTONDOWN` 拖动 | Svelte 实现 + Tauri `data-tauri-drag-region` |
| **窗口透明度** | `WS_EX_LAYERED` + `SetLayeredWindowAttributes` | Tauri + `windows` crate 设置 |
| **系统托盘** | `Shell_NotifyIcon` + `WM_TRAYICON` | `tray-icon` crate |
| **托盘右键菜单** | `CreatePopupMenu` + `TrackPopupMenu` | `tray-icon` 内置 `ContextMenu` |
| **全局快捷键** | 缺失 (README 提及但未实现) | `global-hotkey` crate |
| **配置存储** | 手写 `std::wifstream` 解析 INI | `serde` + `toml` |
| **开机自启** | 未实现 | Tauri `process` plugin + 注册表 |
| **电源变动监听** | `WM_POWERBROADCAST` + `SetTimer` | `windows` crate + `PBT_APMPOWERSTATUSCHANGE` |
| **任务栏重建恢复** | `RegisterWindowMessage("TaskbarCreated")` | `tray-icon` 内置处理 |
| **窗口隐藏到托盘** | `ShowWindow(SW_HIDE)` + `g_isMinimized` | Tauri window.hide() |
| **错误提示** | `MessageBox()` | Tauri dialog plugin |
| **WinRing0 加载** | `LoadLibrary` + `GetProcAddress` | `libloading` + `rust-embed` 运行时释放 |
| **EC 后端选择** | 仅 WinRing0 | `EcBackend` trait → 自动检测 WMI/WinRing0 |
| **WMI 通信** | 不支持 | WMI `MICommonInterface.MiInterface` 32 字节协议 |

---

## 7. 实施阶段

### 阶段 0：环境搭建（半天）

- [ ] 安装 Rust (rustup + stable toolchain)
- [ ] 安装 Tauri CLI: `cargo install tauri-cli`
- [ ] 创建项目: `cargo tauri init --template svelte`
- [ ] 验证 `cargo tauri dev` 能跑空窗口
- [ ] 下载 WinRing0 (WinRing0x64.dll + WinRing0x64.sys) 到 `src-tauri/bin/`

### 阶段 1：EC 抽象层（1 天）

#### 1a：trait 定义 + WinRing0 后端

- [ ] 实现 `ec/backend.rs`：`EcBackend` trait 定义
- [ ] 实现 `ec/winring0.rs`：WinRing0 `libloading` FFI + `EcBackend` impl
- [ ] 单元测试：mock WinRing0 DLL 验证 EC 读写逻辑
- [ ] 验证：在目标机器上通过 WinRing0 读取 EC 值

#### 1b：WMI 后端

- [ ] 调研 `windows` crate 的 WMI API 调用方式
- [ ] 实现 `ec/wmi.rs`：发现 `MICommonInterface` 实例 + 调用 `MiInterface`
- [ ] 实现 32 字节命令缓冲区编码/解码
- [ ] 实现充电限制 rawCode ↔ 百分比映射
- [ ] 单元测试：mock WMI 响应验证编解码
- [ ] 验证：在目标机器上通过 WMI 读取 EC 值

#### 1c：后端工厂 + 自动选择

- [ ] 实现 `ec/mod.rs`：`create_backend()` 工厂函数 (WMI 优先)
- [ ] 实现 error 类型：合并 WinRing0/WMI 两种错误
- [ ] 配置项 `backend = "auto" | "winring0" | "wmi"`
- [ ] 验证：WMI 可用时自动选 WMI，不可用时回退 WinRing0

### 阶段 2：业务逻辑 + 配置（半天）

- [ ] 实现 `ec/battery.rs`：`BatteryCare` 结构体（trait 泛型）
- [ ] 实现 `ec/performance.rs`：`PerfMode` 枚举 + 映射
- [ ] 实现 `config.rs`：serde + toml，配置文件路径 `%APPDATA%/.../config.toml`
- [ ] 实现 `config.rs` 中的后端选择持久化

### 阶段 3：Tauri 集成 — 命令层（半天）

- [ ] 实现 `commands.rs`：所有 Tauri Command
- [ ] 使用 `tauri::State<AppState>` 管理 `Box<dyn EcBackend>` 生命周期
- [ ] 测试：用 Tauri 调试面板手动 invoke 命令
- [ ] 验证：前端能通过 `invoke('get_status')` 获取 EC 数据

### 阶段 4：系统托盘 + 快捷键 + 电源监听（半天）

- [ ] 实现 `tray.rs`：托盘图标 + 右键菜单（电池养护/性能模式子菜单）
- [ ] 实现 `hotkey.rs`：注册 `Ctrl+Alt+B` / `Ctrl+Alt+P`
- [ ] 实现 `power_event.rs`：`WM_POWERBROADCAST` 监听 + 自动重应用
- [ ] 验证：托盘能控制电池养护和性能模式
- [ ] 验证：快捷键生效

### 阶段 5：前端 UI（1-2 天）

- [ ] `TitleBar.svelte`：自定义标题栏 + 拖拽
- [ ] `BatteryCare.svelte`：开关 + 滑条（0-100%）
- [ ] `PerformanceMode.svelte`：5 种模式卡片选择
- [ ] `EcStatus.svelte`：后端名称 + EC 实时状态面板
- [ ] `SettingsPanel.svelte`：开机自启 + 电源重应用 + 后端选择
- [ ] `App.svelte`：组合所有组件 + 响应式布局
- [ ] 样式：仿 Fluent Design 浅色风格
- [ ] 窗口透明度（Tauri 配置 + Rust 设置）
- [ ] 验证：所有 UI 操作能正确调用后端

### 阶段 6：单文件打包 + 分发（1 天）

- [ ] 使用 `rust-embed` 将 WinRing0 DLL/SYS 嵌入 exe
- [ ] 实现运行时释放 + 进程退出清理逻辑
- [ ] Tauri manifest 嵌入管理员权限 (`requireAdministrator`)
- [ ] 构建命令：`cargo tauri build --bundles none`
- [ ] 可选项：UPX 压缩 `upx --best --lzma target/release/XiaomiPcManagerLite.exe`
- [ ] WebView2 检测 + 引导页面（Win10 用户）
- [ ] 更新 README：单文件使用说明 + 双后端说明
- [ ] 验证：在一台干净 Windows VM 上只复制 exe 即可运行

### 总预估工时：4-6 天（按一个开发者）

---

## 8. 关键技术决策

### 8.1 WinRing0 处理策略

**现状**：WinRing0 是闭源驱动，包含：
- `WinRing0x64.dll` / `WinRing0.dll` — 用户态 DLL
- `WinRing0x64.sys` / `WinRing0.sys` — 内核驱动

**方案**：将 DLL 和 SYS 作为二进制资源嵌入到 exe 中，运行时按需释放到临时目录。
- 使用 `rust-embed` 或 `winresource` crate 嵌入文件
- 启动时检测目标目录是否存在对应文件，不存在则释放
- 进程退出时自动清理释放的文件
- 优点：单文件分发，用户无感
- 驱动签名问题：WMI 后端（无需驱动）优先尝试，不成功再尝试 WinRing0

```rust
// 伪代码：嵌入 + 运行时释放
static WINRING0_DLL: EmbeddedFile = include_embedded!("WinRing0x64.dll");
static WINRING0_SYS: EmbeddedFile = include_embedded!("WinRing0x64.sys");

fn ensure_driver_files() -> PathBuf {
    let dir = temp_dir().join("XiaomiPcManagerLite");
    fs::create_dir_all(&dir).ok();
    for (name, data) in [("WinRing0x64.dll", &WINRING0_DLL),
                         ("WinRing0x64.sys", &WINRING0_SYS)] {
        let path = dir.join(name);
        if !path.exists() {
            fs::write(&path, data).ok();
        }
    }
    dir
}
```

### 8.2 管理员权限

WinRing0 驱动需要管理员权限加载。Tauri 打包时配置：
- `tauri.conf.json` 中设置 `"security": { ... }`
- MSI 安装包中配置 `RequestedExecutionLevel="highestAvailable"` 或 `requireAdministrator`
- 应用启动时如果非管理员，弹出提示

### 8.3 配置文件路径

| 原项目 | 新项目 |
|--------|--------|
| `exe 目录/config.ini` | `%APPDATA%/XiaomiPcManagerLite/config.toml` |
| 手写 key=value 解析 | `serde` + `toml` 自动序列化 |

使用 `dirs` crate (`dirs::config_dir()`) 获取标准配置目录。

### 8.4 监听电源事件

Tauri 窗口基于 WebView2，不直接暴露 `WM_POWERBROADCAST`。实现方式：
1. 在 `setup()` 中通过 `windows` crate 创建一个隐藏的 Win32 窗口
2. 注册 `WNDCLASS` + `WindowProc`，处理 `WM_POWERBROADCAST`
3. 收到事件后通过 Tauri event 系统推到前端
4. 前端延迟 3s 后自动调用 `set_battery_care` + `set_performance_mode`

### 8.5 窗口透明度

原项目使用 `WS_EX_LAYERED` + `SetLayeredWindowAttributes(alpha=180)`。
Tauri 默认不支持 `WS_EX_LAYERED`，实现方式：
1. 在 `tauri.conf.json` 设置 `"transparent": true`
2. 或通过 `windows` crate 在 `setup()` 中获取窗口 HWND 后设置
3. 前端 CSS `background: rgba(245, 250, 255, 0.9)` 实现透明外观

### 8.6 单文件分发策略

项目产出品为单一 exe 文件，无需安装程序，满足便携分发需求。

**嵌入内容**（使用 `rust-embed`）：
| 文件 | 大小 | 说明 |
|------|------|------|
| `WinRing0x64.dll` | ~50KB | WinRing0 用户态 DLL (64位) |
| `WinRing0x64.sys` | ~20KB | WinRing0 内核驱动 (64位) |
| `WinRing0.dll` | ~40KB | WinRing0 用户态 DLL (32位, fallback) |
| `WinRing0.sys` | ~16KB | WinRing0 内核驱动 (32位, fallback) |
| 前端静态资源 | ~200KB | Svelte 编译后的 HTML/CSS/JS |
| 合计嵌入开销 | ~330KB | 加上 Rust/Tauri 本体约 5MB，最终 exe 约 5.5MB |

**运行时释放流程**：
1. exe 启动，检测本目录是否存在 `WinRing0x64.dll`
2. 不存在则从嵌入资源释放到 `exe 目录/.driver/` 下
3. 尝试初始化 WMI 后端（无需驱动）
4. WMI 失败则尝试 WinRing0 后端（加载释放的 DLL/SYS）
5. 进程退出时清理释放的临时文件

**WebView2 依赖**：
- Windows 11：系统自带，无需额外操作
- Windows 10：首次运行提示引导安装 [Evergreen WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/)
- Tauri 的 `webview-install-mode` 配置可设置为 `downloadBootstrapper`

**压缩**：可使用 UPX 压缩 exe（`upx --best --lzma`），最终体积约 2-3MB。

### 8.7 任务栏重建恢复

`tray-icon` crate 内部已处理 `TaskbarCreated` 消息，无需手动实现。

---

## 9. 安全注意事项

1. WinRing0 驱动需以管理员权限加载
2. 所有 `unsafe` 代码集中在 `ec/driver.rs` 的 FFI 绑定中，对外暴露 safe 接口
3. 配置文件的 `%APPDATA%` 路径自动获取，不写死
4. 前端通过 Tauri `invoke` 调用后端，不直接暴露 Win32 API
5. 使用 `thiserror` 确保所有错误被正确处理

---

## 10. 风险与缓解

| 风险 | 影响 | 缓解措施 |
|------|------|----------|
| WinRing0 驱动签名问题 | 驱动无法加载，EC 不可用 | **WMI 后端优先**，无需驱动签名即可工作；WinRing0 作为 fallback |
| WMI MIInterface 非标准接口 | 可能存在型号兼容问题 | 保持 WinRing0 作为 fallback；配置项可强制指定后端 |
| WebView2 运行时缺失 | 窗口无法显示 | Win11 自带 WebView2；Win10 提示用户安装（单页引导） |
| 单文件释放 DLL 被杀毒软件拦截 | 运行时释放失败 | 数字签名 exe；释放前校验 hash；提供管理员信任指南 |
| Tauri 窗口透明度不支持 | UI 效果不同 | 备选方案：通过 `windows` crate 手动设置窗口属性 |
| `tray-icon` 在特定 Windows 版本问题 | 托盘不工作 | `tray-icon` 已处理 Win10/Win11 差异；回退方案：纯 Win32 托盘 |
| 前端不熟悉 Tauri API | 开发效率下降 | 初期使用 React (社区资源多) 或严格参照 Tauri 官方示例 |

---

## 11. 附录：Cargo.toml 关键依赖

```toml
[package]
name = "xiaomi-pc-manager-lite"
version = "2.0.0"
edition = "2024"

[lib]
name = "xiaomi_pc_manager_lite_lib"
crate-type = ["lib", "cdylib", "staticlib"]

[build-dependencies]
tauri-build = { version = "2", features = [] }

[dependencies]
tauri = { version = "2", features = ["tray-icon"] }
tauri-plugin-dialog = "2"
tauri-plugin-process = "2"
tauri-plugin-shell = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
libloading = "0.8"
tray-icon = "0.24"
global-hotkey = "0.8"
rust-embed = "8"                         # 将 WinRing0 DLL/SYS 嵌入 exe
windows = { version = "0.62", features = [
    "Win32_Foundation",
    "Win32_UI_WindowsAndMessaging",
    "Win32_System_Power",
    "Win32_System_LibraryLoader",
    "Win32_System_Registry",
    "Win32_System_Com",                  # WMI COM 支持
    "Win32_System_Wmi",                  # WMI API
    "Win32_System_Ole",                  # CoInitializeSecurity
]}
thiserror = "2"
dirs = "6"
log = "0.4"
env_logger = "0.11"
```

### 附录：前端 package.json 关键依赖

```json
{
  "dependencies": {
    "@tauri-apps/api": "^2",
    "@tauri-apps/plugin-dialog": "^2",
    "@tauri-apps/plugin-process": "^2"
  },
  "devDependencies": {
    "@sveltejs/vite-plugin-svelte": "^5",
    "svelte": "^5",
    "typescript": "^5",
    "vite": "^6"
  }
}
```

---

## 12. 附录：Tauri 配置

```json
// src-tauri/tauri.conf.json (关键部分)
{
  "productName": "Xiaomi PC Manager Lite",
  "version": "2.0.0",
  "identifier": "com.xiaomi.pc-manager-lite",
  "build": {
    "frontendDist": "../build",
    "devUrl": "http://localhost:5173",
    "beforeBuildCommand": "npm run build",
    "beforeDevCommand": "npm run dev"
  },
  "app": {
    "windows": [
      {
        "title": "小米电脑管家轻量版",
        "width": 650,
        "height": 520,
        "center": true,
        "decorations": false,
        "resizable": false
      }
    ],
    "security": {
      "csp": null
    }
  },
  "plugins": {}
}
```
