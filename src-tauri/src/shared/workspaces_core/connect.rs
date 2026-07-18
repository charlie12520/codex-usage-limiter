use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::backend::app_server::WorkspaceSession;
use crate::codex::args::resolve_workspace_codex_args;
use crate::codex::home::resolve_workspace_codex_home;
use crate::shared::process_core::kill_child_process_tree;
use crate::types::{AppSettings, WorkspaceEntry};

use super::helpers::resolve_entry_and_parent;

static CONNECT_WORKSPACE_SPAWN_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(super) fn workspace_session_spawn_lock() -> &'static Mutex<()> {
    CONNECT_WORKSPACE_SPAWN_LOCK.get_or_init(|| Mutex::new(()))
}

async fn session_process_is_alive(session: &Arc<WorkspaceSession>) -> bool {
    let mut child = session.child.lock().await;
    matches!(child.try_wait(), Ok(None))
}

pub(crate) async fn bind_workspace_session(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    session: Arc<WorkspaceSession>,
    workspace_id: &str,
    workspace_path: Option<&str>,
) {
    let previous = {
        let sessions = sessions.lock().await;
        sessions.get(workspace_id).cloned()
    };
    if previous
        .as_ref()
        .is_some_and(|previous| !Arc::ptr_eq(previous, &session))
    {
        previous.unwrap().revoke_workspace(workspace_id).await;
    }
    sessions
        .lock()
        .await
        .insert(workspace_id.to_string(), Arc::clone(&session));
    session.bind_workspace(workspace_id, workspace_path).await;
}

pub(crate) async fn revoke_and_unbind_workspace(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: &str,
) -> Option<Arc<WorkspaceSession>> {
    let session = {
        let sessions = sessions.lock().await;
        sessions.get(workspace_id).cloned()
    }?;
    session.revoke_workspace(workspace_id).await;
    let mut sessions = sessions.lock().await;
    if sessions
        .get(workspace_id)
        .is_some_and(|candidate| Arc::ptr_eq(candidate, &session))
    {
        sessions.remove(workspace_id);
    }
    Some(session)
}

/// Replaces every mapping that still points at `old_session` without allowing
/// the retired epoch to admit a new inference request.  The caller owns the
/// spawn lock; this function deliberately does not hold the session map while
/// revoking permits or waiting for already-admitted writes to register.
pub(crate) async fn swap_workspace_session(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    old_session: &Arc<WorkspaceSession>,
    new_session: Arc<WorkspaceSession>,
    workspace_paths: &[(String, Option<String>)],
) {
    let replacing: Vec<(String, Option<String>)> = {
        let sessions = sessions.lock().await;
        workspace_paths
            .iter()
            .filter(|(workspace_id, _)| {
                sessions
                    .get(workspace_id)
                    .is_some_and(|candidate| Arc::ptr_eq(candidate, old_session))
            })
            .cloned()
            .collect()
    };

    // Revoke before changing any map entry.  A retained Arc to the old
    // session therefore fails admission even while callers are observing the
    // transition.
    for (workspace_id, _) in &replacing {
        old_session.revoke_workspace(workspace_id).await;
    }
    if let Some(gate) = &old_session.quota_gate {
        while gate.active_admissions() != 0 {
            tokio::task::yield_now().await;
        }
    }

    {
        let mut sessions = sessions.lock().await;
        for (workspace_id, _) in &replacing {
            // Do not overwrite a mapping that a newer serialized operation
            // has already replaced.
            if sessions
                .get(workspace_id)
                .is_some_and(|candidate| Arc::ptr_eq(candidate, old_session))
            {
                sessions.insert(workspace_id.clone(), Arc::clone(&new_session));
            }
        }
    }
    for (workspace_id, workspace_path) in replacing {
        new_session
            .bind_workspace(&workspace_id, workspace_path.as_deref())
            .await;
    }
}

