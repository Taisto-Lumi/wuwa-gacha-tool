use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::State;

use crate::onedrive::{self, DeviceLoginInfo, OneDriveStatus, PollResult, UploadResult};
use crate::AppState;

const SYNC_TEMP_DIR_NAME: &str = "sync-temp";

fn is_sync_snapshot_name(name: &str) -> bool {
    (name.starts_with("sync-local-") || name.starts_with("sync-remote-")) && name.ends_with(".db")
}

/// Removes snapshots left by an interrupted sync. Only the two historical
/// snapshot filename patterns are eligible, both in the old root location and
/// in the dedicated temporary directory.
pub fn cleanup_stale_sync_files(app_data_dir: &Path) {
    let directories = [
        app_data_dir.to_path_buf(),
        app_data_dir.join(SYNC_TEMP_DIR_NAME),
    ];
    let mut removed = 0usize;
    for directory in directories {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
                || !is_sync_snapshot_name(name)
            {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(error) => log::warn!(
                    target: "app::onedrive",
                    "event=sync_temp_cleanup_failed file={} error={error}",
                    path.display()
                ),
            }
        }
    }
    if removed > 0 {
        log::info!(
            target: "app::onedrive",
            "event=sync_temp_cleanup_completed removed={removed}"
        );
    }
}

struct SyncTempFiles {
    local: PathBuf,
    remote: PathBuf,
}

impl SyncTempFiles {
    fn new(directory: &Path, stamp: u128) -> Result<Self, String> {
        std::fs::create_dir_all(directory)
            .map_err(|error| format!("创建同步临时目录失败: {error}"))?;
        Ok(Self {
            local: directory.join(format!("sync-local-{stamp}.db")),
            remote: directory.join(format!("sync-remote-{stamp}.db")),
        })
    }
}

impl Drop for SyncTempFiles {
    fn drop(&mut self) {
        for path in [&self.local, &self.remote] {
            if let Err(error) = std::fs::remove_file(path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    log::warn!(
                        target: "app::onedrive",
                        "event=sync_temp_remove_failed file={} error={error}",
                        path.display()
                    );
                }
            }
        }
    }
}

#[derive(Debug, Serialize)]
pub struct LoginPollStatus {
    pub connected: bool,
    pub pending: bool,
}

#[derive(Debug, Serialize)]
pub struct OneDriveSyncResult {
    pub added_count: usize,
    pub duplicate_count: usize,
    pub total_count: usize,
    pub uploaded_count: usize,
    pub conflict_retries: u32,
    pub updated_at: String,
}

#[tauri::command]
pub async fn get_onedrive_status(state: State<'_, AppState>) -> Result<OneDriveStatus, String> {
    let auth = state.onedrive.lock().await;
    Ok(onedrive::status(&auth))
}

#[tauri::command]
pub async fn start_onedrive_login(state: State<'_, AppState>) -> Result<DeviceLoginInfo, String> {
    let mut auth = state.onedrive.lock().await;
    onedrive::start_login(&state.http, &mut auth).await
}

#[tauri::command]
pub async fn poll_onedrive_login(state: State<'_, AppState>) -> Result<LoginPollStatus, String> {
    let mut auth = state.onedrive.lock().await;
    match onedrive::poll_login(&state.http, &mut auth).await? {
        PollResult::Pending => Ok(LoginPollStatus {
            connected: false,
            pending: true,
        }),
        PollResult::Connected => Ok(LoginPollStatus {
            connected: true,
            pending: false,
        }),
    }
}

#[tauri::command]
pub async fn cancel_onedrive_login(state: State<'_, AppState>) -> Result<(), String> {
    state.onedrive.lock().await.pending = None;
    Ok(())
}

#[tauri::command]
pub async fn disconnect_onedrive(state: State<'_, AppState>) -> Result<(), String> {
    let mut auth = state.onedrive.lock().await;
    onedrive::disconnect(&mut auth)
}

