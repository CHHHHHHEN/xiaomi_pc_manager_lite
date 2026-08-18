fn main() {
    // 为可执行文件嵌入 Windows 版本信息（资源表）：文件资源管理器
    // "属性 → 详细信息"会显示描述/版本/版权等，缺失时显示为空（实测）。
    // winres 构建期工具：Cargo.toml [build-dependencies] 声明，无运行时成本。
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set("FileDescription", "Xiaomi PC Manager Lite");
        res.set("ProductName", "Xiaomi PC Manager Lite");
        res.set("CompanyName", "");
        res.set("LegalCopyright", "");
        res.set("OriginalFilename", "xiaomi-pc-manager-lite.exe");
        // 嵌入应用图标（多尺寸 ICO，16/32/48/256 PNG 块）：资源管理器/
        // 任务栏在窗口创建前（如 UAC 弹窗、资源管理器文件视图）使用 exe
        // 自带的图标，窗口创建后由 set_main_window_icon 覆盖为同源图像。
        res.set_icon("icons/tray_icon.ico");
        // FileVersion/ProductVersion 与展示版本号 `src/util.rs::APP_VERSION`
        // 保持同步（Windows 支持四段 `1.0.0.6`；Cargo.toml 的 semver 为 `1.0.0`）。
        res.set("FileVersion", "1.0.0.6");
        res.set("ProductVersion", "1.0.0.6");
        // 需要管理员权限（WritePort 提权）：版本信息里带 requestedExecutionLevel，
        // 与启动时 elevate_self() 的 UAC 弹窗语义一致（不在此处强制嵌入清单，
        // 保留运行时提权逻辑的单一事实来源）。
        if let Err(e) = res.compile() {
            println!("cargo:warning=winres failed to embed version info: {}", e);
        }
    }
}
