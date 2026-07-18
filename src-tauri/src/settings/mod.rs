use std::path::PathBuf;

use tokio::sync::Mutex;

use tauri::{State, Window};

use crate::shared::settings_core::{
    get_app_settings_core, get_codex_config_path_core, update_app_settings_core,
};
use crate::state::AppState;
use crate::types::{AppSettings, BackendMode};
use crate::window;
use crate::shared::quota_guard::coordinator::{QuotaGuardHandle, SettingsChanged};

#[tauri::command]
pub(crate) async fn get_app_settings(
    state: State<'_, AppState>,
    window: Window,
) -> Result<AppSettings, String> {
    let settings = get_app_settings_core(&state.app_settings).await;
    let _ = window::apply_window_appearance(&window, settings.theme.as_str());
    Ok(settings)
}

#[tauri::command]
pub(crate) async fn update_app_settings(
    settings: AppSettings,
    state: State<'_, AppState>,
    window: Window,
) -> Result<AppSettings, String> {
    let (previous, updated) = update_app_settings_transaction(
        settings,
        &state.app_settings,
        &state.settings_path,
        &state.settings_update_lock,
        &state.quota_guard,
    )
    .await?;

    if should_reset_remote_backend(&previous, &updated) {
        *state.remote_backend.lock().await = None;
    }
    ensure_remote_runtime_for_settings(&updated, state.clone()).await;
    let _ = window::apply_window_appearance(&window, updated.theme.as_str());
    Ok(updated)
}
/// Disables quota guarding through the same serialized settings transaction as
/// the settings UI. A successful return means the disabled setting is durable
/// and the quota actor has acknowledged its SettingsChanged event.
pub(crate) async fn disable_quota_guard_and_open(
    state: &AppState,
) -> Result<AppSettings, String> {
    disable_quota_guard_and_open_transaction(
        &state.app_settings,
        &state.settings_path,
        &state.settings_update_lock,
        &state.quota_guard,
    )
    .await
}

async fn disable_quota_guard_and_open_transaction(
    app_settings: &Mutex<AppSettings>,
    settings_path: &PathBuf,
    settings_update_lock: &Mutex<()>,
    quota_guard: &QuotaGuardHandle,
) -> Result<AppSettings, String> {
    let mut settings = get_app_settings_core(app_settings).await;
    settings.quota_guard.enabled = false;
    let (_, updated) = update_app_settings_transaction(
        settings,
        app_settings,
        settings_path,
        settings_update_lock,
        quota_guard,
    )
    .await?;
    Ok(updated)
}


/// Performs the stateful part of a settings update without requiring a window.
///
/// The lock intentionally spans the settings read, validation, durable write,
/// and quota-actor acknowledgement. Those operations decide whether an update
/// is an enable transition, so later requests must see the prior request's
/// effective settings before making that decision.
async fn update_app_settings_transaction(
    settings: AppSettings,
    app_settings: &Mutex<AppSettings>,
    settings_path: &PathBuf,
    settings_update_lock: &Mutex<()>,
    quota_guard: &QuotaGuardHandle,
) -> Result<(AppSettings, AppSettings), String> {
    let _settings_transaction = settings_update_lock.lock().await;
    let previous = get_app_settings_core(app_settings).await;
    let prior_policy = quota_guard.gate().policy();
    let enabling = !previous.quota_guard.enabled && settings.quota_guard.enabled;

    // The closed admission barrier precedes even validation: a racing start
    // cannot escape an enable request which later proves invalid.
    if enabling {
        quota_guard.begin_enable().await;
    }
    if let Err(error) = validate_quota_guard_settings(&settings)
        .and_then(|()| validate_quota_guard_backend_compatibility(&settings))
    {
        if enabling {
            quota_guard.gate().set_policy(prior_policy);
        }
        return Err(error);
    }

    let updated = match update_app_settings_core(settings, app_settings, settings_path).await {
        Ok(updated) => updated,
        Err(error) => {
            if enabling {
                quota_guard.gate().set_policy(prior_policy);
            }
            return Err(error);
        }
    };

    // The actor must acknowledge the durable setting before it may change
    // admission. If it cannot, restore both the durable and in-memory settings
    // before restoring the prior policy.
    if let Err(error) = quota_guard
        .settings_changed(SettingsChanged {
            previous: previous.quota_guard.clone(),
            updated: updated.quota_guard.clone(),
        })
        .await
    {
        let rollback = update_app_settings_core(previous.clone(), app_settings, settings_path).await;
        quota_guard.gate().set_policy(prior_policy);
        if let Err(rollback_error) = rollback {
            return Err(format!(
                "{error}; quota guard settings rollback failed: {rollback_error}"
            ));
        }
        return Err(error);
    }

    Ok((previous, updated))
}

