use rust_embed::RustEmbed;
use std::path::PathBuf;

#[derive(RustEmbed)]
#[folder = "bin"]
struct WinRing0Binaries;

pub fn extract_winring0() -> Result<PathBuf, String> {
    let (dll_name, sys_name) = if cfg!(target_pointer_width = "64") {
        ("WinRing0x64.dll", "WinRing0x64.sys")
    } else {
        ("WinRing0.dll", "WinRing0.sys")
    };

    let embedded_dll = WinRing0Binaries::get(dll_name)
        .or_else(|| WinRing0Binaries::get("WinRing0.dll"))
        .ok_or_else(|| format!("{} not found in embedded binaries", dll_name))?;

    let embedded_sys = WinRing0Binaries::get(sys_name)
        .or_else(|| WinRing0Binaries::get("WinRing0.sys"))
        .ok_or_else(|| format!("{} not found in embedded binaries", sys_name))?;

    let target_dir = std::env::current_exe()
        .map_err(|e| format!("current_exe: {}", e))?
        .parent()
        .ok_or("no parent directory")?
        .to_path_buf();

    // Clean up old extraction locations from previous versions
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join("XiaomiPcManagerLite"));
    let old_sys_dir = std::path::PathBuf::from(
        std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into()),
    )
    .join("Temp");
    let _ = std::fs::remove_file(old_sys_dir.join(dll_name));
    let _ = std::fs::remove_file(old_sys_dir.join(sys_name));

    // Remove stale files at the target location, retry once if handles linger
    for retry in 0..2 {
        let _ = std::fs::remove_file(target_dir.join(dll_name));
        let _ = std::fs::remove_file(target_dir.join(sys_name));
        if retry == 0 {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }

    let dll_path = target_dir.join(dll_name);
    std::fs::write(&dll_path, &embedded_dll.data)
        .map_err(|e| format!("write {}: {}", dll_name, e))?;

    let sys_path = target_dir.join(sys_name);
    std::fs::write(&sys_path, &embedded_sys.data)
        .map_err(|e| format!("write {}: {}", sys_name, e))?;

    log::info!("Extracted {} + {} to {:?}", dll_name, sys_name, target_dir);
    Ok(dll_path)
}

