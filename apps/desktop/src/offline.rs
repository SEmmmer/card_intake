//! Desktop commands for local copies. All disk-heavy workers own the same job
//! control as imports, so delivery and ejection cannot start in the middle.

use super::{AppState, imports::ActiveImport};
use card_intake_core::{
    backup::{self, DriveView, Target},
    copies, eject,
    import::{ImportProgress, ImportStatus},
    offline::{self, FolderCheck},
    send, volumes,
};
use chrono::NaiveDateTime;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Manager, State, ipc::Channel};

impl AppState {
    pub fn offline_root(&self) -> Result<PathBuf, String> {
        if let Some(folder) = &self
            .settings
            .lock()
            .map_err(|e| e.to_string())?
            .offline_folder
        {
            return Ok(PathBuf::from(folder));
        }
        let pictures = self
            .pictures
            .clone()
            .or_else(|| {
                std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                    .map(|p| PathBuf::from(p).join("Pictures"))
            })
            .ok_or("Couldn't find Pictures. Save an offline folder first.")?;
        Ok(pictures.join("Card Intake Offline"))
    }

    pub fn begin_copy(&self) -> Result<ActiveImport, String> {
        let _gate = self.activity_gate.lock().map_err(|e| e.to_string())?;
        if self.delivering.load(Ordering::SeqCst) {
            return Err("Wait for the delivery to finish.".into());
        }
        self.imports.begin()
    }

    fn backup_views(&self) -> Result<Vec<DriveView>, String> {
        let drives = self
            .settings
            .lock()
            .map_err(|e| e.to_string())?
            .backup_drives
            .clone();
        Ok(backup::connected(&drives, &volumes::mount_points()))
    }

    pub fn backup_targets(&self) -> Result<Vec<Target>, String> {
        Ok(self
            .backup_views()?
            .into_iter()
            .filter_map(|v| {
                v.root.map(|root| Target {
                    uuid: v.drive.uuid,
                    label: v.drive.label,
                    root,
                })
            })
            .collect())
    }
}

#[derive(Serialize)]
pub struct Candidate {
    root: PathBuf,
    label: String,
}

#[derive(Serialize)]
pub struct OfflineCopies {
    folder: PathBuf,
    drives: Vec<DriveView>,
    candidates: Vec<Candidate>,
}

