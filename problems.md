# Problems

## 严重

### 1. `tauri.conf.json` 构建命令使用 `npm` 而非 `pnpm`

**文件**: `src-tauri/tauri.conf.json:9-10`

项目使用 `pnpm` 作为包管理器，但 `tauri.conf.json` 中的构建命令写的是 `npm`：

```json
"beforeDevCommand": "npm run dev",
"beforeBuildCommand": "npm run build"
```

改为 `pnpm run dev` / `pnpm run build`。

---

### 2. `wmi.rs` `to_pcwstr` 每次调用泄漏内存

**文件**: `src-tauri/src/ec/wmi.rs:284-287`

```rust
fn to_pcwstr(s: &str) -> PCWSTR {
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let leaked: &'static [u16] = Box::leak(wide.into_boxed_slice());
    PCWSTR(leaked.as_ptr())
}
```

每次传入 `"MiInterface"` 或 `"Buffer"` 都会 `Box::leak` — 泄漏的内存不会回收。这两个是固定字符串，调用频次高。

**修复**: 用 `OnceLock<[u16]>` 做静态缓存：

```rust
fn cached_pcwstr(s: &str) -> PCWSTR {
    static MI_INTERFACE: OnceLock<[u16]> = OnceLock::new();
    static BUFFER: OnceLock<[u16]> = OnceLock::new();
    let data = match s {
        "MiInterface" => MI_INTERFACE.get_or_init(|| ...),
        "Buffer" => BUFFER.get_or_init(|| ...),
        _ => return to_pcwstr(s),
    };
    PCWSTR(data.as_ptr())
}
```

---

### 3. `wmi.rs` `ExecMethod` 错误码被吞掉

**文件**: `src-tauri/src/ec/wmi.rs:145`

```rust
.map_err(|_| EcError::WmiCallFailed(0))
```

`IWbemServices::ExecMethod` 返回的 `windows_core::Result<()>` 包含 HRESULT 错误码，但这里直接用 `|_|` 丢弃，无法区分失败原因（权限不足/接口不存在/参数错误）。

**修复**: 提取 HRESULT 传入 `WmiCallFailed`：

```rust
.map_err(|e| EcError::WmiCallFailed(e.code().0 as u16))
```

---

### 4. `lib.rs` 后端创建失败直接 `panic!`

**文件**: `src-tauri/src/lib.rs:28-36`

```rust
let backend = match backend {
    Ok(b) => b,
    Err(e) => {
        log::error!("Failed to create EC backend: {}", e);
        panic!("EC backend required: {:?}", e);
    }
};
```

两个后端都无法初始化时程序直接崩溃，用户看到的只是进程退出，没有任何界面提示。

**建议**: 用 `tauri::api::dialog` 或 `tauri_plugin_dialog` 弹一个错误框：「未找到可用的 EC 后端，请确认驱动已安装」，然后退出。

---

## 中等

### 5. `wmi.rs` `SafeArrayDestroy` 时机有误导性

**文件**: `src-tauri/src/ec/wmi.rs:110-132`

```rust
let sa = SafeArrayCreateVector(...);
// ... fill sa with data ...
let mut v = VARIANT::default();
v.Anonymous.Anonymous.Anonymous.parray = sa;
in_params.Put(prop_name, 0, &v as *const VARIANT, 0)?;
SafeArrayDestroy(sa)?;   // <-- 这里销毁
// ... 之后 ExecMethod(&in_params)
```

`IWbemClassObject::Put` 会拷贝数据，所以提前销毁 `sa` 是安全的。但从代码阅读顺序上看，销毁发生在 `ExecMethod` 之前，容易让人疑惑 `in_params` 是否还有效。

**建议**: 加注释说明 `Put` 拷贝了数据，或移到 `ExecMethod` 之后统一清理。

---

### 6. `winring0.rs` DLL 路径硬编码

**文件**: `src-tauri/src/ec/winring0.rs:45`

```rust
let lib = unsafe { Library::new(dll_name) }
```

`WinRing0x64.dll` 直接以文件名加载，依赖 DLL 在搜索路径中。目前 `src-tauri/bin/` 下有文件但不参与构建部署。后续 `rust-embed` 嵌入后需要提取到临时目录再加载。

