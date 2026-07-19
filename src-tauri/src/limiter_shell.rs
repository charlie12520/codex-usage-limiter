//! Shell integration for the standalone limiter: tray icon, close-to-tray
//! restore, and start-at-login. Desktop-only features degrade to inert
//! commands on mobile so the invoke surface stays uniform.

use tauri::AppHandle;

pub(crate) const TRAY_ID: &str = "limiter-tray";

#[cfg(desktop)]
pub(crate) fn init_tray(app: &AppHandle) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    use tauri::Manager;

    let show = MenuItem::with_id(app, "limiter-show", "Show", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "limiter-quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(app.default_window_icon().expect("bundled window icon").clone())
        .tooltip("Codex Usage Limiter")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            "limiter-show" => show_main_window(app),
            "limiter-quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

#[cfg(desktop)]
fn show_main_window(app: &AppHandle) {
    use tauri::Manager;
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Frontend pushes the current usage summary here on every quota update so
/// hovering the tray icon answers "how much is left" without opening the app.
#[tauri::command]
pub(crate) fn set_tray_usage_tooltip(app: AppHandle, tooltip: String) {
    #[cfg(desktop)]
    {
        use tauri::Manager;
        if let Some(tray) = app.tray_by_id(TRAY_ID) {
            let _ = tray.set_tooltip(Some(tooltip));
        }
        return;
    }
    #[allow(unreachable_code)]
    {
        let _ = (app, tooltip);
    }
}

/// The path launched at login. Inside an AppImage, current_exe points at the
/// extracted mount, so prefer the persistent AppImage path.
#[cfg(desktop)]
fn launch_path() -> Result<String, String> {
    if let Ok(appimage) = std::env::var("APPIMAGE") {
        if !appimage.is_empty() {
            return Ok(appimage);
        }
    }
    std::env::current_exe()
        .map(|path| path.display().to_string())
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "windows")]
mod autostart_impl {
    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "CodexUsageLimiter";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub(super) fn get() -> bool {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("reg")
            .args(["query", RUN_KEY, "/v", VALUE])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    pub(super) fn set(enabled: bool) -> Result<(), String> {
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new("reg");
        if enabled {
            let exe = format!("\"{}\"", super::launch_path()?);
            command.args(["add", RUN_KEY, "/v", VALUE, "/t", "REG_SZ", "/d", &exe, "/f"]);
        } else {
            command.args(["delete", RUN_KEY, "/v", VALUE, "/f"]);
        }
        let output = command
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|error| error.to_string())?;
        // Deleting an absent value fails in reg.exe; disabled-when-absent is
        // the desired end state, so only enable failures surface.
        if enabled && !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod autostart_impl {
    fn plist_path() -> Result<std::path::PathBuf, String> {
        let home = std::env::var("HOME").map_err(|error| error.to_string())?;
        Ok(std::path::PathBuf::from(home)
            .join("Library/LaunchAgents/com.codexusagelimiter.desktop.plist"))
    }

    pub(super) fn get() -> bool {
        plist_path().map(|path| path.exists()).unwrap_or(false)
    }

    pub(super) fn set(enabled: bool) -> Result<(), String> {
        let path = plist_path()?;
        if !enabled {
            if path.exists() {
                std::fs::remove_file(&path).map_err(|error| error.to_string())?;
            }
            return Ok(());
        }
        let exe = super::launch_path()?;
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.codexusagelimiter.desktop</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#
        );
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        std::fs::write(&path, plist).map_err(|error| error.to_string())
    }
}

#[cfg(all(desktop, not(any(target_os = "windows", target_os = "macos"))))]
mod autostart_impl {
    fn desktop_entry_path() -> Result<std::path::PathBuf, String> {
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|_| {
                std::env::var("HOME")
                    .map(|home| std::path::PathBuf::from(home).join(".config"))
            })
            .map_err(|error| error.to_string())?;
        Ok(base.join("autostart/codex-usage-limiter.desktop"))
    }

    pub(super) fn get() -> bool {
        desktop_entry_path().map(|path| path.exists()).unwrap_or(false)
    }

    pub(super) fn set(enabled: bool) -> Result<(), String> {
        let path = desktop_entry_path()?;
        if !enabled {
            if path.exists() {
                std::fs::remove_file(&path).map_err(|error| error.to_string())?;
            }
            return Ok(());
        }
        let exe = super::launch_path()?;
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=Codex Usage Limiter\nExec=\"{exe}\"\nX-GNOME-Autostart-enabled=true\n"
        );
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        std::fs::write(&path, entry).map_err(|error| error.to_string())
    }
}

#[tauri::command]
pub(crate) fn get_autostart() -> bool {
    #[cfg(desktop)]
    {
        return autostart_impl::get();
    }
    #[allow(unreachable_code)]
    false
}

#[tauri::command]
pub(crate) fn set_autostart(enabled: bool) -> Result<(), String> {
    #[cfg(desktop)]
    {
        return autostart_impl::set(enabled);
    }
    #[allow(unreachable_code)]
    {
        let _ = enabled;
        Err("Start at login is not supported on this platform.".into())
    }
}