async fn remove_session_references(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    session: &Arc<WorkspaceSession>,
) {
    let ids: Vec<String> = {
        let sessions = sessions.lock().await;
        sessions
            .iter()
            .filter(|(_, candidate)| Arc::ptr_eq(candidate, session))
            .map(|(workspace_id, _)| workspace_id.clone())
            .collect()
    };
    for workspace_id in ids {
        let _ = revoke_and_unbind_workspace(sessions, &workspace_id).await;
    }
}

pub(super) async fn take_live_shared_session(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
) -> Option<Arc<WorkspaceSession>> {
    loop {
        let existing_session = {
            let sessions = sessions.lock().await;
            sessions.values().next().cloned()
        };
        let Some(existing_session) = existing_session else {
            return None;
        };
        if session_process_is_alive(&existing_session).await {
            return Some(existing_session);
        }
        remove_session_references(sessions, &existing_session).await;
    }
}

pub(crate) async fn connect_workspace_core<F, Fut>(
    workspace_id: String,
    workspaces: &Mutex<HashMap<String, WorkspaceEntry>>,
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    app_settings: &Mutex<AppSettings>,
    spawn_session: F,
) -> Result<(), String>
where
    F: Fn(WorkspaceEntry, Option<String>, Option<String>, Option<PathBuf>) -> Fut,
    Fut: Future<Output = Result<Arc<WorkspaceSession>, String>>,
{
    let (entry, parent_entry) = resolve_entry_and_parent(workspaces, &workspace_id).await?;
    let _spawn_guard = workspace_session_spawn_lock().lock().await;
    if let Some(existing_for_entry) = {
        let sessions = sessions.lock().await;
        sessions.get(&entry.id).cloned()
    } {
        if session_process_is_alive(&existing_for_entry).await {
            return Ok(());
        }
        remove_session_references(sessions, &existing_for_entry).await;
    }
    if let Some(existing_session) = take_live_shared_session(sessions).await {
        bind_workspace_session(
            sessions,
            existing_session,
            &entry.id,
            Some(&entry.path),
        )
        .await;
        return Ok(());
    }
    let (default_bin, codex_args) = {
        let settings = app_settings.lock().await;
        (
            settings.codex_bin.clone(),
            resolve_workspace_codex_args(&entry, parent_entry.as_ref(), Some(&settings)),
        )
    };
    let codex_home = resolve_workspace_codex_home(&entry, parent_entry.as_ref());
    let session = spawn_session(entry.clone(), default_bin, codex_args, codex_home).await?;
    bind_workspace_session(sessions, session, &entry.id, Some(&entry.path)).await;
    Ok(())
}