fn hash_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[tauri::command]
pub async fn sync_onedrive_database(
    state: State<'_, AppState>,
    _player_id: String,
    strategy: Option<String>,
) -> Result<OneDriveSyncResult, String> {
    let _sync_guard = state.sync_operation.lock().await;
    let token = {
        let mut auth = state.onedrive.lock().await;
        onedrive::access_token(&state.http, &mut auth).await?
    };
    let folder = onedrive::ensure_sync_directories(&state.http, &token).await?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let temp_files = SyncTempFiles::new(&state.app_data_dir.join(SYNC_TEMP_DIR_NAME), stamp)?;
    let local_path = &temp_files.local;
    let remote_path = &temp_files.remote;
    let (before_count, baseline_etag, baseline_hash) = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        db.create_sync_snapshot(&local_path)?;
        let (etag, hash) = db.cloud_sync_baseline()?;
        (db.record_count()?, etag, hash)
    };
    let local_hash = hash_file(&local_path)?;
    let remote = onedrive::download_snapshot(&state.http, &token, &folder).await?;

    let result = match remote {
        None => {
            match onedrive::upload_snapshot(
                &state.http,
                &token,
                &folder,
                std::fs::read(&local_path).map_err(|e| e.to_string())?,
                None,
            )
            .await?
            {
                UploadResult::Conflict => {
                    Err("云端数据库刚刚被其他设备创建，请重新同步".to_string())
                }
                UploadResult::Uploaded(etag) => {
                    state
                        .db
                        .lock()
                        .map_err(|e| e.to_string())?
                        .save_cloud_sync_baseline(&etag, &local_hash)?;
                    Ok((before_count, before_count, 0))
                }
            }
        }
        Some((remote_etag, bytes)) => {
            std::fs::write(&remote_path, bytes).map_err(|e| e.to_string())?;
            let remote_changed = baseline_etag.as_deref() != Some(remote_etag.as_str());
            let local_changed = baseline_hash.as_deref() != Some(local_hash.as_str());
            if baseline_etag.is_none()
                && before_count > 0
                && strategy.as_deref() != Some("local")
                && strategy.as_deref() != Some("remote")
            {
                Err("本机和云端都已有数据，首次连接时无法判断应保留哪一版；请先在另一端同步，或清空本机数据后重新拉取".to_string())
            } else if remote_changed
                && local_changed
                && baseline_etag.is_some()
                && strategy.as_deref() != Some("local")
                && strategy.as_deref() != Some("remote")
            {
                Err(
                    "本机和云端数据库都已发生变化。为避免覆盖，请先保留其中一端的修改后再同步"
                        .to_string(),
                )
            } else if strategy.as_deref() == Some("remote") || (!remote_changed && !local_changed) {
                let db = state.db.lock().map_err(|e| e.to_string())?;
                db.apply_sync_snapshot(&remote_path)?;
                let after = db.record_count()?;
                db.create_sync_snapshot(&local_path)?;
                db.save_cloud_sync_baseline(&remote_etag, &hash_file(&local_path)?)?;
                Ok((after, 0, after.saturating_sub(before_count)))
            } else if !remote_changed && local_changed || strategy.as_deref() == Some("local") {
                match onedrive::upload_snapshot(
                    &state.http,
                    &token,
                    &folder,
                    std::fs::read(&local_path).map_err(|e| e.to_string())?,
                    Some(&remote_etag),
                )
                .await?
                {
                    UploadResult::Conflict => Err("云端数据库已变化，请重新同步".to_string()),
                    UploadResult::Uploaded(etag) => {
                        state
                            .db
                            .lock()
                            .map_err(|e| e.to_string())?
                            .save_cloud_sync_baseline(&etag, &local_hash)?;
                        Ok((before_count, before_count, 0))
                    }
                }
            } else {
                let db = state.db.lock().map_err(|e| e.to_string())?;
                db.apply_sync_snapshot(&remote_path)?;
                let after = db.record_count()?;
                db.create_sync_snapshot(&local_path)?;
                db.save_cloud_sync_baseline(&remote_etag, &hash_file(&local_path)?)?;
                Ok((after, 0, after.saturating_sub(before_count)))
            }
        }
    };
    let (total, uploaded, added) = result?;
    Ok(OneDriveSyncResult {
        added_count: added,
        duplicate_count: 0,
        total_count: total,
        uploaded_count: uploaded,
        conflict_retries: 0,
        updated_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Deprecated compatibility alias for clients released before the database-wide sync rename.
#[tauri::command]
pub async fn sync_onedrive_uid(
    state: State<'_, AppState>,
    player_id: String,
) -> Result<OneDriveSyncResult, String> {
    sync_onedrive_database(state, player_id, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_removes_legacy_and_new_snapshot_files_only() {
        let root =
            std::env::temp_dir().join(format!("wuwa-sync-cleanup-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(SYNC_TEMP_DIR_NAME)).unwrap();
        for path in [
            root.join("sync-local-1.db"),
            root.join("sync-remote-2.db"),
            root.join(SYNC_TEMP_DIR_NAME).join("sync-local-3.db"),
            root.join(SYNC_TEMP_DIR_NAME).join("sync-remote-4.db"),
        ] {
            std::fs::write(path, b"snapshot").unwrap();
        }
        std::fs::write(root.join("sync-local-not-a-db.txt"), b"keep").unwrap();
        std::fs::write(root.join("other.db"), b"keep").unwrap();

        cleanup_stale_sync_files(&root);

        assert!(!root.join("sync-local-1.db").exists());
        assert!(!root.join("sync-remote-2.db").exists());
        assert!(!root
            .join(SYNC_TEMP_DIR_NAME)
            .join("sync-local-3.db")
            .exists());
        assert!(!root
            .join(SYNC_TEMP_DIR_NAME)
            .join("sync-remote-4.db")
            .exists());
        assert!(root.join("sync-local-not-a-db.txt").exists());
        assert!(root.join("other.db").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
