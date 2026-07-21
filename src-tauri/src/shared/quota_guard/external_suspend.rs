use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::model::SuspendedExternalEngine;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessInfo {
    pid: u32,
    parent_pid: u32,
    start_time: u64,
    image_path: String,
}

#[derive(Debug, Default)]
pub(crate) struct SweepResult {
    pub(crate) suspended: Vec<SuspendedExternalEngine>,
    pub(crate) skipped: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct ResumeResult {
    pub(crate) resumed: Vec<SuspendedExternalEngine>,
    pub(crate) skipped: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StartupReconcilePlan {
    /// Trip-time identities are always frozen again after the shutdown hook
    /// has released them for the app restart.
    pub(crate) resuspend_persisted: bool,
    /// Engines that did not belong to this episode are only discovered when
    /// the episode explicitly opted into newcomer prevention.
    pub(crate) sweep_newcomers: bool,
}

pub(crate) fn startup_reconcile_plan(
    episode_in_force: bool,
    external_suspend: bool,
    prevent_new_sessions: bool,
) -> StartupReconcilePlan {
    StartupReconcilePlan {
        resuspend_persisted: episode_in_force && external_suspend,
        sweep_newcomers: episode_in_force && external_suspend && prevent_new_sessions,
    }
}

fn image_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_ascii_lowercase()
}

pub(crate) fn is_external_codex_engine(path: &str) -> bool {
    let image = image_name(path);
    if matches!(image.as_str(), "codex" | "codex.exe") {
        return true;
    }
    let bare = image.strip_suffix(".exe").unwrap_or(&image);
    let Some(triple) = bare.strip_prefix("codex-") else {
        return false;
    };
    let parts = triple.split('-').collect::<Vec<_>>();
    parts.len() >= 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
        && parts
            .iter()
            .any(|part| matches!(*part, "windows" | "linux" | "darwin"))
}

fn explicitly_excluded(path: &str) -> bool {
    matches!(
        image_name(path)
            .strip_suffix(".exe")
            .unwrap_or(&image_name(path))
            .to_ascii_lowercase()
            .as_str(),
        "codex-usage-limiter" | "codex-code-mode-host"
    )
}

fn descendants(processes: &[ProcessInfo], roots: &HashSet<u32>) -> HashSet<u32> {
    let mut children = HashMap::<u32, Vec<u32>>::new();
    for process in processes {
        children
            .entry(process.parent_pid)
            .or_default()
            .push(process.pid);
    }
    let mut excluded = roots.clone();
    let mut pending = roots.iter().copied().collect::<Vec<_>>();
    while let Some(pid) = pending.pop() {
        if let Some(child_pids) = children.get(&pid) {
            for child in child_pids {
                if excluded.insert(*child) {
                    pending.push(*child);
                }
            }
        }
    }
    excluded
}

fn stable_start_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

pub(crate) fn sweep(
    own_session_pids: HashSet<u32>,
    already_suspended: &HashSet<(u32, u64)>,
    now_ms: i64,
) -> SweepResult {
    let processes = match running_processes() {
        Ok(value) => value,
        Err(error) => {
            return SweepResult {
                skipped: vec![format!("external engine sweep unavailable: {error}")],
                ..Default::default()
            }
        }
    };
    let mut roots = own_session_pids;
    roots.insert(std::process::id());
    let current_image = std::env::current_exe()
        .ok()
        .map(|path| path.to_string_lossy().to_string());
    sweep_matching_processes(
        processes,
        &roots,
        already_suspended,
        current_image.as_deref(),
        now_ms,
        suspend_process,
    )
}

fn sweep_matching_processes(
    processes: Vec<ProcessInfo>,
    own_session_pids: &HashSet<u32>,
    already_suspended: &HashSet<(u32, u64)>,
    current_image: Option<&str>,
    now_ms: i64,
    mut suspend: impl FnMut(u32) -> Result<(), String>,
) -> SweepResult {
    let own_tree = descendants(&processes, own_session_pids);
    let mut result = SweepResult::default();
    // Keep the sweep idempotent even if a process source ever returns the
    // same identity twice. NtSuspendProcess increments a per-process count.
    let mut tracked = already_suspended.clone();
    for process in processes {
        if !is_external_codex_engine(&process.image_path) {
            continue;
        }
        if own_tree.contains(&process.pid)
            || explicitly_excluded(&process.image_path)
            || current_image.is_some_and(|image| image.eq_ignore_ascii_case(&process.image_path))
        {
            result.skipped.push(format!(
                "skipped own/bundle engine {} ({})",
                process.image_path, process.pid
            ));
            continue;
        }
        let identity = (process.pid, process.start_time);
        if !tracked.insert(identity) {
            continue;
        }
        match suspend(process.pid) {
            Ok(()) => result.suspended.push(SuspendedExternalEngine {
                pid: process.pid,
                process_start_time: process.start_time,
                image_path: process.image_path,
                suspended_at: now_ms,
            }),
            Err(error) => {
                // A failed attempt was not suspended, so a later sweep may
                // retry it. Successful identities remain in `tracked` for
                // this pass to prevent a second counted suspension.
                tracked.remove(&identity);
                result.skipped.push(format!(
                    "skipped {} ({}): {error}",
                    process.image_path, process.pid
                ));
            }
        }
    }
    result
}

pub(crate) fn resume(entries: &[SuspendedExternalEngine]) -> ResumeResult {
    let processes = match running_processes() {
        Ok(value) => value
            .into_iter()
            .map(|process| (process.pid, process))
            .collect::<HashMap<_, _>>(),
        Err(error) => {
            return ResumeResult {
                skipped: vec![format!("external engine resume unavailable: {error}")],
                ..Default::default()
            }
        }
    };
    resume_matching_entries(entries, &processes, resume_process)
}

pub(crate) fn merge_suspended_entries(
    existing: &[SuspendedExternalEngine],
    additions: &[SuspendedExternalEngine],
) -> Vec<SuspendedExternalEngine> {
    let mut merged = Vec::new();
    let mut seen = HashSet::new();
    for entry in existing.iter().chain(additions) {
        if seen.insert((entry.pid, entry.process_start_time)) {
            merged.push(entry.clone());
        }
    }
    merged
}

pub(crate) fn remaining_after_resume(
    tracked: &[SuspendedExternalEngine],
    live: &[SuspendedExternalEngine],
    resumed: &[SuspendedExternalEngine],
) -> Vec<SuspendedExternalEngine> {
    let live_ids = live
        .iter()
        .map(|entry| (entry.pid, entry.process_start_time))
        .collect::<HashSet<_>>();
    let resumed_ids = resumed
        .iter()
        .map(|entry| (entry.pid, entry.process_start_time))
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    tracked
        .iter()
        .filter(|entry| {
            let identity = (entry.pid, entry.process_start_time);
            live_ids.contains(&identity)
                && !resumed_ids.contains(&identity)
                && seen.insert(identity)
        })
        .cloned()
        .collect()
}

fn resume_matching_entries(
    entries: &[SuspendedExternalEngine],
    processes: &HashMap<u32, ProcessInfo>,
    mut resume: impl FnMut(u32) -> Result<(), String>,
) -> ResumeResult {
    let mut result = ResumeResult::default();
    let mut seen = HashSet::new();
    for entry in entries {
        if !seen.insert((entry.pid, entry.process_start_time)) {
            continue;
        }
        let Some(process) = processes.get(&entry.pid) else {
            result.skipped.push(format!(
                "dropped exited engine {} ({})",
                entry.image_path, entry.pid
            ));
            continue;
        };
        if process.start_time != entry.process_start_time {
            result.skipped.push(format!(
                "dropped PID-reused engine {} ({})",
                entry.image_path, entry.pid
            ));
            continue;
        }
        match resume(entry.pid) {
            Ok(()) => result.resumed.push(entry.clone()),
            Err(error) => result.skipped.push(format!(
                "skipped resume {} ({}): {error}",
                entry.image_path, entry.pid
            )),
        }
    }
    result
}

/// Re-applies suspension only to the exact persisted identities. This is used
/// after the best-effort shutdown resume has allowed the app to exit cleanly.
/// A PID that has been reused is never touched.
pub(crate) fn resuspend(entries: &[SuspendedExternalEngine]) -> SweepResult {
    let processes = match running_processes() {
        Ok(value) => value
            .into_iter()
            .map(|process| (process.pid, process))
            .collect::<HashMap<_, _>>(),
        Err(error) => {
            return SweepResult {
                skipped: vec![format!("external engine re-suspend unavailable: {error}")],
                ..Default::default()
            }
        }
    };
    resuspend_matching_entries(entries, &processes, suspend_process)
}

fn resuspend_matching_entries(
    entries: &[SuspendedExternalEngine],
    processes: &HashMap<u32, ProcessInfo>,
    mut suspend: impl FnMut(u32) -> Result<(), String>,
) -> SweepResult {
    let mut result = SweepResult::default();
    for entry in entries {
        let Some(process) = processes.get(&entry.pid) else {
            result.skipped.push(format!(
                "dropped exited engine {} ({})",
                entry.image_path, entry.pid
            ));
            continue;
        };
        if process.start_time != entry.process_start_time {
            result.skipped.push(format!(
                "dropped PID-reused engine {} ({})",
                entry.image_path, entry.pid
            ));
            continue;
        }
        match suspend(entry.pid) {
            Ok(()) => result.suspended.push(entry.clone()),
            Err(error) => result.skipped.push(format!(
                "skipped re-suspend {} ({}): {error}",
                entry.image_path, entry.pid
            )),
        }
    }
    result
}

/// Keeps only identities still present. Startup invokes this before deciding
/// whether a persisted blocked episode remains suspended.
pub(crate) fn live_entries(
    entries: &[SuspendedExternalEngine],
) -> (Vec<SuspendedExternalEngine>, Vec<String>) {
    let processes = match running_processes() {
        Ok(value) => value
            .into_iter()
            .map(|process| (process.pid, process))
            .collect::<HashMap<_, _>>(),
        Err(error) => {
            return (
                Vec::new(),
                vec![format!(
                    "external engine identity check unavailable: {error}"
                )],
            )
        }
    };
    matching_entries(entries, &processes)
}

fn matching_entries(
    entries: &[SuspendedExternalEngine],
    processes: &HashMap<u32, ProcessInfo>,
) -> (Vec<SuspendedExternalEngine>, Vec<String>) {
    let mut live = Vec::new();
    let mut skipped = Vec::new();
    let mut seen = HashSet::new();
    for entry in entries {
        if processes
            .get(&entry.pid)
            .is_some_and(|process| process.start_time == entry.process_start_time)
        {
            if seen.insert((entry.pid, entry.process_start_time)) {
                live.push(entry.clone());
            }
        } else {
            skipped.push(format!(
                "dropped stale suspended engine {} ({})",
                entry.image_path, entry.pid
            ));
        }
    }
    (live, skipped)
}

#[cfg(unix)]
fn running_processes() -> Result<Vec<ProcessInfo>, String> {
    let output = std::process::Command::new("ps")
        .args(["-x", "-o", "pid=,ppid=,lstart=,comm="])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let parent_pid = fields.next()?.parse().ok()?;
            let started = (0..5)
                .filter_map(|_| fields.next())
                .collect::<Vec<_>>()
                .join(" ");
            let image_path = fields.collect::<Vec<_>>().join(" ");
            (!image_path.is_empty()).then_some(ProcessInfo {
                pid,
                parent_pid,
                start_time: stable_start_hash(&started),
                image_path,
            })
        })
        .collect())
}

