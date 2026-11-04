//! Agent Companion host boundary. wb-switch owns the tray and enable switch;
//! the shared plugin owns its windows and monitor connection.

use tauri::AppHandle;

fn runtime_path() -> std::path::PathBuf {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let target = env!("AGENT_COMPANION_TARGET");
    let dev_name = format!("agent-studio-runtime-{target}{suffix}");

    if let Some(dir) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|p| p.to_path_buf())) {
        // 安装后：sidecar 与主程序同目录
        let bundled = dir.join(format!("agent-studio-runtime{suffix}"));
        if bundled.is_file() {
            return bundled;
        }
        // 开发态：exe 在 target/<profile>/，往上两级是仓库根，sidecar 在 src-tauri/binaries/
        //
        // 这里刻意不用 env!("CARGO_MANIFEST_DIR")：它会把**构建机**的绝对路径
        // 原样内联进二进制，发布出去的 exe 用 strings 就能翻出用户名和目录结构。
        // 而 --remap-path-prefix 管不到它（那是给 rustc 生成的路径用的，压不进
        // env! 展开的字面量）。改成运行时推导，发布包里就不会留编译机路径。
        let dev = dir
            .join("..")
            .join("..")
            .join("src-tauri")
            .join("binaries")
            .join(&dev_name);
        if dev.is_file() {
            return dev;
        }
    }

    // 都找不到时给个相对路径，交给上层按「sidecar 缺失」报错
    std::path::PathBuf::from("binaries").join(dev_name)
}

pub fn config() -> agent_studio_desktop::Config {
    agent_studio_desktop::Config {
        assets: "companion/".into(),
        runtime: runtime_path(),
        manage_autostart: false,
        default_enabled: false,
        show_rail: true,
    }
}

pub fn enabled(app: &AppHandle) -> bool {
    #[cfg(not(desktop))]
    {
        let _ = app;
        return false;
    }
    #[cfg(desktop)]
    {
        !crate::is_screenshot_demo() && agent_studio_desktop::is_enabled(app)
    }
}

#[tauri::command]
pub fn get_companion_enabled(app: AppHandle) -> bool {
    enabled(&app)
}

#[tauri::command]
pub async fn set_companion_enabled(app: AppHandle, enabled: bool) -> Result<bool, String> {
    #[cfg(not(desktop))]
    {
        let _ = (app, enabled);
        return Err("此平台不支持悬浮窗".into());
    }
    #[cfg(desktop)]
    {
        if crate::is_screenshot_demo() {
            return Err("演示模式不启用悬浮窗".into());
        }
        let host = app.clone();
        let confirmed = tauri::async_runtime::spawn_blocking(move || {
            agent_studio_desktop::set_enabled(&host, enabled)
        })
        .await
        .map_err(|error| error.to_string())??;
        crate::tray::refresh_tray_menu(&app);
        Ok(confirmed)
    }
}

#[tauri::command]
pub async fn open_companion_settings(app: AppHandle) -> Result<(), String> {
    #[cfg(not(desktop))]
    {
        let _ = app;
        return Err("此平台不支持悬浮窗".into());
    }
    #[cfg(desktop)]
    {
        if !enabled(&app) {
            return Err("请先启用悬浮窗".into());
        }
        tauri::async_runtime::spawn_blocking(move || agent_studio_desktop::open(&app, "settings"))
            .await
            .map_err(|error| error.to_string())?
    }
}

#[cfg(desktop)]
pub fn toggle_rail(app: &AppHandle) {
    if enabled(app) {
        agent_studio_desktop::toggle_rail(app);
    }
}

#[cfg(desktop)]
pub fn open_settings_from_tray(app: &AppHandle) {
    if !enabled(app) {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let _ = agent_studio_desktop::open(&app, "settings");
    });
}
