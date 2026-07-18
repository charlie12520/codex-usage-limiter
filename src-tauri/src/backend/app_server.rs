use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::env;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::timeout;

use crate::backend::events::{AppServerEvent, EventSink};
use crate::codex::args::parse_codex_args;
use crate::codex::home::resolve_default_codex_home;
use crate::shared::process_core::{kill_child_process_tree, tokio_command};
use crate::shared::quota_guard::coordinator::{QuotaGuardEvent, QuotaGuardEventSink};
use crate::shared::quota_guard::gate::{AdmissionReason, ProcessGate, ProcessPolicy};
use crate::types::WorkspaceEntry;

#[cfg(target_os = "windows")]
use crate::shared::process_core::{build_cmd_c_command, resolve_windows_executable};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

static NEXT_SESSION_EPOCH: AtomicU64 = AtomicU64::new(1);

fn extract_thread_id(value: &Value) -> Option<String> {
    fn extract_from_container(container: Option<&Value>) -> Option<String> {
        let container = container?;
        container
            .get("threadId")
            .or_else(|| container.get("thread_id"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                container
                    .get("thread")
                    .and_then(|thread| thread.get("id"))
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            })
    }

    extract_from_container(value.get("params"))
        .or_else(|| extract_from_container(value.get("result")))
}