pub(crate) const QUOTA_GUARD_REMOTE_BACKEND_INCOMPATIBLE: &str =
    "QUOTA_GUARD_REMOTE_BACKEND_INCOMPATIBLE";

pub(crate) fn validate_quota_guard_settings(settings: &AppSettings) -> Result<(), String> {
    let guard = &settings.quota_guard;
    // Thresholds are u8, but retain the explicit boundary contract here so the
    // command remains correct if its serialized representation changes.
    if guard.primary_threshold_percent > 100 || guard.secondary_threshold_percent > 100 {
        return Err("QUOTA_GUARD_INVALID_THRESHOLD_PERCENT".to_string());
    }
    if !(1..=1440).contains(&guard.drain_timeout_minutes) {
        return Err("QUOTA_GUARD_INVALID_DRAIN_TIMEOUT_MINUTES".to_string());
    }
    if guard.reset_grace_minutes > 1440 {
        return Err("QUOTA_GUARD_INVALID_RESET_GRACE_MINUTES".to_string());
    }
    Ok(())
}

pub(crate) fn validate_quota_guard_backend_compatibility(settings: &AppSettings) -> Result<(), String> {
    if settings.quota_guard.enabled && matches!(settings.backend_mode, BackendMode::Remote) {
        return Err(QUOTA_GUARD_REMOTE_BACKEND_INCOMPATIBLE.to_string());
    }
    Ok(())
}

#[tauri::command]
pub(crate) async fn get_codex_config_path() -> Result<String, String> {
    get_codex_config_path_core()
}

fn should_reset_remote_backend(previous: &AppSettings, updated: &AppSettings) -> bool {
    let backend_mode_changed = !matches!(
        (&previous.backend_mode, &updated.backend_mode),
        (
            crate::types::BackendMode::Local,
            crate::types::BackendMode::Local
        ) | (
            crate::types::BackendMode::Remote,
            crate::types::BackendMode::Remote
        )
    );
    backend_mode_changed
        || previous.remote_backend_provider != updated.remote_backend_provider
        || previous.remote_backend_host != updated.remote_backend_host
        || previous.remote_backend_token != updated.remote_backend_token
}