#[cfg(unix)]
fn suspend_process(pid: u32) -> Result<(), String> {
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGSTOP) };
    (result == 0)
        .then_some(())
        .ok_or_else(|| std::io::Error::last_os_error().to_string())
}

#[cfg(unix)]
fn resume_process(pid: u32) -> Result<(), String> {
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGCONT) };
    (result == 0)
        .then_some(())
        .ok_or_else(|| std::io::Error::last_os_error().to_string())
}

#[cfg(target_os = "windows")]
fn running_processes() -> Result<Vec<ProcessInfo>, String> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut processes = Vec::new();
        let mut next = Process32FirstW(snapshot, &mut entry);
        while next != 0 {
            let pid = entry.th32ProcessID;
            if pid != 0 {
                let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                if !handle.is_null() && process_owned_by_current_user(handle) {
                    let mut creation: FILETIME = zeroed();
                    let mut exit: FILETIME = zeroed();
                    let mut kernel: FILETIME = zeroed();
                    let mut user: FILETIME = zeroed();
                    let start_time = if GetProcessTimes(
                        handle,
                        &mut creation,
                        &mut exit,
                        &mut kernel,
                        &mut user,
                    ) != 0
                    {
                        (u64::from(creation.dwHighDateTime) << 32)
                            | u64::from(creation.dwLowDateTime)
                    } else {
                        0
                    };
                    let mut buffer = vec![0_u16; 32_768];
                    let mut length = buffer.len() as u32;
                    let image_path =
                        if QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length)
                            != 0
                        {
                            String::from_utf16_lossy(&buffer[..length as usize])
                        } else {
                            let end = entry
                                .szExeFile
                                .iter()
                                .position(|value| *value == 0)
                                .unwrap_or(entry.szExeFile.len());
                            String::from_utf16_lossy(&entry.szExeFile[..end])
                        };
                    processes.push(ProcessInfo {
                        pid,
                        parent_pid: entry.th32ParentProcessID,
                        start_time,
                        image_path,
                    });
                    CloseHandle(handle);
                }
            }
            entry = zeroed();
            entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
            next = Process32NextW(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
        Ok(processes)
    }
}