fn extract_turn_id(value: &Value) -> Option<String> {
    let containers = [value.get("params"), value.get("result")];
    containers.into_iter().flatten().find_map(|container| {
        container
            .get("turnId")
            .or_else(|| container.get("turn_id"))
            .or_else(|| container.get("turn").and_then(|turn| turn.get("id")))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn extract_review_thread_id(value: &Value) -> Option<String> {
    value
        .get("result")
        .and_then(|result| result.get("reviewThreadId").or_else(|| result.get("review_thread_id")))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn push_thread_id(out: &mut Vec<String>, value: Option<&Value>) {
    let Some(value) = value else {
        return;
    };
    if let Some(thread_id) = value.as_str().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        out.push(thread_id.to_string());
        return;
    }
    if let Some(values) = value.as_array() {
        for entry in values {
            push_thread_id(out, Some(entry));
        }
    }
}

fn extract_related_thread_ids(value: &Value) -> Vec<String> {
    fn collect_agent_thread_ids(value: Option<&Value>, out: &mut Vec<String>) {
        let Some(value) = value else {
            return;
        };
        if let Some(values) = value.as_array() {
            for entry in values {
                collect_agent_thread_ids(Some(entry), out);
            }
            return;
        }
        let Some(record) = value.as_object() else {
            return;
        };
        push_thread_id(
            out,
            record.get("threadId").or_else(|| record.get("thread_id")),
        );
        push_thread_id(out, record.get("id"));
        push_thread_id(
            out,
            record
                .get("thread")
                .and_then(|thread| {
                    thread
                        .get("id")
                        .or_else(|| thread.get("threadId"))
                        .or_else(|| thread.get("thread_id"))
                }),
        );
    }

    fn collect_from_container(container: Option<&Value>, out: &mut Vec<String>) {
        let Some(container) = container.and_then(|value| value.as_object()) else {
            return;
        };
        push_thread_id(out, container.get("threadId").or_else(|| container.get("thread_id")));
        push_thread_id(
            out,
            container
                .get("thread")
                .and_then(|thread| thread.get("id")),
        );
        push_thread_id(
            out,
            container
                .get("params")
                .and_then(|params| params.get("threadId").or_else(|| params.get("thread_id"))),
        );
        push_thread_id(
            out,
            container
                .get("result")
                .and_then(|result| result.get("threadId").or_else(|| result.get("thread_id"))),
        );
        push_thread_id(
            out,
            container
                .get("newThreadId")
                .or_else(|| container.get("new_thread_id")),
        );
        push_thread_id(
            out,
            container
                .get("receiverThreadId")
                .or_else(|| container.get("receiver_thread_id")),
        );
        push_thread_id(
            out,
            container
                .get("receiverThreadIds")
                .or_else(|| container.get("receiver_thread_ids")),
        );
        collect_agent_thread_ids(
            container
                .get("receiverAgents")
                .or_else(|| container.get("receiver_agents")),
            out,
        );
        collect_agent_thread_ids(
            container
                .get("receiverAgent")
                .or_else(|| container.get("receiver_agent")),
            out,
        );
        collect_agent_thread_ids(
            container
                .get("agentStatuses")
                .or_else(|| container.get("agent_statuses")),
            out,
        );
        if let Some(status_map) = container.get("statuses").and_then(|value| value.as_object()) {
            out.extend(
                status_map
                    .keys()
                    .map(|key| key.trim().to_string())
                    .filter(|key| !key.is_empty()),
            );
        }
        if let Some(item) = container.get("item") {
            collect_from_container(Some(item), out);
        }
    }

    let mut out = Vec::new();
    collect_from_container(value.get("params"), &mut out);
    collect_from_container(value.get("result"), &mut out);
    collect_from_container(Some(value), &mut out);

    let mut seen = HashSet::new();
    out.into_iter()
        .filter(|thread_id| seen.insert(thread_id.clone()))
        .collect()
}

fn normalize_root_path(value: &str) -> String {
    let normalized = value.replace('\\', "/");
    let normalized = normalized.trim_end_matches('/');
    if normalized.is_empty() {
        return String::new();
    }
    let lower = normalized.to_ascii_lowercase();
    let normalized = if lower.starts_with("//?/unc/") {
        format!("//{}", &normalized[8..])
    } else if lower.starts_with("//?/") || lower.starts_with("//./") {
        normalized[4..].to_string()
    } else {
        normalized.to_string()
    };
    if normalized.is_empty() {
        return String::new();
    }

    let bytes = normalized.as_bytes();
    let is_drive_path = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes[2] == b'/';
    if is_drive_path || normalized.starts_with("//") {
        normalized.to_ascii_lowercase()
    } else {
        normalized.to_string()
    }
}

#[derive(Debug, Clone)]
struct ThreadListEntry {
    thread_id: String,
    cwd: Option<String>,
    is_memory_consolidation: bool,
}

fn extract_thread_entries_from_thread_list_result(value: &Value) -> Vec<ThreadListEntry> {
    fn collect_entries(input: &Value, out: &mut Vec<ThreadListEntry>) {
        if let Some(values) = input.as_array() {
            for value in values {
                collect_entries(value, out);
            }
            return;
        }
        let Some(object) = input.as_object() else {
            return;
        };

        let cwd = object
            .get("cwd")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
            .or_else(|| {
                object
                    .get("thread")
                    .and_then(|thread| thread.get("cwd"))
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string())
            });

        let thread_id = object
            .get("threadId")
            .or_else(|| object.get("thread_id"))
            .or_else(|| object.get("id"))
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
            .or_else(|| {
                object
                    .get("thread")
                    .and_then(|thread| thread.get("id"))
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string())
            });
        if let Some(thread_id) = thread_id {
            let source = object
                .get("source")
                .or_else(|| object.get("thread").and_then(|thread| thread.get("source")));
            let is_memory_consolidation = source
                .and_then(source_subagent_kind)
                .is_some_and(|kind| kind == "memory_consolidation");
            out.push(ThreadListEntry {
                thread_id,
                cwd,
                is_memory_consolidation,
            });
        }

        for key in ["threads", "items", "results", "data"] {
            if let Some(values) = object.get(key).and_then(|value| value.as_array()) {
                for value in values {
                    collect_entries(value, out);
                }
            }
        }
    }

    let mut out = Vec::new();
    if let Some(result) = value.get("result") {
        collect_entries(result, &mut out);
    }
    out
}

fn resolve_workspace_for_cwd(
    cwd: &str,
    workspace_roots: &HashMap<String, String>,
) -> Option<String> {
    let normalized_cwd = normalize_root_path(cwd);
    if normalized_cwd.is_empty() {
        return None;
    }
    workspace_roots
        .iter()
        .filter_map(|(workspace_id, root)| {
            if root.is_empty() {
                return None;
            }
            let is_exact_match = root == &normalized_cwd;
            let is_nested_match = normalized_cwd.len() > root.len()
                && normalized_cwd.starts_with(root)
                && normalized_cwd.as_bytes().get(root.len()) == Some(&b'/');
            if is_exact_match || is_nested_match {
                Some((workspace_id, root.len()))
            } else {
                None
            }
        })
        .max_by_key(|(_, root_len)| *root_len)
        .map(|(workspace_id, _)| workspace_id.clone())
}

fn normalize_subagent_kind(value: &str) -> String {
    let mut normalized = value.trim().to_ascii_lowercase().replace([' ', '-'], "_");
    if let Some(stripped) = normalized.strip_prefix("subagent_") {
        normalized = stripped.to_string();
    } else if let Some(stripped) = normalized.strip_prefix("sub_agent_") {
        normalized = stripped.to_string();
    }
    normalized
}

fn source_subagent_kind(source: &Value) -> Option<String> {
    if let Some(raw) = source.as_str() {
        let normalized = normalize_subagent_kind(raw);
        return if normalized.is_empty() {
            None
        } else {
            Some(normalized)
        };
    }
    let source_obj = source.as_object()?;
    let sub_agent = source_obj
        .get("subAgent")
        .or_else(|| source_obj.get("sub_agent"))
        .or_else(|| source_obj.get("subagent"))?;

    if let Some(raw) = sub_agent.as_str() {
        let normalized = normalize_subagent_kind(raw);
        return if normalized.is_empty() {
            None
        } else {
            Some(normalized)
        };
    }
    let sub_agent_obj = sub_agent.as_object()?;
    if let Some(explicit) = sub_agent_obj
        .get("kind")
        .or_else(|| sub_agent_obj.get("type"))
        .or_else(|| sub_agent_obj.get("name"))
        .or_else(|| sub_agent_obj.get("id"))
        .and_then(Value::as_str)
    {
        let normalized = normalize_subagent_kind(explicit);
        return if normalized.is_empty() {
            None
        } else {
            Some(normalized)
        };
    }

    let candidate_keys: Vec<&String> = sub_agent_obj
        .keys()
        .filter(|key| key.as_str() != "thread_spawn" && key.as_str() != "threadSpawn")
        .collect();
    if candidate_keys.len() != 1 {
        return None;
    }
    let normalized = normalize_subagent_kind(candidate_keys[0]);
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn thread_started_is_memory_consolidation(value: &Value) -> bool {
    value
        .get("params")
        .and_then(|params| {
            params
                .get("thread")
                .and_then(|thread| thread.get("source"))
                .or_else(|| params.get("source"))
        })
        .and_then(source_subagent_kind)
        .is_some_and(|kind| kind == "memory_consolidation")
}

fn should_suppress_hidden_thread_event(
    method_name: Option<&str>,
    has_result_or_error: bool,
) -> bool {
    !has_result_or_error
        && !matches!(
            method_name,
            Some("thread/archived") | Some("codex/backgroundThread")
        )
}

fn is_global_workspace_notification(method: &str) -> bool {
    matches!(
        method,
        "account/updated" | "account/rateLimits/updated" | "account/login/completed"
    )
}

fn should_broadcast_global_workspace_notification(
    method_name: Option<&str>,
    thread_id: Option<&String>,
    request_workspace: Option<&str>,
) -> bool {
    method_name.is_some_and(is_global_workspace_notification)
        && thread_id.is_none()
        && request_workspace.is_none()
}

#[derive(Clone)]
pub(crate) struct PendingLocalStart {
    request_id: u64,
    request_thread_id: Option<String>,
    expected_thread_id: Option<String>,
    request_kind: String,
}

#[derive(Clone)]
pub(crate) struct RequestContext {
    workspace_id: String,
    method: String,
    pending_local_start: Option<PendingLocalStart>,
}

fn build_initialize_params(client_version: &str) -> Value {
    json!({
        "clientInfo": {
            "name": "codex_monitor",
            "title": "Codex Quota Guard",
            "version": client_version
        },
        "capabilities": {
            "experimentalApi": true
        }
    })
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) struct WorkspaceSession {
    pub(crate) codex_args: Option<String>,
    pub(crate) child: Mutex<Child>,
    pub(crate) stdin: Mutex<ChildStdin>,
    pub(crate) pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    pub(crate) request_context: Mutex<HashMap<u64, RequestContext>>,
    pub(crate) thread_workspace: Mutex<HashMap<String, String>>,
    pub(crate) hidden_thread_ids: Mutex<HashSet<String>>,
    pub(crate) next_id: AtomicU64,
    /// Callbacks for background threads - events for these threadIds are sent through the channel
    pub(crate) background_thread_callbacks: Mutex<HashMap<String, mpsc::UnboundedSender<Value>>>,
    pub(crate) owner_workspace_id: String,
    pub(crate) workspace_ids: Mutex<HashSet<String>>,
    pub(crate) workspace_roots: Mutex<HashMap<String, String>>,
    pub(crate) bound_workspace_ids: Mutex<HashSet<String>>,
    pub(crate) session_epoch: String,
    pub(crate) canonical_codex_home: String,
    pub(crate) quota_guard: Option<QuotaGuardEventSink>,
    pub(crate) quota_gate: Option<ProcessGate>,
}

impl WorkspaceSession {
    pub(crate) fn session_epoch(&self) -> &str {
        &self.session_epoch
    }

    pub(crate) fn canonical_codex_home(&self) -> &str {
        &self.canonical_codex_home
    }

    fn observe_guard(&self, event: QuotaGuardEvent) {
        if let Some(quota_guard) = &self.quota_guard {
            let _ = quota_guard.observe(event);
        }
    }

    async fn record_start_failed(
        &self,
        start: &PendingLocalStart,
        workspace_id: &str,
        reason: String,
    ) -> Result<(), String> {
        let event = QuotaGuardEvent::StartFailed {
            request_id: start.request_id,
            session_epoch: self.session_epoch.clone(),
            workspace_id: workspace_id.to_string(),
            reason,
        };
        if let Some(quota_guard) = &self.quota_guard {
            quota_guard.record_start_failed(event).await
        } else {
            Ok(())
        }
    }

    async fn record_pending_start(
        &self,
        start: &PendingLocalStart,
        workspace_id: &str,
    ) -> Result<(), String> {
        let event = QuotaGuardEvent::PendingLocalStart {
            request_id: start.request_id,
            session_epoch: self.session_epoch.clone(),
            workspace_id: workspace_id.to_string(),
            request_thread_id: start.request_thread_id.clone(),
            expected_thread_id: start.expected_thread_id.clone(),
            request_kind: start.request_kind.clone(),
        };
        if let Some(quota_guard) = &self.quota_guard {
            quota_guard.record_pending_start(event).await
        } else {
            Ok(())
        }
    }

    fn close_workspace_admission(&self, workspace_id: &str) {
        if let Some(gate) = &self.quota_gate {
            gate.revoke_epoch(&self.session_epoch, workspace_id);
        }
    }

    fn close_for_identity_change(&self) {
        if let Some(gate) = &self.quota_gate {
            if gate.policy() != ProcessPolicy::DisabledOpen {
                gate.set_policy(ProcessPolicy::EnabledClosed);
            }
        }
    }

    pub(crate) async fn register_workspace_with_path(
        &self,
        workspace_id: &str,
        workspace_path: Option<&str>,
    ) {
        self.workspace_ids
            .lock()
            .await
            .insert(workspace_id.to_string());
        if let Some(path) = workspace_path {
            let normalized = normalize_root_path(path);
            if !normalized.is_empty() {
                self.workspace_roots
                    .lock()
                    .await
                    .insert(workspace_id.to_string(), normalized);
            }
        }
    }

    pub(crate) async fn bind_workspace(
        &self,
        workspace_id: &str,
        workspace_path: Option<&str>,
    ) {
        self.register_workspace_with_path(workspace_id, workspace_path)
            .await;
        let newly_bound = self
            .bound_workspace_ids
            .lock()
            .await
            .insert(workspace_id.to_string());
        if newly_bound {
            if let Some(gate) = &self.quota_gate {
                gate.register_closed_epoch(self.session_epoch.clone(), workspace_id.to_string());
            }
            self.observe_guard(QuotaGuardEvent::WorkspaceBound {
                session_epoch: self.session_epoch.clone(),
                workspace_id: workspace_id.to_string(),
                canonical_codex_home: self.canonical_codex_home.clone(),
            });
        }
    }

    pub(crate) async fn revoke_workspace(&self, workspace_id: &str) {
        if let Some(gate) = &self.quota_gate {
            gate.revoke_epoch(&self.session_epoch, workspace_id);
        }
        let was_bound = self.bound_workspace_ids.lock().await.remove(workspace_id);
        self.workspace_ids.lock().await.remove(workspace_id);
        self.workspace_roots.lock().await.remove(workspace_id);
        if was_bound {
            self.observe_guard(QuotaGuardEvent::WorkspaceDisconnected {
                session_epoch: self.session_epoch.clone(),
                workspace_id: workspace_id.to_string(),
            });
        }
    }

    pub(crate) async fn workspace_ids_snapshot(&self) -> Vec<String> {
        self.workspace_ids.lock().await.iter().cloned().collect()
    }

    pub(crate) async fn is_workspace_bound(&self, workspace_id: &str) -> bool {
        self.bound_workspace_ids.lock().await.contains(workspace_id)
    }

    async fn write_message(&self, value: Value) -> Result<(), String> {
        let mut stdin = self.stdin.lock().await;
        let mut line = serde_json::to_string(&value).map_err(|e| e.to_string())?;
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| e.to_string())
    }

    pub(crate) async fn send_request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.send_request_for_workspace(self.owner_workspace_id.as_str(), method, params)
            .await
    }

    async fn send_initialize_request(&self, params: Value) -> Result<Value, String> {
        self.send_request_inner(self.owner_workspace_id.as_str(), "initialize", params, true)
            .await
    }

    pub(crate) async fn send_request_for_workspace(
        &self,
        workspace_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        self.send_request_inner(workspace_id, method, params, false).await
    }

    async fn send_request_inner(
        &self,
        workspace_id: &str,
        method: &str,
        params: Value,
        allow_unbound: bool,
    ) -> Result<Value, String> {
        if !allow_unbound && !self.is_workspace_bound(workspace_id).await {
            return Err("workspace session is not bound to this workspace".to_string());
        }

        let is_inference = matches!(
            method,
            "turn/start" | "turn/steer" | "review/start" | "thread/compact/start"
        );
        let admission = if is_inference {
            self.quota_gate
                .as_ref()
                .map(|gate| {
                    gate.admit(Some(&self.session_epoch), workspace_id)
                        .map_err(quota_guard_blocked_error)
                })
                .transpose()?
        } else {
            None
        };

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        let request_thread_id = extract_thread_id(&json!({ "params": params.clone() }));
        let pending_local_start = match method {
            "turn/start" | "thread/compact/start" => Some(PendingLocalStart {
                request_id: id,
                expected_thread_id: request_thread_id.clone(),
                request_thread_id,
                request_kind: method.to_string(),
            }),
            "review/start" => Some(PendingLocalStart {
                request_id: id,
                request_thread_id,
                expected_thread_id: None,
                request_kind: method.to_string(),
            }),
            _ => None,
        };

        if let Some(start) = pending_local_start.as_ref() {
            // This acknowledgement is the durable ownership seam. Do not
            // write an inference-start request that recovery cannot account
            // for after a crash.
            if let Err(error) = self.record_pending_start(start, workspace_id).await {
                self.close_workspace_admission(workspace_id);
                return Err(format!(
                    "quota guard could not persist pending local start: {error}"
                ));
            }
        }

        self.pending.lock().await.insert(id, tx);
        self.request_context.lock().await.insert(
            id,
            RequestContext {
                workspace_id: workspace_id.to_string(),
                method: method.to_string(),
                pending_local_start: pending_local_start.clone(),
            },
        );
        if let Some(thread_id) = extract_thread_id(&json!({ "params": params.clone() })) {
            self.thread_workspace
                .lock()
                .await
                .insert(thread_id, workspace_id.to_string());
        }
        if let Err(error) = self
            .write_message(json!({ "id": id, "method": method, "params": params }))
            .await
        {
            self.pending.lock().await.remove(&id);
            self.request_context.lock().await.remove(&id);
            if let Some(start) = pending_local_start.as_ref() {
                if let Err(record_error) = self
                    .record_start_failed(start, workspace_id, error.clone())
                    .await
                {
                    self.close_workspace_admission(workspace_id);
                    return Err(format!(
                        "{error}; quota guard could not persist failed local start: {record_error}"
                    ));
                }
            }
            return Err(error);
        }

        // Closure finalization waits only for admissions that have not yet
        // registered and written. A response may take minutes and must not
        // keep the pre-close barrier open.
        drop(admission);
        match timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                self.request_context.lock().await.remove(&id);
                if let Some(start) = pending_local_start.as_ref() {
                    if let Err(record_error) = self
                        .record_start_failed(start, workspace_id, "request canceled".to_string())
                        .await
                    {
                        self.close_workspace_admission(workspace_id);
                        return Err(format!(
                            "request canceled; quota guard could not persist failed local start: {record_error}"
                        ));
                    }
                }
                Err("request canceled".to_string())
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                self.request_context.lock().await.remove(&id);
                let message = format!(
                    "request timed out after {} seconds",
                    REQUEST_TIMEOUT.as_secs()
                );
                if let Some(start) = pending_local_start.as_ref() {
                    if let Err(record_error) = self
                        .record_start_failed(start, workspace_id, message.clone())
                        .await
                    {
                        self.close_workspace_admission(workspace_id);
                        return Err(format!(
                            "{message}; quota guard could not persist failed local start: {record_error}"
                        ));
                    }
                }
                Err(message)
            }
        }
    }

    pub(crate) async fn send_notification(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), String> {
        let value = if let Some(params) = params {
            json!({ "method": method, "params": params })
        } else {
            json!({ "method": method })
        };
        self.write_message(value).await
    }

    pub(crate) async fn send_response(&self, id: Value, result: Value) -> Result<(), String> {
        self.write_message(json!({ "id": id, "result": result }))
            .await
    }
}

