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
    Arc, Mutex, RwLock, RwLockReadGuard,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, State, ipc::Channel};

#[derive(Default)]
pub struct ImportControl {
    active: Mutex<Option<Arc<AtomicBool>>>,
    card_done: AtomicBool,
    pub(super) ready: Mutex<Option<ImportResult>>,
    pub(super) card_reads: RwLock<()>,
    pub(super) catalogs: Mutex<super::lightroom::UploadCatalogs>,
}

impl ImportControl {
    pub fn is_active(&self) -> bool {
        self.active.lock().map(|a| a.is_some()).unwrap_or(true)
    }

    pub fn card_is_done(&self) -> bool {
        self.card_done.load(Ordering::Relaxed)
    }

    pub(super) fn reading_card(&self) -> Result<RwLockReadGuard<'_, ()>, String> {
        let reading = self.card_reads.read().map_err(|e| e.to_string())?;
        if self.is_active() && self.card_is_done() {
            return Err(
                "The card is done. Wait for the NAS upload before reading a card again.".into(),
            );
        }
        Ok(reading)
    }

    pub(super) fn eject_when_ready(
        &self,
        eject: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let _readers = self.card_reads.write().map_err(|e| e.to_string())?;
        let active = self.active.lock().map_err(|e| e.to_string())?;
        if active.is_some() && !self.card_is_done() {
            return Err("Wait for the card to be read and checked before ejecting it.".into());
        }
        eject()
    }

    pub(super) fn ready_import(&self, id: chrono::NaiveDateTime) -> Option<ImportResult> {
        self.ready
            .lock()
            .ok()?
            .as_ref()
            .filter(|r| r.started_at == id)
            .cloned()
    }

    pub(super) fn cleanup_catalogs(&self) {
        if let Ok(mut catalogs) = self.catalogs.lock() {
            catalogs.clear();
        }
    }

    pub fn cancel(&self) -> Result<(), String> {
        if let Some(cancel) = self.active.lock().map_err(|e| e.to_string())?.as_ref() {
            cancel.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    pub(super) fn begin(self: &Arc<Self>) -> Result<ActiveImport, String> {
        let mut active = self.active.lock().map_err(|e| e.to_string())?;
        if active.is_some() {
            return Err("A copy job is already running. Stop it or wait for it to finish.".into());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.card_done.store(false, Ordering::Relaxed);
        *self.ready.lock().map_err(|e| e.to_string())? = None;
        *active = Some(cancel.clone());
        Ok(ActiveImport {
            control: self.clone(),
            cancel,
        })
    }
}

/// Owned by the worker, so dropping an IPC request cannot unlock a running import.
pub(super) struct ActiveImport {
    control: Arc<ImportControl>,
    pub(super) cancel: Arc<AtomicBool>,
}

impl Drop for ActiveImport {
    fn drop(&mut self) {
        if let Ok(mut ready) = self.control.ready.lock() {
            *ready = None;
        }
        self.control.cleanup_catalogs();
        if let Ok(mut active) = self.control.active.lock() {
            self.control.card_done.store(false, Ordering::Relaxed);
            *active = None;
        }
    }
}

struct HeartbeatWorker {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl HeartbeatWorker {
    fn start(control: Arc<ImportControl>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let thread = std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                if let Ok(mut catalogs) = control.catalogs.lock() {
                    catalogs.refresh();
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for HeartbeatWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Durable recovery always precedes ejection and the card-done notification.
fn save_recovery(
    state: &AppState,
    result: &mut ImportResult,
    pending: &Path,
) -> Result<(), String> {
    super::offline::save_import_manifest(state, result)
        .map_err(|e| format!("The laptop copy is complete, but its file list couldn't be saved: {e}. Keep the card and Resume to retry."))?;
    let mut ledger = state.ledger.lock().map_err(|e| e.to_string())?;
    let mut updated = ledger.clone();
    result.card_key = result
        .card_id
        .as_deref()
        .and_then(|id| updated.card_key_for_volume(id));
    updated.record_import(result.clone());
    state.save_ledger(&updated)?;
    *ledger = updated;
    match std::fs::remove_file(pending) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!(
            "Couldn't remove the card's resume record: {e}. Keep the card and Resume to retry."
        )),
    }
}

fn eject_original(request: &ImportRequest, result: &mut ImportResult) {
    if volume_info::volume_info(&request.root).id != result.card_id {
        result
            .warnings
            .push("The card changed after copying; it was not ejected.".into());
    } else {
        match eject::eject_card(&request.root) {
            Ok(()) => result.ejected = true,
            Err(error) => result.warnings.push(error),
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
    #[serde(default)]
    offline: bool,
    #[serde(default)]
    also_backup: bool,
    #[serde(default)]
    backup_uuids: Option<Vec<String>>,
    #[serde(default)]
    resume: bool,
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

    fn resume(
        mut self,
        root: PathBuf,
        card_id: Option<&str>,
        connected: &[card_intake_core::backup::Target],
    ) -> Result<Self, String> {
        if card_id.is_none() || self.request.expected_card_id.as_deref() != card_id {
            return Err("The saved import belongs to another card.".into());
        }
        if self.eject && !self.request.verify {
            return Err("Enable file verification before choosing automatic ejection.".into());
        }
        self.request.root = root;
        for drive in &mut self.request.backup_drives {
            *drive = connected
                .iter()
                .find(|d| d.uuid == drive.uuid)
                .cloned()
                .ok_or_else(|| format!("Connect {} to resume its backup.", drive.label))?;
        }
        Ok(self)
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
    if request.resume {
        let job = PendingImport::load(&state.pending_path)?
            .ok_or("No saved import to resume. Read the card again.")?;
        return job.resume(
            request.root,
            request.expected_card_id.as_deref(),
            &state.backup_targets()?,
        );
    }
    if request.eject && !request.verify {
        return Err("Enable file verification before choosing automatic ejection.".into());
    }
    let nas = PathBuf::from(nas_root(&*state.settings.lock().map_err(|e| e.to_string())?).0);
    // To the NAS, the card is read into this computer's offline folder at full speed
    // and sent on at the same time. That needs the card's checksums; without file
    // checks, or without an offline folder, it goes straight to the NAS as before.
    let (destination, relay_to) = if request.offline {
        (state.offline_root()?, None)
    } else {
        match state.offline_root() {
            Ok(local) if request.verify => (local, Some(nas)),
            _ => (nas, None),
        }
    };
    if request.also_backup && (!request.offline || !request.verify) {
        return Err("Backup drives require Copy to this computer and file verification.".into());
    }
    let backup_drives = if request.also_backup {
        let connected = state.backup_targets()?;
        let uuids = request
            .backup_uuids
            .unwrap_or_else(|| connected.iter().map(|d| d.uuid.clone()).collect());
        if uuids.is_empty() {
            return Err("Connect a remembered backup drive first.".into());
        }
        uuids
            .iter()
            .map(|id| {
                connected
                    .iter()
                    .find(|d| &d.uuid == id)
                    .cloned()
                    .ok_or_else(|| {
                        format!("Backup drive {id} isn't connected. Connect it to resume.")
                    })
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    let card_id = request
        .expected_card_id
        .or_else(|| volume_info::volume_info(&request.root).id);
    Ok(PendingImport {
        eject: request.eject,
        request: ImportRequest {
            root: request.root,
            nas_root: destination,
            offline: request.offline || relay_to.is_some(),
            relay_to,
            backup_drives,
            expected_card_id: card_id,
            shoots: request.shoots,
            include_videos: request.include_videos,
            verify: request.verify,
            workers: 0,
            camera: request.camera,
        },
    })
}

/// A resumed card must look up existing local folders at its saved destination,
/// even if the user chose a different offline folder while it was stopped.
pub(super) fn pending_offline_folder(state: &AppState, root: &Path) -> Option<PathBuf> {
    let job = PendingImport::load(&state.pending_path).ok().flatten()?;
    (job.request.offline
        && job.request.relay_to.is_none()
        && job.request.expected_card_id.is_some()
        && job.request.expected_card_id == volume_info::volume_info(root).id)
        .then_some(job.request.nas_root)
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
    let active = state.begin_copy()?;
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
        let exiftool = app.state::<AppState>().exiftool.clone();
        let heartbeat = HeartbeatWorker::start(active.control.clone());
        let mut result = import::import_card_with_recovery(&request, exiftool.as_deref(), &active.cancel, |snapshot| {
            // Insertion may still be reading shutter counts or health. Wait for
            // those readers too, then prevent any new card reads during upload.
            let waiting = Instant::now();
            let _readers = active.control.card_reads.write().map_err(|e| e.to_string())?;
            snapshot.timings.card_seconds += waiting.elapsed().as_secs_f64();
            snapshot.average_bytes_per_sec = (snapshot.timings.card_seconds > 0.0).then(|| (snapshot.timings.card_bytes as f64 / snapshot.timings.card_seconds) as u64);
            let state = app.state::<AppState>();
            save_recovery(&state, snapshot, &state.pending_path)?;
            if eject && !active.cancel.load(Ordering::Relaxed) { eject_original(&request, snapshot); }
            *active.control.ready.lock().map_err(|e| e.to_string())? = Some(snapshot.clone());
            active.control.card_done.store(true, Ordering::Relaxed);
            Ok(())
        }, |progress| {
            if progress.phase != phase || progress.phase == "file_complete" || last.elapsed() >= Duration::from_millis(100) {
                phase = progress.phase;
                last = Instant::now();
                if on_progress.send(progress).is_err() {
                    active.cancel.store(true, Ordering::Relaxed);
                }
            }
        });
        drop(heartbeat);
        // Serialize with Lightroom registration: a catalog prepared at the same
        // instant either gets the final list here or sees the completed history.
        let mut ready = active.control.ready.lock().unwrap_or_else(|e| e.into_inner());
        let was_ready = ready.is_some();
        if ready.as_ref().is_some_and(|r| r.ejected) { result.ejected = true; }
        if let Ok(mut catalogs) = active.control.catalogs.lock() { catalogs.finish(&mut result); }
        let state = app.state::<AppState>();
        if !was_ready && result.offline && result.local_complete && let Err(error) = super::offline::save_import_manifest(&state, &result) {
            result.status = ImportStatus::Failed;
            result.error = Some(format!("The laptop copy is complete, but its file list couldn't be saved: {error}. Resume to retry. The card was not ejected."));
        }
        if result.sent.is_some() && let Some(manifest) = result.sent_manifest.take() && let Err(error) = super::offline::save_sent_manifest(&state, result.started_at, &manifest) {
            result.warnings.push(format!("Some laptop files are left, and their list couldn't be saved for Delete laptop copy: {error}"));
        }
        result.sent_manifest = None;
        result.local_files.clear();
        if !was_ready && eject && result.status == ImportStatus::Complete && !active.cancel.load(Ordering::Relaxed) {
            eject_original(&request, &mut result);
        }
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
        *ready = None;
        drop(ready);
        drop(active);
        result
    }).await.map_err(|e| format!("The import worker stopped unexpectedly: {e}. Retry to check existing copies."))
}

#[tauri::command]
pub fn cancel_import(state: State<'_, AppState>) -> Result<(), String> {
    state.imports.cancel()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn recovery_result(root: &Path) -> ImportResult {
        serde_json::from_value(serde_json::json!({
            "started_at":"2026-11-10T12:00:00", "finished_at":"2026-11-10T12:00:01",
            "root":root.join("card"), "nas_root":root.join("laptop"), "offline_root":root.join("laptop"),
            "card_id":null, "status":"complete", "total_files":1, "total_bytes":8,
            "copied_files":1,"skipped_files":0,"verified_files":1,"completed_bytes":8,
            "shoots":["2026/2026-11-10_Test"],"error":null,"warnings":[],"ejected":false,
            "offline":true,"local_complete":true,"uploading":true,"upload_to":root.join("photos")
        })).unwrap()
    }

    #[test]
    fn card_done_saves_recovery_before_removing_resume_and_keeps_one_history_entry() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::tests::local_state(dir.path());
        let mut result = recovery_result(dir.path());
        let relative = result.shoots[0].join("one.ARW");
        std::fs::create_dir_all(result.nas_root.join(&result.shoots[0])).unwrap();
        std::fs::write(result.nas_root.join(&relative), b"original").unwrap();
        result
            .local_files
            .push(card_intake_core::copies::file(&result.nas_root, relative).unwrap());
        std::fs::write(&state.pending_path, b"saved resume").unwrap();
        save_recovery(&state, &mut result, &state.pending_path).unwrap();
        assert!(!state.pending_path.exists());
        let saved = card_intake_core::ledger::Ledger::load(&state.ledger_path).unwrap();
        assert_eq!(saved.imports.len(), 1);
        assert!(
            saved.imports[0].uploading
                && saved.imports[0].local_complete
                && saved.imports[0].offline
        );
        let manifest_path = std::fs::read_dir(dir.path().join("offline-manifests"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let manifest = card_intake_core::send::Manifest::load(&manifest_path).unwrap();
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.offline_root, result.nas_root);
        result.uploading = false;
        result.offline = false;
        let mut ledger = state.ledger.lock().unwrap();
        ledger.record_import(result);
        assert_eq!(ledger.imports.len(), 1);
        assert!(!ledger.imports[0].uploading);
    }

    #[test]
    fn a_recovery_save_failure_keeps_resume_and_never_unlocks_the_card() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::tests::local_state(dir.path());
        std::fs::write(dir.path().join("offline-manifests"), b"blocked").unwrap();
        std::fs::write(&state.pending_path, b"saved resume").unwrap();
        let active = state.begin_copy().unwrap();
        let mut result = recovery_result(dir.path());
        assert!(save_recovery(&state, &mut result, &state.pending_path).is_err());
        assert!(state.pending_path.exists());
        assert!(!state.imports.card_is_done());
        assert!(state.imports.ready_import(result.started_at).is_none());
        drop(active);
    }

    #[test]
    fn card_done_blocks_new_card_readers_and_unlocks_only_ejection_while_the_job_stays_active() {
        let control = Arc::new(ImportControl::default());
        let active = control.begin().unwrap();
        assert!(control.reading_card().is_ok());
        assert!(
            control
                .eject_when_ready(|| panic!("must not eject a busy card"))
                .is_err()
        );
        *control.ready.lock().unwrap() = Some(recovery_result(Path::new("local-fixture")));
        control.card_done.store(true, Ordering::Relaxed);
        assert!(control.reading_card().is_err());
        assert!(control.eject_when_ready(|| Ok(())).is_ok());
        assert!(control.begin().is_err());
        drop(active);
        assert!(control.reading_card().is_ok());
    }

    #[test]
    fn saved_job_survives_restart_with_names_destination_and_options() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-import.json");
        assert!(PendingImport::load(&path).unwrap().is_none());
        let mut job = PendingImport {
            request: ImportRequest {
                root: dir.path().join("card"),
                nas_root: dir.path().join("offline"),
                expected_card_id: Some("original-card".into()),
                shoots: vec![ShootSelection {
                    date: chrono::NaiveDate::from_ymd_opt(2026, 11, 7).unwrap(),
                    name: "東京".into(),
                }],
                include_videos: false,
                verify: true,
                workers: 4,
                camera: None,
                offline: true,
                relay_to: Some(dir.path().join("photos")),
                backup_drives: vec![card_intake_core::backup::Target {
                    uuid: "581ae938-40dc-4cea-a35a-67e7e04fd40f".into(),
                    label: "Trip backup".into(),
                    root: dir.path().join("drive"),
                }],
            },
            eject: true,
        };
        job.save(&path).unwrap();
        let loaded = PendingImport::load(&path).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&loaded).unwrap(),
            serde_json::to_value(&job).unwrap()
        );
        let mut moved = job.request.backup_drives[0].clone();
        moved.root = dir.path().join("new-drive-letter");
        let new_card_root = dir.path().join("new-reader-letter");
        let resumed = loaded
            .clone()
            .resume(
                new_card_root.clone(),
                Some("original-card"),
                std::slice::from_ref(&moved),
            )
            .unwrap();
        assert_eq!(resumed.request.root, new_card_root);
        assert_eq!(resumed.request.nas_root, job.request.nas_root);
        assert!(resumed.request.offline);
        assert_eq!(resumed.request.backup_drives, vec![moved]);
        assert!(
            loaded
                .clone()
                .resume(dir.path().join("card"), Some("other-card"), &[])
                .is_err()
        );
        assert!(
            loaded
                .resume(dir.path().join("card"), Some("original-card"), &[])
                .unwrap_err()
                .contains("Connect Trip backup")
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