#[cfg(target_os = "windows")]
fn process_owned_by_current_user(process: windows_sys::Win32::Foundation::HANDLE) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        EqualSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe fn sid(token: HANDLE) -> Option<Vec<u8>> {
        let mut size = 0_u32;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut size);
        if size == 0 {
            return None;
        }
        let mut data = vec![0_u8; size as usize];
        if GetTokenInformation(token, TokenUser, data.as_mut_ptr().cast(), size, &mut size) == 0 {
            return None;
        }
        Some(data)
    }
    unsafe {
        let mut process_token = std::ptr::null_mut();
        let mut current_token = std::ptr::null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut process_token) == 0
            || OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut current_token) == 0
        {
            return false;
        }
        let process_user = sid(process_token);
        let current_user = sid(current_token);
        CloseHandle(process_token);
        CloseHandle(current_token);
        let Some(process_user) = process_user else {
            return false;
        };
        let Some(current_user) = current_user else {
            return false;
        };
        let process_sid = (&*(process_user.as_ptr() as *const TOKEN_USER)).User.Sid;
        let current_sid = (&*(current_user.as_ptr() as *const TOKEN_USER)).User.Sid;
        EqualSid(process_sid, current_sid) != 0
    }
}

#[cfg(target_os = "windows")]
#[link(name = "ntdll")]
extern "system" {
    fn NtSuspendProcess(process_handle: windows_sys::Win32::Foundation::HANDLE) -> i32;
    fn NtResumeProcess(process_handle: windows_sys::Win32::Foundation::HANDLE) -> i32;
}

