# Xiaomi PC Manager Lite

轻量级小米 PC 管理工具 — 电池养护 / 性能模式切换

## 构建

```bash
# 安装依赖
npm install

# 开发模式
npm run tauri dev

# 发布构建
npm run tauri build
```

## 架构

- **后端**: Rust + Tauri v2，双 EC 通信后端（WMI MIInterface 优先 → WinRing0 fallback）
- **前端**: Svelte 5
- **分发**: 单一 exe（WinRing0 DLL/SYS 嵌入资源，无需安装）

详见 [refactor-plan.md](refactor-plan.md)
