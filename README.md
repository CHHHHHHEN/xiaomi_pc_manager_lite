

# Xiaomi PC Manager Lite

轻量级小米 PC 管理工具 — 电池养护 / 性能模式切换

<img src="docs/ui.png" alt="UI 截图" width="468.75" height="667.5" />

## 主要功能

WinRing0 和 WMI 双控制后端，两者功能基本相同（WMI 仅支持固定充电上限值）。

- 设置最大充电水平，开启或者关闭充电保护功能
- 切换风扇策略 Eco/Quiet/Smart/Fast/Extreme
- Fn 功能键自定义绑定（如 Fn+K 循环切换性能模式，可在设置中重新绑定）
- 托盘快捷操作：右键直接切换电池养护 / 选择性能模式，悬停实时显示状态与电量
- 电源智能管理：拔电自动切节能模式（可关），休眠唤醒自动重设设置
- 电池健康显示：GUI 状态区与托盘 tooltip 实时展示电池健康度（满充/设计容量），数据来自 Windows 标准 ACPI WMI，与后端无关
- 电池时长估算：按充放电速率实时估算"预计剩余/充满时长"，GUI 与托盘 tooltip 展示
- 窗口隐藏到托盘后命令即时生效，状态变化（性能/养护）弹托盘通知

应用图标来源为项目根目录 `chi.png`（透明背景）：构建期由 `icons/icon.png`
（512×512）与 `icons/tray_icon.ico`（16/32/48/256 多帧）承载，任务栏/窗口/
标题栏/托盘/exe 图标统一使用该图像。

## 构建

```bash
# 类型检查
cargo check

# 构建
cargo build

# 发布构建
cargo build --release

# 运行
cargo run
```

## 代码结构

```
src/
├── main.rs         # 二进制入口：windows_subsystem 属性 + 调用 launch::run()
├── lib.rs          # 库根：声明全部模块、说明分层（依赖方向单一、无环）
├── launch.rs       # 组合根：日志/panic 兜底/单实例/提权/后端初始化/GUI 启动
├── util/           # 跨层工具（各层唯一事实来源）
│   ├── app.rs      #   应用元数据（APP_NAME/APP_VERSION/窗口尺寸/日志路径）
│   ├── fs.rs       #   原子文件写（临时文件 + fsync + rename，配置与驱动共用）
│   ├── text.rs     #   UTF-16 缓冲（WideString）与定长宽字符串写入
│   ├── thread.rs   #   线程兜底（spawn_guarded/catch_panic/panic_message）
│   └── sync.rs     #   锁毒化恢复 + 只告警一次（lock_or_recover/log_once）
├── win/            # Windows 互操作基础设施（供 ec/platform 复用，仅依赖 util）
│   ├── com.rs      #   COM 公寓/root\wmi 连接/WQL 枚举/SAFEARRAY
│   └── variant.rs  #   VARIANT RAII + 属性读取
├── app/            # 应用核心层：领域模型 + 用例编排 + 端口定义（不依赖 GUI/平台/ec）
│   ├── ec.rs       # 硬件访问端口：EcBackend trait、EcError、BackendPreference、EcBackendFactory
│   ├── config.rs   # 配置序列化 (TOML) + 消毒（AppConfig/ConfigStore）
│   ├── command.rs  # 共享命令枚举 (UiCommand)
│   ├── startup.rs  # 后端初始化/回退/启动配置应用（经 EcBackendFactory 端口注入工厂）
│   ├── battery.rs  # 充电上限策略 + 应用/回写规则（WMI 协议细节在 ec）
│   ├── performance.rs # 性能模式枚举 + 循环序列
│   ├── limits.rs   # 充电限值校验/兜底常量
│   ├── fnkey.rs    # Fn 绑定模型 + 匹配/去重/校验（纯逻辑）
│   ├── notify.rs   # 托盘通知触发判定（纯决策）
│   ├── power.rs    # 电源状态枚举 + PowerSource 端口
│   └── sink.rs     # 后台线程 → GUI 命令端口（CommandSink）
├── ec/             # 硬件访问适配器（实现 app::ec 的端口）
│   ├── backend.rs  # 后端创建（create_backend / BackendFactory）+ NullBackend + 端口重导出
│   ├── embed.rs    # WinRing0 DLL/SYS 运行时提取（原子写、内容校验、架构文件名）
│   ├── winring0.rs # WinRing0 后端 (I/O Port)
│   ├── wmi/        # WMI 后端 (MICommonInterface)
│   │   ├── mod.rs  # 线程模型 + WmiWorker + WmiBackend 代理 + EcBackend 实现
│   │   └── protocol.rs # MiInterface 线协议 + WMI 充电上限映射（纯逻辑，无 COM 依赖）
│   ├── fn_watcher.rs # Fn 功能键 WMI 事件监听（经 CommandSink 回传）
│   ├── addr.rs     # EC 寄存器地址常量
│   └── mock.rs     # 测试用后端模拟
├── gui/            # 图形界面（eframe/egui）
│   ├── app.rs      # 主应用结构 + eframe 入口 + CommandSink 实现
│   ├── commands.rs # 命令处理 + 状态管理（测试在 commands/tests.rs）
│   └── view.rs     # UI 渲染
├── platform/       # Windows 平台集成（适配器，实现 app 层端口）
│   ├── power.rs    # GetSystemPowerStatus → PowerSource 端口实现
│   ├── window.rs   # 主窗口显示控制（托盘隐藏/显示）
│   ├── icon.rs     # 窗口/托盘图标构建与设置
│   ├── battery_health.rs # 电池健康监测（root\WMI 容量读数）
│   ├── autostart.rs # 开机自启动（计划任务）
│   ├── privilege.rs # 管理员提权
│   └── single_instance.rs # 单实例保护
└── tray/           # 系统托盘 + 消息泵
    ├── worker.rs        # 托盘图标 + 全局热键 + 电源事件（线程局部状态收敛）
    ├── message_window.rs # 隐藏消息窗口
    └── notify.rs        # 托盘气泡通知展示（判定在 app::notify）
```

分层约定（依赖方向单向、无环）：`util`（跨层工具）→ `win`（Windows 互操作）
→ `app`（领域 + 端口，不依赖 GUI/平台/ec）；`ec` 实现 `app::ec` 的硬件端口，
`platform` 实现 `app::power` 等平台端口，两者共用 `win` 基础设施；
`gui`/`tray` 依赖 `app` 领域模型，并组合 `ec`/`platform` 的适配器实现。
驱动文件名的唯一事实来源在 `ec::embed`，`winring0` 经其引用、不再相互依赖。
