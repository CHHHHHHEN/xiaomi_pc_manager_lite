use rust_embed::RustEmbed;
use std::path::PathBuf;

#[derive(RustEmbed)]
#[folder = "bin"]
struct WinRing0Binaries;

pub fn extract_winring0() -> Result<PathBuf, String> {
    let dll_name = if cfg!(target_pointer_width = "64") {
        "WinRing0x64.dll"
    } else {
        "WinRing0.dll"
    };

    let embedded = WinRing0Binaries::get(dll_name)
        .or_else(|| WinRing0Binaries::get("WinRing0.dll"))
        .ok_or_else(|| format!("{} not found in embedded binaries", dll_name))?;

    let temp_dir = std::env::temp_dir().join("XiaomiPcManagerLite").join("bin");
    std::fs::create_dir_all(&temp_dir)
        .map_err(|e| format!("create temp dir: {}", e))?;

    let dll_path = temp_dir.join(dll_name);
    std::fs::write(&dll_path, &embedded.data)
        .map_err(|e| format!("write {}: {}", dll_name, e))?;

    log::info!("Extracted {} to {:?}", dll_name, dll_path);
    Ok(dll_path)
}

pub fn cleanup_temp() {
    let temp_dir = std::env::temp_dir().join("XiaomiPcManagerLite").join("bin");
    if temp_dir.exists() {
        let _ = std::fs::remove_dir_all(&temp_dir);
        log::info!("Cleaned up temp WinRing0 binaries");
    }
}