**影响**: 当前直接在 Windows 运行会找不到 DLL。

---

### 7. `config.rs` 路径含空格风险

**文件**: `src-tauri/src/ec/config.rs:42-45`

```rust
fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("XiaomiPcManagerLite")
}
```

Windows 上 `%APPDATA%` 路径为 `C:\Users\<用户名>\AppData\Roaming`，如果用户名含空格（如 "John Doe"），`std::fs` 操作不会有问题（Rust 的 PathBuf 正确处理空格）。**无需修改**。

---

### 8. `App.svelte` 后端选择切换不生效

**文件**: `src/App.svelte:99-101`

前端 `SettingsPanel` 的 `onBackendChange` 调用 `handleSaveConfig()` 将偏好存入配置文件，但正在运行的后端不会切换。用户需要重启应用才能生效。

**建议**: 在切换后端时弹确认对话框提示「切换后端需要重启应用」。

---

## 低 / 设计建议

### 9. `tauri.conf.json` 构建目标应限制为 Windows

**文件**: `src-tauri/tauri.conf.json:28`

```json
"targets": "all"
```

会尝试为 `msi`/`nsis`/`dmg`/`deb`/`AppImage` 等所有平台生成安装包。Windows-only 项目应改为：

```json
"targets": "nsis"
```

（推荐 NSIS — 单一 exe 安装包）

---

### 10. `PerfMode` 枚举已定义未使用

**文件**: `src-tauri/src/ec/performance.rs`

`PerfMode` 枚举提供了 `from_ec_value()`、`name()`、`all()`、`is_valid()` 等方法，但在 `commands.rs` 和前端中都直接用 `u8` 传递性能模式值。

**建议**:
- `commands.rs` 的 `StatusResponse` 可增加 `performance_mode_name: String` 字段，用 `PerfMode::name()` 填充
- 前端可以不感知 EC 原始值

---

### 11. `src/stores/` 空目录

目录存在但没有任何文件。项目使用 Svelte 5 `$state` rune 在 `App.svelte` 中管理状态，不需要额外 store。

**建议**: 删除或留作后续扩展用。

---

### 12. `lib.rs` config 保存逻辑不完整

**文件**: `src-tauri/src/lib.rs:51-53`

```rust
if config.auto_apply_on_startup {
    config.save().ok();
}
```

只有 `auto_apply_on_startup` 为 true 时才保存配置。但 `commands.rs` 中每次修改电池/性能模式都会单独 `config.save().ok()`，所以运行时配置已经持久化了。这里的 save 仅用于同步启动时从 EC 读取到的当前值到配置文件。

**建议**: 无条件保存一次，确保配置文件始终反映 EC 真实状态：

```rust
config.save().ok();
```

---

### 13. `Cargo.toml` 缺少 `global-hotkey` 依赖

**文件**: `src-tauri/Cargo.toml`

`remaining-works.md` 中计划了 `global-hotkey = "0.8"`，但尚未添加到 `[dependencies]`。

---

## Windows 迁移踩坑清单

切换 Windows 后 `cargo build` 预期会遇到的编译/运行时问题：

| # | 预期问题 | 可能原因 |
|---|---------|---------|
| 1 | `windows` crate 找不到 `Win32_System_Wmi` 模块 | 需确认 features 列表完整 |
| 2 | `IWbemServices::ExecMethod` 签名不符合预期 | windows crate v0.62 的 WMI API 需要实测 |
| 3 | `SafeArrayCreateVector` 返回 null | `OleAut32.dll` 未初始化或版本不匹配 |
| 4 | `VARIANT` 构造 segfault | `Anonymous` 嵌套 union 字段访问路径错误 |
| 5 | `CoInitializeEx` 失败 (HRESULT) | 已在调试模式下调用过 COM？改为 `COINIT_APARTMENTTHREADED`？ |
| 6 | `ConnectServer` 返回 `WBEM_E_ACCESS_DENIED` | 需要管理员权限或 `CoSetProxyBlanket` 参数调整 |
| 7 | WinRing0 DLL 加载失败 | `bin/` 目录的 DLL 不在搜索路径中 |
