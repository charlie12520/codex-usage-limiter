use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessPolicy {
    DisabledOpen,
    EnabledClosed,
    EnabledOpen,
}

impl ProcessPolicy {
    fn encode(self) -> u8 { match self { Self::DisabledOpen => 0, Self::EnabledClosed => 1, Self::EnabledOpen => 2 } }
    fn decode(value: u8) -> Self { match value { 0 => Self::DisabledOpen, 2 => Self::EnabledOpen, _ => Self::EnabledClosed } }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AdmissionReason {
    Open,
    GuardDisabled,
    ProcessClosed,
    EpochUnverified,
    WorkspaceUnbound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdmissionStatus {
    pub(crate) open: bool,
    pub(crate) reason: AdmissionReason,
}

#[derive(Default)]
struct GateState {
    permits: HashMap<(String, String), bool>,
    admissions: usize,
}

#[derive(Clone)]
pub(crate) struct ProcessGate {
    policy: Arc<AtomicU8>,
    state: Arc<Mutex<GateState>>,
}

impl Default for ProcessGate {
    fn default() -> Self {
        Self { policy: Arc::new(AtomicU8::new(ProcessPolicy::DisabledOpen.encode())), state: Arc::new(Mutex::new(GateState::default())) }
    }
}

impl ProcessGate {
    pub(crate) fn policy(&self) -> ProcessPolicy { ProcessPolicy::decode(self.policy.load(Ordering::SeqCst)) }

    /// Process-policy writes and admission creation are serialized by the same
    /// mutex.  This makes a close a real admission barrier: a token returned
    /// before this call is a pre-close request, and none can be returned after.
    pub(crate) fn set_policy(&self, policy: ProcessPolicy) {
        let _state = self.state.lock().expect("gate lock poisoned");
        self.policy.store(policy.encode(), Ordering::SeqCst);
    }

    pub(crate) fn close(&self) { self.set_policy(ProcessPolicy::EnabledClosed); }

    pub(crate) fn register_closed_epoch(&self, epoch: String, workspace_id: String) {
        self.state.lock().expect("gate lock poisoned").permits.insert((epoch, workspace_id), false);
    }

    pub(crate) fn set_epoch_open(&self, epoch: &str, workspace_id: &str, open: bool) {
        if let Some(permit) = self.state.lock().expect("gate lock poisoned").permits.get_mut(&(epoch.to_string(), workspace_id.to_string())) { *permit = open; }
    }

    pub(crate) fn revoke_epoch(&self, epoch: &str, workspace_id: &str) {
        self.state.lock().expect("gate lock poisoned").permits.remove(&(epoch.to_string(), workspace_id.to_string()));
    }

    pub(crate) fn status(&self, epoch: Option<&str>, workspace_id: &str) -> AdmissionStatus {
        let state = self.state.lock().expect("gate lock poisoned");
        self.status_locked(&state, epoch, workspace_id)
    }

    pub(crate) fn admit(&self, epoch: Option<&str>, workspace_id: &str) -> Result<AdmissionToken, AdmissionStatus> {
        let mut state = self.state.lock().expect("gate lock poisoned");
        let status = self.status_locked(&state, epoch, workspace_id);
        if !status.open { return Err(status); }
        state.admissions = state.admissions.saturating_add(1);
        Ok(AdmissionToken { state: Arc::clone(&self.state), released: false })
    }

    fn status_locked(&self, state: &GateState, epoch: Option<&str>, workspace_id: &str) -> AdmissionStatus {
        match self.policy() {
            ProcessPolicy::DisabledOpen => AdmissionStatus { open: true, reason: AdmissionReason::GuardDisabled },
            ProcessPolicy::EnabledClosed => AdmissionStatus { open: false, reason: AdmissionReason::ProcessClosed },
            ProcessPolicy::EnabledOpen => match epoch.and_then(|value| state.permits.get(&(value.to_string(), workspace_id.to_string()))) {
                Some(true) => AdmissionStatus { open: true, reason: AdmissionReason::Open },
                Some(false) => AdmissionStatus { open: false, reason: AdmissionReason::EpochUnverified },
                None => AdmissionStatus { open: false, reason: AdmissionReason::WorkspaceUnbound },
            },
        }
    }

    pub(crate) fn active_admissions(&self) -> usize { self.state.lock().expect("gate lock poisoned").admissions }

    /// Wait outside the actor after `close`.  No token can be added after the
    /// close, so zero is a stable finalized admission set.
    pub(crate) async fn wait_for_admissions(&self) {
        while self.active_admissions() != 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
}

pub(crate) struct AdmissionToken { state: Arc<Mutex<GateState>>, released: bool }
impl AdmissionToken { pub(crate) fn release(mut self) { self.release_inner(); } fn release_inner(&mut self) { if !self.released { let mut state = self.state.lock().expect("gate lock poisoned"); state.admissions = state.admissions.saturating_sub(1); self.released = true; } } }
impl Drop for AdmissionToken { fn drop(&mut self) { self.release_inner(); } }

#[cfg(test)]
mod tests {
    use super::{AdmissionReason, ProcessGate, ProcessPolicy};
    #[test]
    fn enabled_open_requires_a_verified_epoch() {
        let gate = ProcessGate::default(); gate.set_policy(ProcessPolicy::EnabledOpen);
        assert_eq!(gate.status(None, "w").reason, AdmissionReason::WorkspaceUnbound);
        gate.register_closed_epoch("e".into(), "w".into());
        assert_eq!(gate.status(Some("e"), "w").reason, AdmissionReason::EpochUnverified);
        gate.set_epoch_open("e", "w", true);
        let token = gate.admit(Some("e"), "w").unwrap(); assert_eq!(gate.active_admissions(), 1); drop(token); assert_eq!(gate.active_admissions(), 0);
    }
}