fn quota_guard_blocked_error(status: crate::shared::quota_guard::gate::AdmissionStatus) -> String {
    let state = match status.reason {
        AdmissionReason::Open => "open",
        AdmissionReason::GuardDisabled => "guardDisabled",
        AdmissionReason::ProcessClosed => "processClosed",
        AdmissionReason::EpochUnverified => "epochUnverified",
        AdmissionReason::WorkspaceUnbound => "workspaceUnbound",
    };
    format!("QUOTA_GUARD_BLOCKED|state={state}|verifyAt=")
}

pub(crate) fn build_codex_path_env(codex_bin: Option<&str>) -> Option<String> {
    let mut paths: Vec<PathBuf> = env::var_os("PATH")
        .map(|value| env::split_paths(&value).collect())
        .unwrap_or_default();

    let mut extras: Vec<PathBuf> = Vec::new();

    #[cfg(not(target_os = "windows"))]
    {
        extras.extend(
            [
                "/opt/homebrew/bin",
                "/usr/local/bin",
                "/usr/bin",
                "/bin",
                "/usr/sbin",
                "/sbin",
            ]
            .into_iter()
            .map(PathBuf::from),
        );

        if let Ok(home) = env::var("HOME") {
            let home_path = Path::new(&home);
            extras.push(home_path.join(".local/bin"));
            extras.push(home_path.join(".local/share/mise/shims"));
            extras.push(home_path.join(".cargo/bin"));
            extras.push(home_path.join(".bun/bin"));
            let nvm_root = home_path.join(".nvm/versions/node");
            if let Ok(entries) = std::fs::read_dir(nvm_root) {
                for entry in entries.flatten() {
                    let bin_path = entry.path().join("bin");
                    if bin_path.is_dir() {
                        extras.push(bin_path);
                    }
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = env::var("APPDATA") {
            extras.push(Path::new(&appdata).join("npm"));
        }
        if let Ok(local_app_data) = env::var("LOCALAPPDATA") {
            extras.push(
                Path::new(&local_app_data)
                    .join("Microsoft")
                    .join("WindowsApps"),
            );
        }
        if let Ok(home) = env::var("USERPROFILE").or_else(|_| env::var("HOME")) {
            let home_path = Path::new(&home);
            extras.push(home_path.join(".cargo").join("bin"));
            extras.push(home_path.join("scoop").join("shims"));
        }
        if let Ok(program_data) = env::var("PROGRAMDATA") {
            extras.push(Path::new(&program_data).join("chocolatey").join("bin"));
        }
    }

    if let Some(bin_path) = codex_bin.filter(|value| !value.trim().is_empty()) {
        if let Some(parent) = Path::new(bin_path).parent() {
            extras.push(parent.to_path_buf());
        }
    }

    for extra in extras {
        if !paths.iter().any(|path| path == &extra) {
            paths.push(extra);
        }
    }

    if paths.is_empty() {
        return None;
    }

    env::join_paths(paths)
        .ok()
        .map(|joined| joined.to_string_lossy().to_string())
}

pub(crate) fn build_codex_command_with_bin(
    codex_bin: Option<String>,
    codex_args: Option<&str>,
    args: Vec<String>,
) -> Result<Command, String> {
    let bin = codex_bin
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "codex".into());

    let path_env = build_codex_path_env(codex_bin.as_deref());
    let mut command_args = parse_codex_args(codex_args)?;
    command_args.extend(args);

    #[cfg(target_os = "windows")]
    let mut command = {
        let bin_trimmed = bin.trim();
        let resolved = resolve_windows_executable(bin_trimmed, path_env.as_deref());
        let resolved_path = resolved
            .as_deref()
            .unwrap_or_else(|| Path::new(bin_trimmed));
        let ext = resolved_path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase());

        if matches!(ext.as_deref(), Some("cmd") | Some("bat")) {
            let mut command = tokio_command("cmd");
            let command_line = build_cmd_c_command(resolved_path, &command_args)?;
            command.arg("/D");
            command.arg("/S");
            command.arg("/C");
            command.raw_arg(command_line);
            command
        } else {
            let mut command = tokio_command(resolved_path);
            command.args(command_args);
            command
        }
    };

    #[cfg(not(target_os = "windows"))]
    let mut command = {
        let mut command = tokio_command(bin.trim());
        command.args(command_args);
        command
    };

    if let Some(path_env) = path_env {
        command.env("PATH", path_env);
    }
    Ok(command)
}

pub(crate) async fn check_codex_installation(
    codex_bin: Option<String>,
) -> Result<Option<String>, String> {
    let mut command = build_codex_command_with_bin(codex_bin, None, vec!["--version".to_string()])?;
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let output = match timeout(Duration::from_secs(5), command.output()).await {
        Ok(result) => result.map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                "Codex CLI not found. Install Codex and ensure `codex` is on your PATH.".to_string()
            } else {
                e.to_string()
            }
        })?,
        Err(_) => {
            return Err(
                "Timed out while checking Codex CLI. Make sure `codex --version` runs in Terminal."
                    .to_string(),
            );
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        if detail.is_empty() {
            return Err(
                "Codex CLI failed to start. Try running `codex --version` in Terminal.".to_string(),
            );
        }
        return Err(format!(
            "Codex CLI failed to start: {detail}. Try running `codex --version` in Terminal."
        ));
    }

    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(if version.is_empty() {
        None
    } else {
        Some(version)
    })
}