#[tauri::command]
pub async fn get_offline_copies(app: AppHandle) -> Result<OfflineCopies, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let folder = state.offline_root()?;
        let offline_volume = offline::disk(&folder).ok().map(|f| f.root);
        let candidates = volumes::mount_points()
            .into_iter()
            .filter_map(|root| {
                let facts = offline::disk(&root).ok()?;
                (facts.local
                    && !facts.system
                    && offline_volume.as_ref() != Some(&root)
                    && volumes::camera_card(&root).is_none())
                .then(|| Candidate {
                    root,
                    label: facts.label.unwrap_or_else(|| "Backup drive".into()),
                })
            })
            .collect();
        Ok(OfflineCopies {
            folder,
            drives: state.backup_views()?,
            candidates,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn check_offline_folder(path: String) -> Result<FolderCheck, String> {
    tauri::async_runtime::spawn_blocking(move || offline::check(Path::new(path.trim())))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn save_offline_folder(path: String, app: AppHandle) -> Result<FolderCheck, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let _active = state.begin_copy()?;
        let path = path.trim();
        let check = offline::check(Path::new(path));
        if !check.ok {
            return Ok(check);
        }
        let mut settings = state.settings.lock().map_err(|e| e.to_string())?;
        let mut updated = settings.clone();
        updated.offline_folder = Some(path.to_string());
        updated
            .save(&state.settings_path)
            .map_err(|e| e.to_string())?;
        *settings = updated;
        Ok(check)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn check_init_backup(
    path: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<backup::InitCheck, String> {
    let active = state.begin_copy()?;
    tauri::async_runtime::spawn_blocking(move || {
        let _active = active;
        let state = app.state::<AppState>();
        let check = backup::check_init(Path::new(path.trim()), &state.offline_root()?);
        if let Some(drive) = &check.drive {
            let mut settings = state.settings.lock().map_err(|e| e.to_string())?;
            let mut updated = settings.clone();
            if !updated.backup_drives.iter().any(|d| d.uuid == drive.uuid) {
                updated.backup_drives.push(drive.clone());
            }
            updated
                .save(&state.settings_path)
                .map_err(|e| e.to_string())?;
            *settings = updated;
        }
        Ok(check)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn forget_backup(uuid: String, state: State<'_, AppState>) -> Result<(), String> {
    let _active = state.begin_copy()?;
    let mut settings = state.settings.lock().map_err(|e| e.to_string())?;
    let mut updated = settings.clone();
    updated.backup_drives.retain(|d| d.uuid != uuid);
    updated
        .save(&state.settings_path)
        .map_err(|e| e.to_string())?;
    *settings = updated;
    Ok(())
}

#[tauri::command]
pub async fn eject_backup(
    uuid: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let active = state.begin_copy()?;
    tauri::async_runtime::spawn_blocking(move || {
        let _active = active;
        let state = app.state::<AppState>();
        let drive = state
            .backup_targets()?
            .into_iter()
            .find(|d| d.uuid == uuid)
            .ok_or("The drive isn't connected.")?;
        drive.check()?;
        eject::eject_card(&drive.root)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn back_up_now(
    uuid: String,
    on_progress: Channel<ImportProgress>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<backup::BackupResult, String> {
    let active = state.begin_copy()?;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let drive = state
            .backup_targets()?
            .into_iter()
            .find(|d| d.uuid == uuid)
            .ok_or("The drive isn't connected.")?;
        let folder = state.offline_root()?;
        offline::validate(&folder)?;
        let files = copies::files(&folder, Path::new(""))?;
        Ok(backup::back_up(&files, &drive, &active.cancel, |p| {
            if on_progress.send(p).is_err() {
                active.cancel.store(true, Ordering::Relaxed);
            }
        }))
    })
    .await
    .map_err(|e| e.to_string())?
}

fn manifest_path(state: &AppState, import_id: NaiveDateTime) -> PathBuf {
    state
        .ledger_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("sent-manifests")
        .join(format!("{}.json", import_id.format("%Y%m%dT%H%M%S%.9f")))
}

fn source_manifest_path(state: &AppState, import_id: NaiveDateTime) -> PathBuf {
    state
        .ledger_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("offline-manifests")
        .join(format!("{}.json", import_id.format("%Y%m%dT%H%M%S%.9f")))
}

/// The laptop files verified on the NAS, which Delete laptop copy may remove.
pub(super) fn save_sent_manifest(
    state: &AppState,
    import_id: NaiveDateTime,
    manifest: &send::Manifest,
) -> Result<(), String> {
    manifest.save(&manifest_path(state, import_id))
}

pub(super) fn save_import_manifest(
    state: &AppState,
    result: &card_intake_core::import::ImportResult,
) -> Result<(), String> {
    let manifest = send::Manifest {
        offline_root: result.nas_root.clone(),
        shoots: result.shoots.clone(),
        files: result
            .local_files
            .iter()
            .map(|f| send::VerifiedFile {
                relative: f.relative.clone(),
                size: f.size,
                modified: f.modified,
            })
            .collect(),
    };
    manifest.save(&source_manifest_path(state, result.started_at))
}

#[tauri::command]
pub async fn send_to_nas(
    import_id: NaiveDateTime,
    on_progress: Channel<ImportProgress>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<send::SendResult, String> {
    let active = state.begin_copy()?;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let record = state
            .ledger
            .lock()
            .map_err(|e| e.to_string())?
            .imports
            .iter()
            .find(|r| r.started_at == import_id && r.offline && r.sent.is_none())
            .cloned()
            .ok_or("This import isn't waiting to be sent.")?;
        if !record.local_complete && record.status != ImportStatus::Complete {
            return Err("Resume the import to finish the laptop copy first.".into());
        }
        let nas = state
            .shared_root()
            .ok_or("Connect a confirmed network photo share first.")?;
        let folder = record.offline_root.as_ref().unwrap_or(&record.nas_root);
        if folder.exists() {
            offline::validate(folder)?;
        }
        let source = send::Manifest::load(&source_manifest_path(&state, import_id)).map_err(|e| format!("Couldn't read this import's local file list. Resume the import to save it before sending: {e}"))?;
        if &source.offline_root != folder || source.shoots != record.shoots { return Err("The local file list doesn't match this import. Resume the import before sending.".into()); }
        let result = send::send_import(
            &source,
            &state.backup_targets()?,
            &nas,
            true,
            &active.cancel,
            |p| {
                if on_progress.send(p).is_err() {
                    active.cancel.store(true, Ordering::Relaxed);
                }
            },
        );
        if let Some(sent) = &result.sent {
            // Save the deletion manifest first. A history save failure leaves all
            // local files intact and Send can be safely pressed again.
            result.manifest.save(&manifest_path(&state, import_id))?;
            let mut ledger = state.ledger.lock().map_err(|e| e.to_string())?;
            let mut updated = ledger.clone();
            let saved = updated
                .imports
                .iter_mut()
                .find(|r| r.started_at == import_id)
                .ok_or("Import no longer in history")?;
            saved.offline_root = Some(folder.clone());
            saved.nas_root = nas.clone();
            saved.sent = Some(sent.clone());
            saved.status = ImportStatus::Complete;
            saved.error = None;
            saved.verified_files = saved.total_files;
            saved.shoots.extend(sent.found_in.iter().cloned());
            saved.shoots.retain(|s| nas.join(s).is_dir());
            saved.shoots.sort();
            saved.shoots.dedup();
            state.save_ledger(&updated)?;
            *ledger = updated;
        }
        Ok(result)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn delete_laptop_copy(
    import_id: NaiveDateTime,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<send::Deleted, String> {
    let active = state.begin_copy()?;
    tauri::async_runtime::spawn_blocking(move || {
        let _active = active;
        let state = app.state::<AppState>();
        let record = state
            .ledger
            .lock()
            .map_err(|e| e.to_string())?
            .imports
            .iter()
            .find(|r| r.started_at == import_id && r.sent.is_some() && !r.laptop_deleted)
            .cloned()
            .ok_or("Send and verify this import on the NAS first.")?;
        let manifest = send::Manifest::load(&manifest_path(&state, import_id))?;
        if record.offline_root.as_ref() != Some(&manifest.offline_root) {
            return Err("The saved laptop folder doesn't match this import.".into());
        }
        let result = send::delete_laptop(&manifest)?;
        if result.complete {
            let mut ledger = state.ledger.lock().map_err(|e| e.to_string())?;
            let mut updated = ledger.clone();
            if let Some(saved) = updated
                .imports
                .iter_mut()
                .find(|r| r.started_at == import_id)
            {
                saved.laptop_deleted = true;
            }
            state.save_ledger(&updated)?;
            *ledger = updated;
        }
        Ok(result)
    })
    .await
    .map_err(|e| e.to_string())?
}