async fn ensure_remote_runtime_for_settings(settings: &AppSettings, state: State<'_, AppState>) {
    if cfg!(any(target_os = "android", target_os = "ios")) {
        return;
    }
    if !matches!(settings.backend_mode, BackendMode::Remote) {
        return;
    }

    let _ = crate::tailscale::tailscale_daemon_start(state).await;
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex as StdMutex, MutexGuard, OnceLock};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use tokio::sync::{mpsc, oneshot, Mutex};
    use tokio::time::timeout;

    use super::{
        disable_quota_guard_and_open_transaction, should_reset_remote_backend,
        update_app_settings_transaction, validate_quota_guard_backend_compatibility,
        validate_quota_guard_settings, QUOTA_GUARD_REMOTE_BACKEND_INCOMPATIBLE,
    };
    use crate::shared::quota_guard::coordinator::{ActorEvent, QuotaGuardHandle};
    use crate::shared::quota_guard::gate::ProcessPolicy;
    use crate::types::{AppSettings, BackendMode};

    fn test_settings_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "codex-monitor-settings-{label}-{}-{nonce}.json",
            std::process::id()
        ))
    }

    static CODEX_HOME_TEST_LOCK: OnceLock<StdMutex<()>> = OnceLock::new();

    struct TestCodexHome {
        prior: Option<OsString>,
        path: PathBuf,
    }

    impl TestCodexHome {
        fn activate(label: &str) -> (MutexGuard<'static, ()>, Self) {
            let lock = CODEX_HOME_TEST_LOCK
                .get_or_init(|| StdMutex::new(()))
                .lock()
                .expect("CODEX_HOME test lock poisoned");
            let path = test_settings_path(label).with_extension("codex-home");
            std::fs::create_dir_all(&path).expect("create isolated CODEX_HOME");
            let prior = std::env::var_os("CODEX_HOME");
            std::env::set_var("CODEX_HOME", &path);
            (lock, Self { prior, path })
        }
    }

    impl Drop for TestCodexHome {
        fn drop(&mut self) {
            match &self.prior {
                Some(value) => std::env::set_var("CODEX_HOME", value),
                None => std::env::remove_var("CODEX_HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn enable_quota_guard() -> AppSettings {
        let mut settings = AppSettings::default();
        settings.quota_guard.enabled = true;
        settings
    }

    #[test]
    fn disable_is_durable_before_the_actor_acknowledges_and_opens_only_afterward() {
        tauri::async_runtime::block_on(async {
            let mut enabled_settings = AppSettings::default();
            enabled_settings.quota_guard.enabled = true;
            let app_settings = Arc::new(Mutex::new(enabled_settings));
            let settings_update_lock = Arc::new(Mutex::new(()));
            let quota_guard = QuotaGuardHandle::default();
            quota_guard
                .gate()
                .set_policy(ProcessPolicy::EnabledClosed);
            let (actor_sender, mut actor_receiver) = mpsc::channel(1);
            *quota_guard
                .inner
                .sender
                .lock()
                .expect("quota actor sender lock poisoned") = Some(actor_sender);
            let settings_path = test_settings_path("disable-durable-before-ack");
            let disable = {
                let app_settings = Arc::clone(&app_settings);
                let settings_update_lock = Arc::clone(&settings_update_lock);
                let quota_guard = quota_guard.clone();
                let settings_path = settings_path.clone();
                tauri::async_runtime::spawn(async move {
                    disable_quota_guard_and_open_transaction(
                        app_settings.as_ref(),
                        &settings_path,
                        settings_update_lock.as_ref(),
                        &quota_guard,
                    )
                    .await
                })
            };

            let event = timeout(Duration::from_secs(2), actor_receiver.recv())
                .await
                .expect("disable did not reach quota actor")
                .expect("quota actor channel closed");
            let (change, reply) = match event {
                ActorEvent::SettingsChanged(change, reply) => (change, reply),
                _ => panic!("expected settings change"),
            };
            assert!(change.previous.enabled);
            assert!(!change.updated.enabled);
            assert!(!app_settings.lock().await.quota_guard.enabled);
            assert!(
                !crate::storage::read_settings(&settings_path)
                    .expect("read durable settings")
                    .quota_guard
                    .enabled
            );
            assert_eq!(
                quota_guard.gate().policy(),
                ProcessPolicy::EnabledClosed,
                "settings persistence must not optimistically open the gate"
            );

            quota_guard.gate().set_policy(ProcessPolicy::DisabledOpen);
            reply
                .send(Ok(()))
                .expect("disable stopped waiting for quota actor");
            assert!(
                disable
                    .await
                    .expect("disable task panicked")
                    .is_ok()
            );
            assert_eq!(quota_guard.gate().policy(), ProcessPolicy::DisabledOpen);
            let _ = std::fs::remove_file(settings_path);
        });
    }

    #[test]
    fn failed_disable_actor_acknowledgement_rolls_back_durable_settings_and_policy() {
        tauri::async_runtime::block_on(async {
            let mut enabled_settings = AppSettings::default();
            enabled_settings.quota_guard.enabled = true;
            let app_settings = Arc::new(Mutex::new(enabled_settings));
            let settings_update_lock = Arc::new(Mutex::new(()));
            let quota_guard = QuotaGuardHandle::default();
            quota_guard
                .gate()
                .set_policy(ProcessPolicy::EnabledClosed);
            let (actor_sender, mut actor_receiver) = mpsc::channel(1);
            *quota_guard
                .inner
                .sender
                .lock()
                .expect("quota actor sender lock poisoned") = Some(actor_sender);
            let settings_path = test_settings_path("disable-actor-rollback");
            let disable = {
                let app_settings = Arc::clone(&app_settings);
                let settings_update_lock = Arc::clone(&settings_update_lock);
                let quota_guard = quota_guard.clone();
                let settings_path = settings_path.clone();
                tauri::async_runtime::spawn(async move {
                    disable_quota_guard_and_open_transaction(
                        app_settings.as_ref(),
                        &settings_path,
                        settings_update_lock.as_ref(),
                        &quota_guard,
                    )
                    .await
                })
            };

            let event = timeout(Duration::from_secs(2), actor_receiver.recv())
                .await
                .expect("disable did not reach quota actor")
                .expect("quota actor channel closed");
            let reply = match event {
                ActorEvent::SettingsChanged(_, reply) => reply,
                _ => panic!("expected settings change"),
            };
            reply
                .send(Err("actor rejected disable".to_string()))
                .expect("disable stopped waiting for quota actor");
            let result = disable
                .await
                .expect("disable task panicked");
            assert_eq!(
                result.err().as_deref(),
                Some("actor rejected disable")
            );
            assert!(app_settings.lock().await.quota_guard.enabled);
            assert!(
                crate::storage::read_settings(&settings_path)
                    .expect("read rolled-back settings")
                    .quota_guard
                    .enabled
            );
            assert_eq!(quota_guard.gate().policy(), ProcessPolicy::EnabledClosed);
            let _ = std::fs::remove_file(settings_path);
        });
    }

    #[test]
    fn settings_transaction_serializes_stale_updates_through_actor_acknowledgement() {
        tauri::async_runtime::block_on(async {
            let app_settings = Arc::new(Mutex::new(AppSettings::default()));
            let settings_update_lock = Arc::new(Mutex::new(()));
            let quota_guard = QuotaGuardHandle::default();
            let (actor_sender, mut actor_receiver) = mpsc::channel(2);
            *quota_guard
                .inner
                .sender
                .lock()
                .expect("quota actor sender lock poisoned") = Some(actor_sender);
            let settings_path = test_settings_path("serialized-stale-updates");

            let first = {
                let app_settings = Arc::clone(&app_settings);
                let settings_update_lock = Arc::clone(&settings_update_lock);
                let quota_guard = quota_guard.clone();
                let settings_path = settings_path.clone();
                tauri::async_runtime::spawn(async move {
                    update_app_settings_transaction(
                        enable_quota_guard(),
                        app_settings.as_ref(),
                        &settings_path,
                        settings_update_lock.as_ref(),
                        &quota_guard,
                    )
                    .await
                })
            };

            let (first_change, first_reply) = match timeout(
                Duration::from_secs(2),
                actor_receiver.recv(),
            )
            .await
            .expect("first update did not reach quota actor")
            .expect("quota actor channel closed")
            {
                ActorEvent::SettingsChanged(change, reply) => (change, reply),
                _ => panic!("expected settings change"),
            };
            assert!(!first_change.previous.enabled);
            assert!(first_change.updated.enabled);

            let (second_started_sender, second_started) = oneshot::channel();
            let second = {
                let app_settings = Arc::clone(&app_settings);
                let settings_update_lock = Arc::clone(&settings_update_lock);
                let quota_guard = quota_guard.clone();
                let settings_path = settings_path.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = second_started_sender.send(());
                    update_app_settings_transaction(
                        enable_quota_guard(),
                        app_settings.as_ref(),
                        &settings_path,
                        settings_update_lock.as_ref(),
                        &quota_guard,
                    )
                    .await
                })
            };
            second_started.await.expect("second update task did not start");

            assert!(
                timeout(Duration::from_millis(100), actor_receiver.recv())
                    .await
                    .is_err(),
                "a stale second update reached the quota actor before the first acknowledgement"
            );

            first_reply
                .send(Ok(()))
                .expect("first update stopped waiting for quota actor");
            assert!(
                first
                    .await
                    .expect("first update task panicked")
                    .is_ok()
            );

            let second_event = timeout(Duration::from_secs(2), actor_receiver.recv())
                .await
                .expect("second update did not reach quota actor")
                .expect("quota actor channel closed");
            let (second_change, second_reply) = match second_event {
                ActorEvent::SettingsChanged(change, reply) => (change, reply),
                _ => panic!("expected settings change"),
            };
            assert!(
                second_change.previous.enabled,
                "the second update must observe the first effective settings"
            );
            assert!(second_change.updated.enabled);
            second_reply
                .send(Ok(()))
                .expect("second update stopped waiting for quota actor");
            assert!(
                second
                    .await
                    .expect("second update task panicked")
                    .is_ok()
            );
            assert!(app_settings.lock().await.quota_guard.enabled);
            let _ = std::fs::remove_file(settings_path);
        });
    }

    #[test]
    fn invalid_enable_restores_the_prior_admission_policy() {
        tauri::async_runtime::block_on(async {
            let app_settings = Mutex::new(AppSettings::default());
            let settings_update_lock = Mutex::new(());
            let quota_guard = QuotaGuardHandle::default();
            quota_guard
                .gate()
                .set_policy(ProcessPolicy::EnabledOpen);
            let mut invalid_settings = enable_quota_guard();
            invalid_settings.quota_guard.drain_timeout_minutes = 0;

            let result = update_app_settings_transaction(
                invalid_settings,
                &app_settings,
                &test_settings_path("invalid-enable"),
                &settings_update_lock,
                &quota_guard,
            )
            .await;
            assert_eq!(
                result.as_ref().err().map(String::as_str),
                Some("QUOTA_GUARD_INVALID_DRAIN_TIMEOUT_MINUTES")
            );
            assert_eq!(
                quota_guard.gate().policy(),
                ProcessPolicy::EnabledOpen,
                "a rejected enable must restore the policy it closed before validation"
            );
            assert!(!app_settings.lock().await.quota_guard.enabled);
        });
    }

    #[test]
    fn failed_enable_write_restores_the_prior_admission_policy() {
        tauri::async_runtime::block_on(async {
            let app_settings = Mutex::new(AppSettings::default());
            let settings_update_lock = Mutex::new(());
            let quota_guard = QuotaGuardHandle::default();
            quota_guard
                .gate()
                .set_policy(ProcessPolicy::EnabledOpen);
            let parent_file = test_settings_path("enable-write-failure");
            std::fs::write(&parent_file, "not a directory").expect("create settings parent file");
            let settings_path = parent_file.join("settings.json");

            assert!(
                update_app_settings_transaction(
                    enable_quota_guard(),
                    &app_settings,
                    &settings_path,
                    &settings_update_lock,
                    &quota_guard,
                )
                .await
                .is_err()
            );
            assert_eq!(
                quota_guard.gate().policy(),
                ProcessPolicy::EnabledOpen,
                "a failed enable write must restore the policy it closed before persistence"
            );
            assert!(!app_settings.lock().await.quota_guard.enabled);
            std::fs::remove_file(parent_file).expect("remove settings parent file");
        });
    }

    #[test]
    fn should_reset_remote_backend_when_provider_changes() {
        let previous = AppSettings::default();
        let mut updated = previous.clone();
        updated.remote_backend_provider = crate::types::RemoteBackendProvider::Tcp;
        updated.remote_backend_host = "remote.example:4732".to_string();
        assert!(should_reset_remote_backend(&previous, &updated));
    }

    #[test]
    fn should_reset_remote_backend_when_transport_token_changes() {
        let previous = AppSettings::default();
        let mut updated = previous.clone();
        updated.remote_backend_token = Some("token-1".to_string());
        assert!(should_reset_remote_backend(&previous, &updated));
    }

    #[test]
    fn should_not_reset_remote_backend_for_non_transport_setting_changes() {
        let previous = AppSettings::default();
        let mut updated = previous.clone();
        updated.theme = "dark".to_string();
        updated.backend_mode = BackendMode::Local;
        assert!(!should_reset_remote_backend(&previous, &updated));
    }

    #[test]
    fn quota_guard_rejects_remote_backend() {
        let mut settings = AppSettings::default();
        settings.quota_guard.enabled = true;
        settings.backend_mode = BackendMode::Remote;
        assert_eq!(
            validate_quota_guard_backend_compatibility(&settings),
            Err(QUOTA_GUARD_REMOTE_BACKEND_INCOMPATIBLE.to_string())
        );
    }

    #[test]
    fn quota_guard_validates_bounded_deadlines() {
        let mut settings = AppSettings::default();
        settings.quota_guard.drain_timeout_minutes = 0;
        assert!(validate_quota_guard_settings(&settings).is_err());
        settings.quota_guard.drain_timeout_minutes = 1;
        settings.quota_guard.reset_grace_minutes = 1440;
        assert!(validate_quota_guard_settings(&settings).is_ok());
    }
}