pub(crate) async fn spawn_workspace_session<E: EventSink>(
    entry: WorkspaceEntry,
    default_codex_bin: Option<String>,
    codex_args: Option<String>,
    codex_home: Option<PathBuf>,
    client_version: String,
    event_sink: E,
    quota_guard: Option<QuotaGuardEventSink>,
) -> Result<Arc<WorkspaceSession>, String> {
    let codex_bin = default_codex_bin;
    let _ = check_codex_installation(codex_bin.clone()).await?;
    let canonical_codex_home = codex_home
        .clone()
        .or_else(resolve_default_codex_home)
        .and_then(|path| std::fs::canonicalize(&path).ok().or(Some(path)))
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_default();
    let session_epoch = NEXT_SESSION_EPOCH.fetch_add(1, Ordering::SeqCst).to_string();
    let quota_gate = quota_guard.as_ref().map(QuotaGuardEventSink::gate);
    if let Some(gate) = &quota_gate {
        gate.register_closed_epoch(session_epoch.clone(), entry.id.clone());
    }

    let mut command = build_codex_command_with_bin(
        codex_bin,
        codex_args.as_deref(),
        vec!["app-server".to_string()],
    )?;
    command.current_dir(&entry.path);
    if let Some(path) = codex_home.as_ref() {
        command.env("CODEX_HOME", path);
    }
    command.stdin(std::process::Stdio::piped());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let stdin = child.stdin.take().ok_or("missing stdin")?;
    let stdout = child.stdout.take().ok_or("missing stdout")?;
    let stderr = child.stderr.take().ok_or("missing stderr")?;

    let session = Arc::new(WorkspaceSession {
        codex_args,
        child: Mutex::new(child),
        stdin: Mutex::new(stdin),
        pending: Mutex::new(HashMap::new()),
        request_context: Mutex::new(HashMap::new()),
        thread_workspace: Mutex::new(HashMap::new()),
        hidden_thread_ids: Mutex::new(HashSet::new()),
        next_id: AtomicU64::new(1),
        background_thread_callbacks: Mutex::new(HashMap::new()),
        owner_workspace_id: entry.id.clone(),
        workspace_ids: Mutex::new(HashSet::from([entry.id.clone()])),
        workspace_roots: Mutex::new(HashMap::from([(
            entry.id.clone(),
            normalize_root_path(&entry.path),
        )])),
        bound_workspace_ids: Mutex::new(HashSet::new()),
        session_epoch,
        canonical_codex_home,
        quota_guard,
        quota_gate,
    });

    let session_clone = Arc::clone(&session);
    let fallback_workspace_id = entry.id.clone();
    let event_sink_clone = event_sink.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(err) => {
                    let payload = AppServerEvent {
                        workspace_id: fallback_workspace_id.clone(),
                        message: json!({
                            "method": "codex/parseError",
                            "params": { "error": err.to_string(), "raw": line },
                        }),
                    };
                    event_sink_clone.emit_app_server_event(payload);
                    continue;
                }
            };

            let maybe_id = value.get("id").and_then(|id| id.as_u64());
            let has_method = value.get("method").is_some();
            let has_result_or_error = value.get("result").is_some() || value.get("error").is_some();
            let method_name = value.get("method").and_then(|method| method.as_str());

            // Keep the complete local-start context until this response has been
            // classified; frontend routing must not be the ownership source.
            let thread_id = extract_thread_id(&value);
            let mut request_workspace: Option<String> = None;
            let mut request_method: Option<String> = None;
            let mut pending_local_start: Option<PendingLocalStart> = None;
            let mut deferred_start_failure: Option<(PendingLocalStart, String, String)> = None;
            if let Some(id) = maybe_id {
                if has_result_or_error {
                    if let Some(context) = session_clone.request_context.lock().await.remove(&id) {
                        request_workspace = Some(context.workspace_id);
                        request_method = Some(context.method);
                        pending_local_start = context.pending_local_start;
                    }
                }
            }
            if let (Some(start), Some(workspace_id)) =
                (pending_local_start.as_ref(), request_workspace.as_ref())
            {
                if value.get("error").is_some() {
                    deferred_start_failure = Some((
                        start.clone(),
                        workspace_id.clone(),
                        value
                            .get("error")
                            .and_then(|error| error.get("message"))
                            .and_then(Value::as_str)
                            .unwrap_or("app-server request failed")
                            .to_string(),
                    ));
                } else {
                    session_clone.observe_guard(QuotaGuardEvent::StartResponse {
                        request_id: start.request_id,
                        session_epoch: session_clone.session_epoch.clone(),
                        workspace_id: workspace_id.clone(),
                        method: start.request_kind.clone(),
                        value: value.clone(),
                    });
                }
            }

            if let Some(ref workspace_id) = request_workspace {
                let related_thread_ids = extract_related_thread_ids(&value);
                if !related_thread_ids.is_empty() {
                    let mut thread_workspace = session_clone.thread_workspace.lock().await;
                    for tid in related_thread_ids {
                        thread_workspace.insert(tid, workspace_id.clone());
                    }
                } else if let Some(ref tid) = thread_id {
                    session_clone
                        .thread_workspace
                        .lock()
                        .await
                        .insert(tid.clone(), workspace_id.clone());
                }
            }
            if matches!(request_method.as_deref(), Some("thread/list")) {
                let thread_entries = extract_thread_entries_from_thread_list_result(&value);
                if !thread_entries.is_empty() {
                    let workspace_roots = session_clone.workspace_roots.lock().await.clone();
                    let mut hidden_thread_ids = Vec::new();
                    let mut thread_workspace = session_clone.thread_workspace.lock().await;
                    for entry in thread_entries {
                        if entry.is_memory_consolidation {
                            thread_workspace.remove(&entry.thread_id);
                            hidden_thread_ids.push(entry.thread_id);
                            continue;
                        }
                        let mapped_workspace = entry
                            .cwd
                            .as_deref()
                            .and_then(|cwd| resolve_workspace_for_cwd(cwd, &workspace_roots));
                        if let Some(workspace_id) = mapped_workspace {
                            thread_workspace.insert(entry.thread_id, workspace_id);
                        }
                    }
                    drop(thread_workspace);
                    if !hidden_thread_ids.is_empty() {
                        let mut hidden = session_clone.hidden_thread_ids.lock().await;
                        for thread_id in hidden_thread_ids {
                            hidden.insert(thread_id);
                        }
                    }
                }
            }

            let mapped_thread_workspace = if let Some(ref tid) = thread_id {
                session_clone
                    .thread_workspace
                    .lock()
                    .await
                    .get(tid)
                    .cloned()
            } else {
                None
            };

            let routed_workspace_id = mapped_thread_workspace
                .or_else(|| request_workspace.clone())
                .unwrap_or_else(|| fallback_workspace_id.clone());

            if let Some(start) = pending_local_start {
                if value.get("error").is_none() {
                    let response_thread_id = if start.request_kind == "review/start" {
                        extract_review_thread_id(&value)
                    } else {
                        extract_thread_id(&value).or(start.expected_thread_id)
                    };
                    if let Some(response_thread_id) = response_thread_id {
                        session_clone
                            .thread_workspace
                            .lock()
                            .await
                            .insert(response_thread_id, routed_workspace_id.clone());
                    }
                }
            }

            // The quota observer must see raw app-server messages before hidden
            // thread suppression or frontend fan-out can discard them.
            match method_name {
                Some("account/rateLimits/updated") => {
                    session_clone.observe_guard(QuotaGuardEvent::RateLimits {
                        session_epoch: session_clone.session_epoch.clone(),
                        workspace_id: routed_workspace_id.clone(),
                        value: value.clone(),
                    });
                }
                Some("turn/started") => {
                    if let (Some(thread_id), Some(turn_id)) =
                        (thread_id.as_deref(), extract_turn_id(&value))
                    {
                        session_clone.observe_guard(QuotaGuardEvent::TurnStarted {
                            session_epoch: session_clone.session_epoch.clone(),
                            workspace_id: routed_workspace_id.clone(),
                            thread_id: thread_id.to_string(),
                            turn_id,
                        });
                    }
                }
                Some("turn/completed") => {
                    if let (Some(thread_id), Some(turn_id)) =
                        (thread_id.as_deref(), extract_turn_id(&value))
                    {
                        let params = value.get("params");
                        let status = params
                            .and_then(|params| params.get("status").or_else(|| params.get("turn").and_then(|turn| turn.get("status"))))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let error = params.and_then(|params| params.get("error").or_else(|| params.get("turn").and_then(|turn| turn.get("error")))).cloned();
                        session_clone.observe_guard(QuotaGuardEvent::TurnCompleted {
                            session_epoch: session_clone.session_epoch.clone(),
                            workspace_id: routed_workspace_id.clone(),
                            thread_id: thread_id.to_string(),
                            turn_id,
                            status,
                            error,
                        });
                    }
                }
                Some("account/updated") | Some("account/login/completed") => {
                    // An identity notification is security-significant. Close
                    // before the non-blocking handoff so no request races the
                    // strict identity revalidation the actor will start.
                    session_clone.close_for_identity_change();
                    session_clone.observe_guard(QuotaGuardEvent::AccountIdentityChanged {
                        session_epoch: session_clone.session_epoch.clone(),
                        workspace_id: routed_workspace_id.clone(),
                        reason: method_name.unwrap_or_default().to_string(),
                    });
                }
                _ => {}
            }

            if let Some(ref tid) = thread_id {
                if method_name == Some("codex/backgroundThread") {
                    let action = value
                        .get("params")
                        .and_then(|params| params.get("action"))
                        .and_then(Value::as_str)
                        .unwrap_or("hide");
                    if action.eq_ignore_ascii_case("hide") {
                        session_clone.hidden_thread_ids.lock().await.insert(tid.clone());
                    }
                } else if method_name == Some("thread/started")
                    && thread_started_is_memory_consolidation(&value)
                {
                    session_clone.hidden_thread_ids.lock().await.insert(tid.clone());
                    let payload = AppServerEvent {
                        workspace_id: routed_workspace_id.clone(),
                        message: json!({
                            "method": "codex/backgroundThread",
                            "params": {
                                "threadId": tid,
                                "action": "hide"
                            }
                        }),
                    };
                    event_sink_clone.emit_app_server_event(payload);
                    continue;
                }

                let should_suppress_hidden_thread = {
                    let hidden = session_clone.hidden_thread_ids.lock().await;
                    hidden.contains(tid)
                };
                if should_suppress_hidden_thread
                    && should_suppress_hidden_thread_event(method_name, has_result_or_error)
                {
                    continue;
                }
            }

            if matches!(method_name, Some("item/started") | Some("item/completed")) {
                let related_thread_ids = extract_related_thread_ids(&value);
                if !related_thread_ids.is_empty() {
                    let mut thread_workspace = session_clone.thread_workspace.lock().await;
                    for related_id in related_thread_ids {
                        thread_workspace
                            .entry(related_id)
                            .or_insert_with(|| routed_workspace_id.clone());
                    }
                }
            }

            if method_name == Some("thread/archived") {
                if let Some(ref tid) = thread_id {
                    session_clone.thread_workspace.lock().await.remove(tid);
                    session_clone.hidden_thread_ids.lock().await.remove(tid);
                }
            }

            if let Some(id) = maybe_id {
                if has_result_or_error {
                    let sender = session_clone.pending.lock().await.remove(&id);
                    if let Some((start, workspace_id, reason)) = deferred_start_failure {
                        let session = Arc::clone(&session_clone);
                        let response = value.clone();
                        tokio::spawn(async move {
                            if session
                                .record_start_failed(&start, &workspace_id, reason)
                                .await
                                .is_err()
                            {
                                session.close_workspace_admission(&workspace_id);
                            }
                            if let Some(tx) = sender {
                                let _ = tx.send(response);
                            }
                        });
                    } else if let Some(tx) = sender {
                        let _ = tx.send(value);
                    }
                } else if has_method {
                    // Check for background thread callback
                    let mut sent_to_background = false;
                    if let Some(ref tid) = thread_id {
                        let callbacks = session_clone.background_thread_callbacks.lock().await;
                        if let Some(tx) = callbacks.get(tid) {
                            let _ = tx.send(value.clone());
                            sent_to_background = true;
                        }
                    }
                    // Don't emit to frontend if this is a background thread event
                    if !sent_to_background {
                        if should_broadcast_global_workspace_notification(
                            method_name,
                            thread_id.as_ref(),
                            request_workspace.as_deref(),
                        ) {
                            let workspace_ids = session_clone.workspace_ids_snapshot().await;
                            if workspace_ids.is_empty() {
                                let payload = AppServerEvent {
                                    workspace_id: routed_workspace_id.clone(),
                                    message: value,
                                };
                                event_sink_clone.emit_app_server_event(payload);
                            } else {
                                for workspace_id in workspace_ids {
                                    let payload = AppServerEvent {
                                        workspace_id,
                                        message: value.clone(),
                                    };
                                    event_sink_clone.emit_app_server_event(payload);
                                }
                            }
                        } else {
                            let payload = AppServerEvent {
                                workspace_id: routed_workspace_id.clone(),
                                message: value,
                            };
                            event_sink_clone.emit_app_server_event(payload);
                        }
                    }
                } else if let Some(tx) = session_clone.pending.lock().await.remove(&id) {
                    let _ = tx.send(value);
                }
            } else if has_method {
                // Check for background thread callback
                let mut sent_to_background = false;
                if let Some(ref tid) = thread_id {
                    let callbacks = session_clone.background_thread_callbacks.lock().await;
                    if let Some(tx) = callbacks.get(tid) {
                        let _ = tx.send(value.clone());
                        sent_to_background = true;
                    }
                }
                // Don't emit to frontend if this is a background thread event
                if !sent_to_background {
                    if should_broadcast_global_workspace_notification(
                        method_name,
                        thread_id.as_ref(),
                        request_workspace.as_deref(),
                    ) {
                        let workspace_ids = session_clone.workspace_ids_snapshot().await;
                        if workspace_ids.is_empty() {
                            let payload = AppServerEvent {
                                workspace_id: routed_workspace_id,
                                message: value,
                            };
                            event_sink_clone.emit_app_server_event(payload);
                        } else {
                            for workspace_id in workspace_ids {
                                let payload = AppServerEvent {
                                    workspace_id,
                                    message: value.clone(),
                                };
                                event_sink_clone.emit_app_server_event(payload);
                            }
                        }
                    } else {
                        let payload = AppServerEvent {
                            workspace_id: routed_workspace_id,
                            message: value,
                        };
                        event_sink_clone.emit_app_server_event(payload);
                    }
                }
            }
        }

        // A dead app-server can no longer honour an epoch permit. Revoke every
        // committed workspace before exposing the disconnect to the actor.
        for workspace_id in session_clone.workspace_ids_snapshot().await {
            session_clone.revoke_workspace(&workspace_id).await;
        }
        session_clone.pending.lock().await.clear();
        session_clone.request_context.lock().await.clear();
    });

    let workspace_id = entry.id.clone();
    let event_sink_clone = event_sink.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let payload = AppServerEvent {
                workspace_id: workspace_id.clone(),
                message: json!({
                    "method": "codex/stderr",
                    "params": { "message": line },
                }),
            };
            event_sink_clone.emit_app_server_event(payload);
        }
    });

    let init_params = build_initialize_params(&client_version);
    let init_result = timeout(
        Duration::from_secs(15),
        session.send_initialize_request(init_params),
    )
    .await;
    let init_response = match init_result {
        Ok(response) => response,
        Err(_) => {
            let mut child = session.child.lock().await;
            kill_child_process_tree(&mut child).await;
            return Err(
                "Codex app-server did not respond to initialize. Check that `codex app-server` works in Terminal."
                    .to_string(),
            );
        }
    };
    init_response?;
    session.send_notification("initialized", None).await?;

    let payload = AppServerEvent {
        workspace_id: entry.id.clone(),
        message: json!({
            "method": "codex/connected",
            "params": { "workspaceId": entry.id.clone() }
        }),
    };
    event_sink.emit_app_server_event(payload);

    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::{
        build_initialize_params, extract_related_thread_ids, extract_thread_entries_from_thread_list_result,
        extract_thread_id, extract_turn_id, normalize_root_path, resolve_workspace_for_cwd,
        should_suppress_hidden_thread_event, source_subagent_kind,
        thread_started_is_memory_consolidation,
    };
    use std::collections::HashMap;
    use serde_json::json;

    #[test]
    fn extract_thread_id_reads_camel_case() {
        let value = json!({ "params": { "threadId": "thread-123" } });
        assert_eq!(extract_thread_id(&value), Some("thread-123".to_string()));
    }

    #[test]
    fn extract_thread_id_reads_snake_case() {
        let value = json!({ "params": { "thread_id": "thread-456" } });
        assert_eq!(extract_thread_id(&value), Some("thread-456".to_string()));
    }

    #[test]
    fn extract_thread_id_reads_hook_notification_thread_id() {
        let value = json!({
            "method": "hook/started",
            "params": {
                "threadId": "thread-hook-1",
                "run": { "id": "hook-1" }
            }
        });
        assert_eq!(extract_thread_id(&value), Some("thread-hook-1".to_string()));
    }

    #[test]
    fn extract_thread_id_returns_none_when_missing() {
        let value = json!({ "params": {} });
        assert_eq!(extract_thread_id(&value), None);
    }

    #[test]
    fn extract_turn_id_accepts_nested_turn() {
        let value = json!({ "params": { "turn": { "id": "turn-123" } } });
        assert_eq!(extract_turn_id(&value), Some("turn-123".to_string()));
    }

    #[test]
    fn build_initialize_params_enables_experimental_api() {
        let params = build_initialize_params("1.2.3");
        assert_eq!(
            params
                .get("capabilities")
                .and_then(|caps| caps.get("experimentalApi"))
                .and_then(|value| value.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn extract_thread_entries_reads_result_data_items() {
        let value = json!({
            "result": {
                "data": [
                    { "id": "thread-a", "cwd": "/tmp/a" },
                    {
                        "threadId": "thread-b",
                        "cwd": "/tmp/b",
                        "source": { "subAgent": "memory_consolidation" }
                    }
                ]
            }
        });
        let entries = extract_thread_entries_from_thread_list_result(&value);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].thread_id, "thread-a");
        assert_eq!(entries[0].cwd.as_deref(), Some("/tmp/a"));
        assert!(!entries[0].is_memory_consolidation);
        assert_eq!(entries[1].thread_id, "thread-b");
        assert_eq!(entries[1].cwd.as_deref(), Some("/tmp/b"));
        assert!(entries[1].is_memory_consolidation);
    }

    #[test]
    fn extract_related_thread_ids_reads_spawn_hints_from_item_payloads() {
        let value = json!({
            "method": "item/completed",
            "params": {
                "threadId": "thread-parent",
                "item": {
                    "type": "mcpToolCall",
                    "new_thread_id": "thread-child"
                }
            }
        });
        let ids = extract_related_thread_ids(&value);
        assert!(ids.contains(&"thread-parent".to_string()));
        assert!(ids.contains(&"thread-child".to_string()));
    }

    #[test]
    fn extract_related_thread_ids_reads_receiver_agent_references() {
        let value = json!({
            "method": "item/completed",
            "params": {
                "threadId": "thread-parent",
                "item": {
                    "type": "collabToolCall",
                    "receiver_agents": [
                        { "thread_id": "thread-child-a" },
                        { "thread": { "id": "thread-child-b" } }
                    ],
                    "statuses": {
                        "thread-child-c": { "status": "running" }
                    }
                }
            }
        });
        let ids = extract_related_thread_ids(&value);
        assert!(ids.contains(&"thread-parent".to_string()));
        assert!(ids.contains(&"thread-child-a".to_string()));
        assert!(ids.contains(&"thread-child-b".to_string()));
        assert!(ids.contains(&"thread-child-c".to_string()));
    }

    #[test]
    fn extract_related_thread_ids_reads_singular_receiver_agent_reference() {
        let value = json!({
            "method": "item/completed",
            "params": {
                "threadId": "thread-parent",
                "item": {
                    "type": "mcpToolCall",
                    "receiver_agent": { "thread_id": "thread-child-single" }
                }
            }
        });
        let ids = extract_related_thread_ids(&value);
        assert!(ids.contains(&"thread-parent".to_string()));
        assert!(ids.contains(&"thread-child-single".to_string()));
    }

    #[test]
    fn resolve_workspace_for_cwd_normalizes_windows_paths() {
        let mut roots = HashMap::new();
        roots.insert("ws-1".to_string(), normalize_root_path("C:\\Dev\\Codex"));
        assert_eq!(
            resolve_workspace_for_cwd("c:/dev/codex", &roots),
            Some("ws-1".to_string())
        );
    }

    #[test]
    fn resolve_workspace_for_cwd_normalizes_windows_namespace_paths() {
        let mut roots = HashMap::new();
        roots.insert("ws-1".to_string(), normalize_root_path("C:\\Dev\\Codex"));
        assert_eq!(
            resolve_workspace_for_cwd("\\\\?\\C:\\Dev\\Codex", &roots),
            Some("ws-1".to_string())
        );
    }

    #[test]
    fn normalize_root_path_normalizes_windows_namespace_unc_paths() {
        assert_eq!(
            normalize_root_path("\\\\?\\UNC\\SERVER\\Share\\Repo\\"),
            "//server/share/repo"
        );
    }

    #[test]
    fn resolve_workspace_for_cwd_matches_nested_paths() {
        let mut roots = HashMap::new();
        roots.insert("ws-1".to_string(), normalize_root_path("/tmp/codex"));
        assert_eq!(
            resolve_workspace_for_cwd("/tmp/codex/subdir/project", &roots),
            Some("ws-1".to_string())
        );
    }

    #[test]
    fn resolve_workspace_for_cwd_prefers_longest_matching_root() {
        let mut roots = HashMap::new();
        roots.insert("ws-parent".to_string(), normalize_root_path("/tmp/codex"));
        roots.insert(
            "ws-child".to_string(),
            normalize_root_path("/tmp/codex/subdir"),
        );
        assert_eq!(
            resolve_workspace_for_cwd("/tmp/codex/subdir/project", &roots),
            Some("ws-child".to_string())
        );
    }

    #[test]
    fn source_subagent_kind_reads_string_variants() {
        assert_eq!(
            source_subagent_kind(&json!("subagent-memory-consolidation")),
            Some("memory_consolidation".to_string())
        );
        assert_eq!(
            source_subagent_kind(&json!("sub_agent_memory_consolidation")),
            Some("memory_consolidation".to_string())
        );
    }

    #[test]
    fn source_subagent_kind_reads_nested_subagent_object_keys() {
        let source = json!({
            "subAgent": {
                "memory_consolidation": {
                    "thread_spawn": { "parent_thread_id": "thread-parent" }
                }
            }
        });
        assert_eq!(
            source_subagent_kind(&source),
            Some("memory_consolidation".to_string())
        );
    }

    #[test]
    fn thread_started_memory_consolidation_detects_thread_source() {
        let value = json!({
            "method": "thread/started",
            "params": {
                "thread": {
                    "id": "thread-1",
                    "source": {
                        "subagent": "memory_consolidation"
                    }
                }
            }
        });
        assert!(thread_started_is_memory_consolidation(&value));
    }

    #[test]
    fn thread_started_memory_consolidation_detects_params_source_fallback() {
        let value = json!({
            "method": "thread/started",
            "params": {
                "threadId": "thread-1",
                "source": {
                    "subAgent": "memory_consolidation"
                }
            }
        });
        assert!(thread_started_is_memory_consolidation(&value));
    }

    #[test]
    fn thread_started_memory_consolidation_rejects_non_memory_subagent() {
        let value = json!({
            "method": "thread/started",
            "params": {
                "thread": {
                    "id": "thread-1",
                    "source": {
                        "subAgent": "review"
                    }
                }
            }
        });
        assert!(!thread_started_is_memory_consolidation(&value));
    }

    #[test]
    fn hidden_thread_suppression_allows_rpc_responses() {
        assert!(!should_suppress_hidden_thread_event(Some("thread/archived"), true));
        assert!(!should_suppress_hidden_thread_event(Some("thread/updated"), true));
        assert!(!should_suppress_hidden_thread_event(None, true));
    }

    #[test]
    fn hidden_thread_suppression_still_blocks_non_exempt_notifications() {
        assert!(should_suppress_hidden_thread_event(
            Some("thread/updated"),
            false
        ));
        assert!(!should_suppress_hidden_thread_event(
            Some("thread/archived"),
            false
        ));
        assert!(!should_suppress_hidden_thread_event(
            Some("codex/backgroundThread"),
            false
        ));
    }
}
