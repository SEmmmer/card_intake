use super::{AppState, nas_root};
use card_intake_core::{
    eject,
    import::{self, ImportProgress, ImportRequest, ImportResult, ImportStatus, ShootSelection},
    volume_info,
};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, State, ipc::Channel};

#[derive(Default)]
pub struct ImportControl {
    active: Mutex<Option<Arc<AtomicBool>>>,
}

impl ImportControl {
    pub fn is_active(&self) -> bool {
        self.active.lock().map(|a| a.is_some()).unwrap_or(true)
    }

    pub fn cancel(&self) -> Result<(), String> {
        if let Some(cancel) = self.active.lock().map_err(|e| e.to_string())?.as_ref() {
            cancel.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    fn begin(self: &Arc<Self>) -> Result<ActiveImport, String> {
        let mut active = self.active.lock().map_err(|e| e.to_string())?;
        if active.is_some() {
            return Err("An import is already running. Stop it or wait for it to finish.".into());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        *active = Some(cancel.clone());
        Ok(ActiveImport {
            control: self.clone(),
            cancel,
        })
    }
}

/// Owned by the worker, so dropping an IPC request cannot unlock a running import.
struct ActiveImport {
    control: Arc<ImportControl>,
    cancel: Arc<AtomicBool>,
}

impl Drop for ActiveImport {
    fn drop(&mut self) {
        if let Ok(mut active) = self.control.active.lock() {
            *active = None;
        }
    }
}

#[derive(Deserialize)]
pub struct StartImport {
    root: PathBuf,
    expected_card_id: Option<String>,
    shoots: Vec<ShootSelection>,
    include_videos: bool,
    verify: bool,
    eject: bool,
    #[serde(default)]
    camera: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingImport {
    request: ImportRequest,
    eject: bool,
}

/// The last import that didn't finish, saved so the app can offer to resume it with
/// the same names and options after a restart.
impl PendingImport {
    fn load(path: &Path) -> Result<Option<Self>, String> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| format!("Couldn't read the saved import: {e}")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        let dir = path.parent().ok_or("Missing settings folder")?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let mut file = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
        file.write_all(&serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        file.as_file().sync_all().map_err(|e| e.to_string())?;
        file.persist(path).map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn core_request(request: StartImport, state: &AppState) -> Result<PendingImport, String> {
    if request.eject && !request.verify {
        return Err("Enable file verification before choosing automatic ejection.".into());
    }
    let destination = nas_root(&*state.settings.lock().map_err(|e| e.to_string())?).0;
    let card_id = request
        .expected_card_id
        .or_else(|| volume_info::volume_info(&request.root).id);
    Ok(PendingImport {
        eject: request.eject,
        request: ImportRequest {
            root: request.root,
            nas_root: PathBuf::from(destination),
            expected_card_id: card_id,
            shoots: request.shoots,
            include_videos: request.include_videos,
            verify: request.verify,
            workers: 0,
            camera: request.camera,
        },
    })
}

#[tauri::command]
pub fn get_pending_import(state: State<'_, AppState>) -> Result<Option<PendingImport>, String> {
    PendingImport::load(&state.pending_path)
}

#[tauri::command]
pub async fn start_import(
    request: StartImport,
    on_progress: Channel<ImportProgress>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ImportResult, String> {
    let active = state.imports.begin()?;
    let job = core_request(request, &state)?;
    job.save(&state.pending_path)?;
    run_import(job, active, on_progress, app).await
}

async fn run_import(
    job: PendingImport,
    active: ActiveImport,
    on_progress: Channel<ImportProgress>,
    app: AppHandle,
) -> Result<ImportResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let PendingImport { request, eject } = job;
        let mut last = Instant::now();
        let mut phase = "";
        let mut result = import::import_card(&request, &active.cancel, |progress| {
            if progress.phase != phase || progress.phase == "file_complete" || last.elapsed() >= Duration::from_millis(100) {
                phase = progress.phase;
                last = Instant::now();
                if on_progress.send(progress).is_err() {
                    active.cancel.store(true, Ordering::Relaxed);
                }
            }
        });
        if eject && result.status == ImportStatus::Complete && !active.cancel.load(Ordering::Relaxed) {
            // Never eject a replacement card inserted just after the last copy.
            if volume_info::volume_info(&request.root).id != result.card_id {
                result.warnings.push("The card changed after copying; it was not ejected.".into());
            } else {
                match eject::eject_card(&request.root) {
                    Ok(()) => result.ejected = true,
                    Err(error) => result.warnings.push(error),
                }
            }
        }
        let state = app.state::<AppState>();
        if result.status == ImportStatus::Complete
            && let Err(e) = std::fs::remove_file(&state.pending_path)
            && e.kind() != io::ErrorKind::NotFound {
            result.warnings.push(format!("Import completed, but its resume record couldn't be removed: {e}"));
        }
        match state.ledger.lock() {
            Ok(mut ledger) => {
                result.card_key = result.card_id.as_deref().and_then(|id| ledger.card_key_for_volume(id));
                ledger.record_import(result.clone());
                if let Err(error) = state.save_ledger(&ledger) {
                    result.warnings.push(format!("Files keep their reported import status, but history could not be saved: {error}"));
                }
            }
            Err(error) => result.warnings.push(format!("Import history could not be saved: {error}")),
        }
        drop(active);
        result
    }).await.map_err(|e| format!("The import worker stopped unexpectedly: {e}. Retry to check existing copies."))
}

#[tauri::command]
pub fn cancel_import(state: State<'_, AppState>) -> Result<(), String> {
    state.imports.cancel()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_job_survives_restart_with_names_destination_and_options() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-import.json");
        assert!(PendingImport::load(&path).unwrap().is_none());
        let mut job = PendingImport {
            request: ImportRequest {
                root: dir.path().join("card"),
                nas_root: PathBuf::from(r"\\NAS\photos"),
                expected_card_id: Some("original-card".into()),
                shoots: vec![ShootSelection {
                    date: chrono::NaiveDate::from_ymd_opt(2026, 11, 7).unwrap(),
                    name: "東京".into(),
                }],
                include_videos: false,
                verify: true,
                workers: 4,
                camera: None,
            },
            eject: true,
        };
        job.save(&path).unwrap();
        let loaded = PendingImport::load(&path).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&loaded).unwrap(),
            serde_json::to_value(&job).unwrap()
        );
        job.eject = false;
        job.save(&path).unwrap();
        assert!(!PendingImport::load(&path).unwrap().unwrap().eject);
        std::fs::write(&path, b"damaged").unwrap();
        assert!(PendingImport::load(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"damaged");
    }

    #[test]
    fn only_one_worker_runs_and_cancellation_is_reset_for_retry() {
        let control = Arc::new(ImportControl::default());
        let active = control.begin().unwrap();
        assert!(control.begin().is_err());
        control.cancel().unwrap();
        assert!(active.cancel.load(Ordering::Relaxed));
        drop(active);
        assert!(!control.is_active());
        let retry = control.begin().unwrap();
        assert!(!retry.cancel.load(Ordering::Relaxed));
    }
}
