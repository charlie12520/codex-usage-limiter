use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use super::model::QuotaGuardRuntimeState;

#[derive(Debug)]
pub(crate) enum LoadRuntime {
    Missing,
    Valid(QuotaGuardRuntimeState),
    Corrupt { quarantined_to: PathBuf },
}

fn backup_path(path: &Path) -> PathBuf { path.with_extension(format!("{}bak", path.extension().and_then(|value| value.to_str()).unwrap_or_default())) }
fn temporary_path(path: &Path) -> PathBuf { path.with_extension(format!("{}tmp", path.extension().and_then(|value| value.to_str()).unwrap_or_default())) }

fn parse(path: &Path) -> Result<QuotaGuardRuntimeState, String> {
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let state: QuotaGuardRuntimeState = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if state.schema_version != 1 { return Err(format!("unsupported quota guard runtime schema {}", state.schema_version)); }
    Ok(state)
}

pub(crate) fn load_runtime(path: &Path, timestamp_ms: i64) -> LoadRuntime {
    if let Ok(state) = parse(path) { return LoadRuntime::Valid(state); }
    let backup = backup_path(path);
    if let Ok(state) = parse(&backup) { return LoadRuntime::Valid(state); }
    if !path.exists() && !backup.exists() { return LoadRuntime::Missing; }
    let corrupt = path.with_file_name(format!("{}.corrupt-{}", path.file_name().and_then(|name| name.to_str()).unwrap_or("quota-guard-state.json"), timestamp_ms));
    if path.exists() { let _ = fs::rename(path, &corrupt); }
    LoadRuntime::Corrupt { quarantined_to: corrupt }
}

#[cfg(target_os = "windows")]
fn replace_file(destination: &Path, replacement: &Path, backup: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
    let wide = |path: &Path| path.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let destination = wide(destination); let replacement = wide(replacement); let backup = wide(backup);
    // ReplaceFileW preserves an on-disk backup if replacement fails; it is removed
    // after a successful write by the caller.
    let result = unsafe { ReplaceFileW(destination.as_ptr(), replacement.as_ptr(), backup.as_ptr(), 0, std::ptr::null(), std::ptr::null()) };
    if result == 0 { return Err(std::io::Error::last_os_error().to_string()); }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn replace_file(destination: &Path, replacement: &Path, backup: &Path) -> Result<(), String> {
    if destination.exists() { fs::rename(destination, backup).map_err(|error| error.to_string())?; }
    fs::rename(replacement, destination).map_err(|error| error.to_string())
}

pub(crate) fn persist_runtime(path: &Path, state: &QuotaGuardRuntimeState) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| "quota guard state path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = temporary_path(path);
    let backup = backup_path(path);
    let bytes = serde_json::to_vec_pretty(state).map_err(|error| error.to_string())?;
    let mut file = File::create(&temporary).map_err(|error| error.to_string())?;
    file.write_all(&bytes).map_err(|error| error.to_string())?;
    file.flush().map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    drop(file);
    if path.exists() { replace_file(path, &temporary, &backup)?; } else { fs::rename(&temporary, path).map_err(|error| error.to_string())?; }
    if backup.exists() { fs::remove_file(backup).map_err(|error| error.to_string())?; }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};
    use super::{load_runtime, persist_runtime, LoadRuntime};
    use crate::shared::quota_guard::model::QuotaGuardRuntimeState;

    fn path() -> std::path::PathBuf { std::env::temp_dir().join(format!("quota-guard-{}.json", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())) }
    #[test]
    fn round_trip_and_backup_recovery_work() {
        let path = path(); let mut state = QuotaGuardRuntimeState::default(); state.lifecycle_generation = 7; persist_runtime(&path, &state).unwrap();
        assert!(matches!(load_runtime(&path, 1), LoadRuntime::Valid(value) if value.lifecycle_generation == 7));
        fs::write(&path, b"not json").unwrap(); let backup = path.with_extension("jsonbak"); fs::write(&backup, serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(matches!(load_runtime(&path, 2), LoadRuntime::Valid(value) if value.lifecycle_generation == 7)); let _ = fs::remove_file(path); let _ = fs::remove_file(backup);
    }
    #[test]
    fn corrupt_file_is_quarantined() {
        let path = path(); fs::write(&path, b"bad").unwrap(); assert!(matches!(load_runtime(&path, 99), LoadRuntime::Corrupt { .. })); assert!(!path.exists());
    }
}