pub(super) async fn kill_session_by_id(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    id: &str,
) {
    let Some(removed) = revoke_and_unbind_workspace(sessions, id).await else {
        return;
    };
    let still_referenced = {
        let sessions = sessions.lock().await;
        sessions
            .values()
            .any(|candidate| Arc::ptr_eq(candidate, &removed))
    };
    if still_referenced {
        return;
    }
    let mut child = removed.child.lock().await;
    kill_child_process_tree(&mut child).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::{HashMap, HashSet};
    use std::process::Stdio;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::process::Command;
    use tokio::sync::Mutex;

    use crate::types::{WorkspaceKind, WorkspaceSettings};
    use crate::shared::quota_guard::gate::{ProcessGate, ProcessPolicy};

    fn make_workspace_entry(id: &str) -> WorkspaceEntry {
        WorkspaceEntry {
            id: id.to_string(),
            name: id.to_string(),
            path: "/tmp".to_string(),
            kind: WorkspaceKind::Main,
            parent_id: None,
            worktree: None,
            settings: WorkspaceSettings::default(),
        }
    }

    fn make_session(_entry: WorkspaceEntry) -> Arc<WorkspaceSession> {
        let mut cmd = if cfg!(windows) {
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", "more"]);
            cmd
        } else {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "cat"]);
            cmd
        };

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let mut child = cmd.spawn().expect("spawn dummy child");
        let stdin = child.stdin.take().expect("dummy child stdin");

        Arc::new(WorkspaceSession {
            codex_args: None,
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            request_context: Mutex::new(HashMap::new()),
            thread_workspace: Mutex::new(HashMap::new()),
            hidden_thread_ids: Mutex::new(HashSet::new()),
            next_id: AtomicU64::new(0),
            background_thread_callbacks: Mutex::new(HashMap::new()),
            owner_workspace_id: "test-owner".to_string(),
            workspace_ids: Mutex::new(HashSet::from(["test-owner".to_string()])),
            workspace_roots: Mutex::new(HashMap::new()),
            bound_workspace_ids: Mutex::new(HashSet::new()),
            session_epoch: "test-epoch".to_string(),
            canonical_codex_home: "test-home".to_string(),
            quota_guard: None,
            quota_gate: Some(ProcessGate::default()),
        })
    }

    #[test]
    fn connect_workspace_is_noop_when_already_connected() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let entry = make_workspace_entry("ws-1");
            let workspaces = Mutex::new(HashMap::from([(entry.id.clone(), entry.clone())]));
            let sessions = Mutex::new(HashMap::from([(
                entry.id.clone(),
                make_session(entry.clone()),
            )]));
            let app_settings = Mutex::new(AppSettings::default());
            let spawn_calls = Arc::new(AtomicUsize::new(0));
            let spawn_calls_ref = spawn_calls.clone();

            connect_workspace_core(
                entry.id.clone(),
                &workspaces,
                &sessions,
                &app_settings,
                move |_entry, _default_bin, _codex_args, _codex_home| {
                    let spawn_calls_ref = spawn_calls_ref.clone();
                    async move {
                        spawn_calls_ref.fetch_add(1, Ordering::SeqCst);
                        Err("should not spawn".to_string())
                    }
                },
            )
            .await
            .expect("connect should be noop");

            assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);
            kill_session_by_id(&sessions, &entry.id).await;
        });
    }

    #[test]
    fn connect_workspace_spawns_when_not_connected() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let entry = make_workspace_entry("ws-2");
            let workspaces = Mutex::new(HashMap::from([(entry.id.clone(), entry.clone())]));
            let sessions = Mutex::new(HashMap::<String, Arc<WorkspaceSession>>::new());
            let app_settings = Mutex::new(AppSettings::default());
            let spawn_calls = Arc::new(AtomicUsize::new(0));
            let spawn_calls_ref = spawn_calls.clone();
            let entry_for_spawn = entry.clone();

            connect_workspace_core(
                entry.id.clone(),
                &workspaces,
                &sessions,

                &app_settings,
                move |_entry, _default_bin, _codex_args, _codex_home| {
                    let spawn_calls_ref = spawn_calls_ref.clone();
                    let entry_for_spawn = entry_for_spawn.clone();
                    async move {
                        spawn_calls_ref.fetch_add(1, Ordering::SeqCst);
                        Ok(make_session(entry_for_spawn))
                    }
                },
            )
            .await
            .expect("connect should spawn");

            assert_eq!(spawn_calls.load(Ordering::SeqCst), 1);
            assert!(sessions.lock().await.contains_key(&entry.id));
            kill_session_by_id(&sessions, &entry.id).await;
        });
    }

    #[test]
    fn replacement_revokes_retained_old_epoch_before_mapping_fresh_closed_epoch() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let entry = make_workspace_entry("ws-replace");
            let sessions = Mutex::new(HashMap::<String, Arc<WorkspaceSession>>::new());
            let mut old = make_session(entry.clone());
            Arc::get_mut(&mut old)
                .expect("old session is not shared")
                .session_epoch = "old-epoch".to_string();
            let old_gate = old.quota_gate.as_ref().expect("old gate").clone();
            old_gate.set_policy(ProcessPolicy::EnabledOpen);
            bind_workspace_session(&sessions, Arc::clone(&old), &entry.id, Some(&entry.path))
                .await;
            old_gate.set_epoch_open(old.session_epoch(), &entry.id, true);

            let mut fresh = make_session(entry.clone());
            Arc::get_mut(&mut fresh)
                .expect("fresh session is not shared")
                .session_epoch = "fresh-epoch".to_string();
            let fresh_gate = fresh.quota_gate.as_ref().expect("fresh gate").clone();
            fresh_gate.set_policy(ProcessPolicy::EnabledOpen);

            swap_workspace_session(
                &sessions,
                &old,
                Arc::clone(&fresh),
                &[(entry.id.clone(), Some(entry.path.clone()))],
            )
            .await;

            assert!(!old_gate.status(Some(old.session_epoch()), &entry.id).open);
            assert!(Arc::ptr_eq(
                sessions
                    .lock()
                    .await
                    .get(&entry.id)
                    .expect("fresh mapping committed"),
                &fresh
            ));
            assert!(fresh.is_workspace_bound(&entry.id).await);
            assert!(!fresh_gate.status(Some(fresh.session_epoch()), &entry.id).open);

            kill_session_by_id(&sessions, &entry.id).await;
            let mut old_child = old.child.lock().await;
            kill_child_process_tree(&mut old_child).await;
        });
    }

    #[test]
    fn bind_inserts_before_committing_and_is_idempotent() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let entry = make_workspace_entry("ws-bind");
            let sessions = Mutex::new(HashMap::<String, Arc<WorkspaceSession>>::new());
            let session = make_session(entry.clone());
            let gate = session.quota_gate.as_ref().expect("fixture gate").clone();
            gate.set_policy(ProcessPolicy::EnabledOpen);

            bind_workspace_session(&sessions, Arc::clone(&session), &entry.id, Some(&entry.path))
                .await;
            assert!(Arc::ptr_eq(
                sessions
                    .lock()
                    .await
                    .get(&entry.id)
                    .expect("session inserted before bind returns"),
                &session
            ));
            assert!(session.is_workspace_bound(&entry.id).await);
            assert!(!gate.status(Some(session.session_epoch()), &entry.id).open);

            bind_workspace_session(&sessions, Arc::clone(&session), &entry.id, Some(&entry.path))
                .await;
            assert_eq!(session.bound_workspace_ids.lock().await.len(), 1);

            gate.set_epoch_open(session.session_epoch(), &entry.id, true);
            let removed = revoke_and_unbind_workspace(&sessions, &entry.id)
                .await
                .expect("bound session removed");
            assert!(Arc::ptr_eq(&removed, &session));
            assert!(!gate.status(Some(session.session_epoch()), &entry.id).open);
            let mut child = session.child.lock().await;
            kill_child_process_tree(&mut child).await;
        });
    }

    #[test]
    fn inference_is_blocked_before_request_registration() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let entry = make_workspace_entry("ws-blocked");
            let sessions = Mutex::new(HashMap::<String, Arc<WorkspaceSession>>::new());
            let session = make_session(entry.clone());
            bind_workspace_session(&sessions, Arc::clone(&session), &entry.id, Some(&entry.path))
                .await;
            session
                .quota_gate
                .as_ref()
                .expect("fixture gate")
                .set_policy(ProcessPolicy::EnabledClosed);

            let error = session
                .send_request_for_workspace(
                    &entry.id,
                    "turn/start",
                    serde_json::json!({ "threadId": "thread-1" }),
                )
                .await
                .expect_err("closed guard blocks inference");
            assert!(error.starts_with("QUOTA_GUARD_BLOCKED|"));
            assert!(session.pending.lock().await.is_empty());
            assert!(session.request_context.lock().await.is_empty());

            kill_session_by_id(&sessions, &entry.id).await;
        });
    }
}
