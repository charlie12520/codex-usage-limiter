use super::model::QuotaGuardRuntimeState;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
#[derive(Debug)]
pub(crate) enum LoadRuntime {
    Missing,
    Valid(QuotaGuardRuntimeState),
    Corrupt { quarantined_to: PathBuf },
}
fn backup_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "{}bak",
        path.extension()
            .and_then(|v| v.to_str())
            .unwrap_or_default()
    ))
}
fn temporary_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "{}tmp",
        path.extension()
            .and_then(|v| v.to_str())
            .unwrap_or_default()
    ))
}
fn parse(path: &Path) -> Result<QuotaGuardRuntimeState, String> {
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if let Some(account) = value
        .get_mut("account")
        .and_then(serde_json::Value::as_object_mut)
    {
        for key in [
            "revalidationReturnPhase",
            "episodePolicy",
            "firedEpisodes",
            "breachedWindows",
            "drainDeadline",
            "allowedDrainTurns",
            "pendingInterruptIndex",
            "pendingLocalStarts",
            "unmatchedStartedTurns",
            "terminalObservations",
        ] {
            account.remove(key);
        }
    }
    let state: QuotaGuardRuntimeState = serde_json::from_value(value).map_err(|e| e.to_string())?;
    if state.schema_version > 2 {
        return Err(format!(
            "unsupported quota guard runtime schema {}",
            state.schema_version
        ));
    }
    Ok(state)
}
pub(crate) fn load_runtime(path: &Path, timestamp_ms: i64) -> LoadRuntime {
    if let Ok(state) = parse(path) {
        return LoadRuntime::Valid(state);
    }
    let backup = backup_path(path);
    if let Ok(state) = parse(&backup) {
        return LoadRuntime::Valid(state);
    }
    if !path.exists() && !backup.exists() {
        return LoadRuntime::Missing;
    }
    let corrupt = path.with_file_name(format!(
        "{}.corrupt-{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("quota-guard-state.json"),
        timestamp_ms
    ));
    if path.exists() {
        let _ = fs::rename(path, &corrupt);
    }
    LoadRuntime::Corrupt {
        quarantined_to: corrupt,
    }
}
#[cfg(target_os = "windows")]
fn replace_file(destination: &Path, replacement: &Path, backup: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
    let wide = |p: &Path| {
        p.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<u16>>()
    };
    let (a, b, c) = (wide(destination), wide(replacement), wide(backup));
    if unsafe {
        ReplaceFileW(
            a.as_ptr(),
            b.as_ptr(),
            c.as_ptr(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}
#[cfg(not(target_os = "windows"))]
fn replace_file(destination: &Path, replacement: &Path, backup: &Path) -> Result<(), String> {
    if destination.exists() {
        fs::rename(destination, backup).map_err(|e| e.to_string())?
    }
    fs::rename(replacement, destination).map_err(|e| e.to_string())
}
pub(crate) fn persist_runtime(path: &Path, state: &QuotaGuardRuntimeState) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "quota guard state path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temporary = temporary_path(path);
    let backup = backup_path(path);
    let bytes = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
    let mut file = File::create(&temporary).map_err(|e| e.to_string())?;
    file.write_all(&bytes).map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    if path.exists() {
        replace_file(path, &temporary, &backup)?
    } else {
        fs::rename(&temporary, path).map_err(|e| e.to_string())?
    }
    if backup.exists() {
        fs::remove_file(backup).map_err(|e| e.to_string())?
    }
    Ok(())
}
