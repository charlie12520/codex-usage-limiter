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
pub(crate) fn quota_guard_breach_body(
    window_name: &str,
    observed_percent: u8,
    threshold_percent: u8,
    reset_time: &str,
) -> String {
    format!(
        "{window_name}: {observed_percent}% used (threshold {threshold_percent}%). Resets {reset_time}."
    )
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
    route: &str,
) -> Result<(), String> {
    use tauri::{Emitter, Manager};
    use tauri_winrt_notification::Toast;

    let activation_argument = format!("{QUOTA_GUARD_NOTIFICATION_OPEN_ACTION}:{route}");
    let activation_app = app.clone();
    Toast::new(&app.config().identifier)
        .title(title)
        .text1(body)
        .add_button("Open usage limiter", &activation_argument)
        .on_activated(move |action| {
            if is_quota_guard_windows_activation(action.as_deref(), &activation_argument) {
                if let Some(window) = activation_app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                    let _ = window.emit("quota-guard-open-panel", ());
                } else {
                    let _ = activation_app.emit("quota-guard-open-panel", ());
                }
            }
            Ok(())
        })
        .show()
        .map_err(|error| error.to_string())
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
            "five-hour window: 92% used (threshold 90%). Resets at 15:30."
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