#[cfg(target_os = "windows")]
fn with_suspend_handle(
    pid: u32,
    operation: unsafe extern "system" fn(windows_sys::Win32::Foundation::HANDLE) -> i32,
) -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SUSPEND_RESUME,
    };
    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SUSPEND_RESUME,
            0,
            pid,
        );
        if handle.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let status = operation(handle);
        CloseHandle(handle);
        (status >= 0)
            .then_some(())
            .ok_or_else(|| format!("NtSuspend/NtResumeProcess status {status:#x}"))
    }
}

#[cfg(target_os = "windows")]
fn suspend_process(pid: u32) -> Result<(), String> {
    with_suspend_handle(pid, NtSuspendProcess)
}

#[cfg(target_os = "windows")]
fn resume_process(pid: u32) -> Result<(), String> {
    const MAX_RESUME_ATTEMPTS: usize = 32;
    for _ in 0..MAX_RESUME_ATTEMPTS {
        with_suspend_handle(pid, NtResumeProcess)?;
        if !has_suspended_threads(pid)? {
            return Ok(());
        }
    }
    Err(format!(
        "NtResumeProcess did not clear the suspend count after {MAX_RESUME_ATTEMPTS} attempts"
    ))
}

/// Reads each thread's previous suspend count without changing it: the
/// temporary SuspendThread is immediately balanced by ResumeThread. This is
/// the smallest reliable public probe for the count hidden by
/// NtResumeProcess.
#[cfg(target_os = "windows")]
fn has_suspended_threads(pid: u32) -> Result<bool, String> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::Threading::{
        OpenThread, ResumeThread, SuspendThread, THREAD_SUSPEND_RESUME,
    };

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut entry: THREADENTRY32 = zeroed();
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut next = Thread32First(snapshot, &mut entry);
        let mut suspended = false;
        let mut error = None;
        while next != 0 {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if thread.is_null() {
                    error = Some(std::io::Error::last_os_error().to_string());
                    break;
                }
                let previous = SuspendThread(thread);
                if previous == u32::MAX {
                    error = Some(std::io::Error::last_os_error().to_string());
                    CloseHandle(thread);
                    break;
                }
                if ResumeThread(thread) == u32::MAX {
                    error = Some(std::io::Error::last_os_error().to_string());
                    CloseHandle(thread);
                    break;
                }
                CloseHandle(thread);
                suspended |= previous > 0;
            }
            entry = zeroed();
            entry.dwSize = size_of::<THREADENTRY32>() as u32;
            next = Thread32Next(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
        error.map_or(Ok(suspended), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        is_external_codex_engine, matching_entries, remaining_after_resume,
        resume_matching_entries, sweep_matching_processes, ProcessInfo,
    };
    use crate::shared::quota_guard::model::SuspendedExternalEngine;
    use std::collections::{HashMap, HashSet};
    #[test]
    fn matches_only_codex_engines_and_target_triples() {
        assert!(is_external_codex_engine(
            "C:/Program Files/OpenAI/codex.exe"
        ));
        assert!(is_external_codex_engine("/opt/homebrew/bin/codex"));
        assert!(is_external_codex_engine("codex-x86_64-pc-windows-msvc.exe"));
        assert!(is_external_codex_engine("codex-aarch64-apple-darwin"));
        assert!(!is_external_codex_engine("codex-usage-limiter.exe"));
        assert!(!is_external_codex_engine("codex-code-mode-host.exe"));
        assert!(!is_external_codex_engine("codex-helper.exe"));
    }

    #[test]
    fn stale_identity_drops_a_reused_pid_without_resuming_it() {
        let entry = SuspendedExternalEngine {
            pid: 42,
            process_start_time: 10,
            image_path: "codex.exe".into(),
            suspended_at: 1,
        };
        let processes = HashMap::from([(
            42,
            ProcessInfo {
                pid: 42,
                parent_pid: 1,
                start_time: 11,
                image_path: "other.exe".into(),
            },
        )]);
        let (live, skipped) = matching_entries(&[entry], &processes);
        assert!(live.is_empty());
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].contains("stale"));
    }

    #[test]
    fn repeated_sweeps_do_not_suspend_an_already_tracked_identity() {
        let process = ProcessInfo {
            pid: 42,
            parent_pid: 1,
            start_time: 10,
            image_path: "codex.exe".into(),
        };
        let own_pids = HashSet::new();
        let mut suspend_calls = Vec::new();
        let first = sweep_matching_processes(
            vec![process.clone()],
            &own_pids,
            &HashSet::new(),
            None,
            1,
            |pid| {
                suspend_calls.push(pid);
                Ok(())
            },
        );
        let known = first
            .suspended
            .iter()
            .map(|entry| (entry.pid, entry.process_start_time))
            .collect();
        let second = sweep_matching_processes(vec![process], &own_pids, &known, None, 2, |pid| {
            suspend_calls.push(pid);
            Ok(())
        });

        assert_eq!(suspend_calls, vec![42]);
        assert_eq!(first.suspended.len(), 1);
        assert!(second.suspended.is_empty());
    }

    #[test]
    fn duplicate_tracked_identity_resumes_and_clears_once() {
        let entry = SuspendedExternalEngine {
            pid: 42,
            process_start_time: 10,
            image_path: "codex.exe".into(),
            suspended_at: 1,
        };
        let processes = HashMap::from([(
            42,
            ProcessInfo {
                pid: 42,
                parent_pid: 1,
                start_time: 10,
                image_path: "codex.exe".into(),
            },
        )]);
        let mut resume_calls = Vec::new();
        let result = resume_matching_entries(&[entry.clone(), entry.clone()], &processes, |pid| {
            resume_calls.push(pid);
            Ok(())
        });
        let (live, stale) = matching_entries(&[entry.clone(), entry.clone()], &processes);

        assert_eq!(resume_calls, vec![42]);
        assert_eq!(result.resumed, vec![entry.clone()]);
        assert!(
            remaining_after_resume(&[entry.clone(), entry.clone()], &live, &result.resumed)
                .is_empty()
        );
        assert_eq!(live.len(), 1, "the tracked entry has one durable identity");
        assert!(stale.is_empty());
    }

    #[test]
    fn startup_reconcile_resuspends_trip_time_engines_by_default_without_sweeping_newcomers() {
        let trip_time = SuspendedExternalEngine {
            pid: 41,
            process_start_time: 7,
            image_path: "codex.exe".into(),
            suspended_at: 1,
        };
        let processes = HashMap::from([
            (
                41,
                ProcessInfo {
                    pid: 41,
                    parent_pid: 1,
                    start_time: 7,
                    image_path: "codex.exe".into(),
                },
            ),
            (
                42,
                ProcessInfo {
                    pid: 42,
                    parent_pid: 1,
                    start_time: 8,
                    image_path: "codex-aarch64-apple-darwin".into(),
                },
            ),
        ]);
        let mut suspended = Vec::new();
        let result = super::resuspend_matching_entries(&[trip_time.clone()], &processes, |pid| {
            suspended.push(pid);
            Ok(())
        });
        let plan = super::startup_reconcile_plan(true, true, false);
        assert!(plan.resuspend_persisted);
        assert!(!plan.sweep_newcomers);
        assert_eq!(result.suspended, vec![trip_time]);
        assert_eq!(
            suspended,
            vec![41],
            "only the persisted trip-time identity is re-suspended"
        );
    }

    #[test]
    fn startup_reconcile_sweeps_trip_time_and_newcomer_engines_when_enabled() {
        let plan = super::startup_reconcile_plan(true, true, true);
        assert!(plan.resuspend_persisted);
        assert!(plan.sweep_newcomers);
    }
}
