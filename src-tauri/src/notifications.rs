#[cfg(all(target_os = "macos", debug_assertions))]
use std::process::Command;



/// The native notification action category used by the quota guard.
///
/// `tauri-plugin-notification` action registration is a mobile JavaScript
/// bridge, so the frontend registers this category there. Windows uses the
/// installed WinRT toast API to preserve the same activation contract.
pub(crate) const QUOTA_GUARD_NOTIFICATION_ACTION_TYPE: &str = "codex-usage-limiter.quota-guard";
pub(crate) const QUOTA_GUARD_NOTIFICATION_OPEN_ACTION: &str = "open-quota-guard";
pub(crate) const QUOTA_GUARD_NOTIFICATION_ROUTE_KEY: &str = "quotaGuardRoute";

/// Formats the breach details that make a quota notification actionable.
/// Speaks in "% remaining" to match the limiter UI; inputs are used-%.
pub(crate) fn quota_guard_breach_body(
    window_name: &str,
    observed_percent: u8,
    threshold_percent: u8,
    reset_time: &str,
) -> String {
    let remaining = 100u8.saturating_sub(observed_percent);
    let floor = 100u8.saturating_sub(threshold_percent);
    format!("{window_name}: {remaining}% left (floor {floor}%). Resets {reset_time}.")
}

/// Delivers a durable quota-breach notification.
///
/// Delivery is best-effort and never controls the quota guard state. Call this
/// only after the policy runtime has persisted its breached transition.
pub(crate) fn notify_quota_breach(
    app: &tauri::AppHandle,
    title: &str,
    body: &str,
    route: &str,
) -> Result<(), String> {
    send_quota_guard_notification(app, title, body, route)
}

/// Delivers an availability notification after the policy runtime has
/// durably verified the Ready transition.
pub(crate) fn notify_quota_available(
    app: &tauri::AppHandle,
    title: &str,
    body: &str,
    route: &str,
) -> Result<(), String> {
    send_quota_guard_notification(app, title, body, route)
}

fn send_quota_guard_notification(
    app: &tauri::AppHandle,
    title: &str,
    body: &str,
    route: &str,
) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        return send_windows_quota_guard_notification(app, title, body, route);
    }

    #[cfg(not(target_os = "windows"))]
    {
        use tauri_plugin_notification::NotificationExt;

        app.notification()
            .builder()
            .title(title)
            .body(body)
            .action_type_id(QUOTA_GUARD_NOTIFICATION_ACTION_TYPE)
            .extra(QUOTA_GUARD_NOTIFICATION_ROUTE_KEY, route)
            .show()
            .map_err(|error| error.to_string())
    }
}

#[cfg(target_os = "windows")]
fn is_quota_guard_windows_activation(action: Option<&str>, activation_argument: &str) -> bool {
    action.map_or(true, |value| value == activation_argument)
}

#[cfg(target_os = "windows")]
fn send_windows_quota_guard_notification(
    app: &tauri::AppHandle,
    title: &str,
    body: &str,
    _route: &str,
) -> Result<(), String> {
    use windows::core::HSTRING;
    use windows::UI::Notifications::{
        ToastNotification, ToastNotificationManager, ToastTemplateType,
    };

    // Uses the legacy ToastText02 template on purpose: Windows banners legacy
    // templates for registry-registered AUMIDs, but silently routes modern
    // ToastGeneric payloads from unpackaged apps straight to the notification
    // center without ever showing a banner (verified empirically 2026-07-18).
    let err = |error: windows::core::Error| error.to_string();
    let xml = ToastNotificationManager::GetTemplateContent(ToastTemplateType::ToastText02)
        .map_err(err)?;
    let texts = xml.GetElementsByTagName(&HSTRING::from("text")).map_err(err)?;
    texts
        .Item(0)
        .map_err(err)?
        .AppendChild(&xml.CreateTextNode(&HSTRING::from(title)).map_err(err)?)
        .map_err(err)?;
    texts
        .Item(1)
        .map_err(err)?
        .AppendChild(&xml.CreateTextNode(&HSTRING::from(body)).map_err(err)?)
        .map_err(err)?;
    let toast = ToastNotification::CreateToastNotification(&xml).map_err(err)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(
        app.config().identifier.as_str(),
    ))
    .map_err(err)?
    .Show(&toast)
    .map_err(err)
}

/// Registers this app's AppUserModelId in HKCU so WinRT toasts actually
/// display. Unpackaged exes have no installer to do this; without the entry
/// Windows accepts and then silently drops every toast.
#[cfg(target_os = "windows")]
pub(crate) fn register_windows_toast_identity(identifier: &str, display_name: &str) {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let key = format!(r"HKCU\Software\Classes\AppUserModelId\{identifier}");
    let _ = Command::new("reg")
        .args(["add", &key, "/v", "DisplayName", "/t", "REG_SZ", "/d", display_name, "/f"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}
#[tauri::command]
pub(crate) async fn is_macos_debug_build() -> bool {
    cfg!(all(target_os = "macos", debug_assertions))
}

#[tauri::command]
pub(crate) async fn app_build_type() -> String {
    if cfg!(debug_assertions) {
        "debug".to_string()
    } else {
        "release".to_string()
    }
}

/// macOS dev-mode fallback for system notifications.
///
/// In `tauri dev` (debug assertions enabled), the app is typically run as a
/// bare binary instead of a bundled `.app`. macOS notifications can silently
/// fail in that mode because the process does not have a stable bundle
/// identifier registered with the system notification center.
///
/// This fallback uses AppleScript via `osascript` so the developer still gets
/// a visible notification during local development.
#[tauri::command]
pub(crate) async fn send_notification_fallback(title: String, body: String) -> Result<(), String> {
    #[cfg(all(target_os = "macos", debug_assertions))]
    {
        let escape = |value: &str| value.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            escape(&body),
            escape(&title)
        );

        let status = Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(script)
            .status()
            .map_err(|error| format!("Failed to run osascript: {error}"))?;

        if status.success() {
            Ok(())
        } else {
            Err(format!("osascript exited with status: {status}"))
        }
    }

    #[cfg(not(all(target_os = "macos", debug_assertions)))]
    {
        let _ = (title, body);
        Err("Notification fallback is only available on macOS debug builds.".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breach_body_names_the_window_usage_threshold_and_reset_time() {
        assert_eq!(
            quota_guard_breach_body("five-hour window", 92, 90, "at 15:30"),
            "five-hour window: 8% left (floor 10%). Resets at 15:30."
        );
    }

    #[test]
    fn quota_notifications_use_a_stable_action_and_route_keys() {
        assert_eq!(QUOTA_GUARD_NOTIFICATION_ACTION_TYPE, "codex-usage-limiter.quota-guard");
        assert_eq!(QUOTA_GUARD_NOTIFICATION_OPEN_ACTION, "open-quota-guard");
        assert_eq!(QUOTA_GUARD_NOTIFICATION_ROUTE_KEY, "quotaGuardRoute");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_default_tap_and_named_open_action_activate_the_panel() {
        let named_action = "open-quota-guard:quota-guard";
        assert!(is_quota_guard_windows_activation(None, named_action));
        assert!(is_quota_guard_windows_activation(Some(named_action), named_action));
        assert!(!is_quota_guard_windows_activation(Some("dismiss"), named_action));
    }
}
