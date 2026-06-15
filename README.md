# Xiaomi PC Manager Lite

轻量级小米 PC 管理工具 — 电池养护 / 性能模式切换

## 主要功能

WinRing0 和 WMI 双控制后端，两者功能基本相同。

- 设置最大充电水平，开启或者关闭充电保护功能
- 切换风扇策略 Eco/Quiet/Smart/Fast/Extreme

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
├── main.rs         # 入口：初始化后端 → 加载配置 → GUI
├── command.rs      # 共享命令枚举 (UiCommand)
├── embed.rs        # WinRing0 DLL/SYS 运行时提取
├── ec/             # 硬件访问层 (Hardware Abstraction Layer)
│   ├── mod.rs      # EC 寄存器地址常量
│   ├── backend.rs  # EcBackend trait + 工厂函数
│   ├── winring0.rs # WinRing0 后端 (I/O Port)
│   ├── wmi.rs      # WMI 后端 (MICommonInterface)
│   ├── battery.rs  # 充电上限映射
│   ├── performance.rs # 性能模式枚举
│   ├── config.rs   # 配置序列化 (TOML)
│   ├── error.rs    # 错误枚举
│   └── fnkey.rs    # Fn+Key WMI 事件监控
├── gui/            # 图形界面
│   ├── app.rs      # 主应用结构 + eframe 入口
│   ├── commands.rs # 命令处理 + 状态管理
│   └── view.rs     # UI 渲染
└── tray/           # 系统托盘 + 消息泵
    ├── window.rs   # 消息窗口
    └── worker.rs   # 托盘图标 + 全局热键 + 电源事件
```
