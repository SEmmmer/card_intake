//! Reads the shutter count of every photo already in the photo share, so each camera's
//! history goes back to its first imported shoot. Folders read before are skipped
//! unless their photos changed, so later runs only read new shoots.

use super::{AppState, CamerasResponse, exiftool_note};
use card_intake_core::shutter::{self, FolderResult, ScanProgress, ShutterDb};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, State, ipc::Channel};

/// ExifTool runs at once. Reading over SMB waits on the network more than the CPU.
const WORKERS: usize = 4;
/// Save progress to the NAS this often, so a long first run isn't lost if it stops.
const SAVE_EVERY: Duration = Duration::from_secs(20);

#[derive(Default)]
pub struct LibraryControl {
    active: AtomicBool,
    cancel: AtomicBool,
}

impl LibraryControl {
    pub fn cancel(&self) {
        if self.active.load(Ordering::Relaxed) {
            self.cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// Marks the scan finished however the worker ends.
struct Active<'a>(&'a LibraryControl);

impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.active.store(false, Ordering::Relaxed);
    }
}

#[derive(Serialize, Clone)]
pub struct LibraryProgress {
    /// "listing" while finding photo folders, then "reading".
    phase: &'static str,
    progress: Option<ScanProgress>,
}

#[derive(Serialize)]
pub struct LibraryScanResult {
    folders_read: usize,
    folders_unchanged: usize,
    photos_read: usize,
    shutter_counts: usize,
    /// Folders that couldn't be read, with the reason. They're tried again next time.
    failed: Vec<String>,
    failed_count: usize,
    cancelled: bool,
    cameras: CamerasResponse,
}

#[tauri::command]
pub async fn scan_library(
    on_progress: Channel<LibraryProgress>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<LibraryScanResult, String> {
    let exiftool = state.exiftool.clone().ok_or_else(exiftool_note)?;
    let root = state.shared_root().ok_or(
        "Connect the photo share first: shutter counts are read from its photos and saved in it.",
    )?;
    if state.library.active.swap(true, Ordering::Relaxed) {
        return Err("Already reading the photo share.".into());
    }
    state.library.cancel.store(false, Ordering::Relaxed);
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let _active = Active(&state.library);
        let cancel = &state.library.cancel;
        let send = |phase, progress| {
            let _ = on_progress.send(LibraryProgress { phase, progress });
        };

        // Start from everything any computer has read so far.
        state.sync_shutter(&ShutterDb::default())?;
        let (known, mine, mut history) = {
            let store = state.shutter.lock().map_err(|e| e.to_string())?;
            let read = |e: std::io::Error| format!("Couldn't read the shutter counts: {e}");
            (
                store.folders().map_err(read)?,
                store.my_cameras().map_err(read)?,
                store.load().map_err(read)?,
            )
        };
        send("listing", None);
        let folders = match shutter::list_library(&root, cancel) {
            Ok(folders) => folders,
            Err(_) if cancel.load(Ordering::Relaxed) => Vec::new(),
            Err(e) => return Err(format!("Couldn't list the photo share: {e}")),
        };
        let (todo, unchanged): (Vec<_>, Vec<_>) = folders
            .into_iter()
            .partition(|f| known.get(&f.relative) != Some(&f.stamp));

        let mut result = LibraryScanResult {
            folders_read: 0,
            folders_unchanged: unchanged.len(),
            photos_read: 0,
            shutter_counts: 0,
            failed: Vec::new(),
            failed_count: 0,
            cancelled: false,
            cameras: CamerasResponse::default(),
        };
        let mut batch = ShutterDb::default();
        let mut saved = Instant::now();
        shutter::read_folders(
            &exiftool,
            todo,
            WORKERS,
            cancel,
            |progress| send("reading", Some(progress)),
            |folder_result| {
                match folder_result {
                    FolderResult::Read { folder, samples } => {
                        let (read, left_out) = history.read(&samples);
                        history.merge(&read);
                        result.folders_read += 1;
                        result.photos_read += folder.photos.len();
                        result.shutter_counts += samples.len() - left_out;
                        // A folder with photos from a camera not in My Cameras is read
                        // again next time, so they count once it's registered.
                        if read.cameras.iter().all(|c| mine.owns_camera(c)) {
                            batch.folders.insert(folder.relative, folder.stamp);
                        }
                        batch.merge(&read);
                    }
                    FolderResult::Failed { folder, error } => {
                        result.failed_count += 1;
                        if result.failed.len() < 5 {
                            result.failed.push(format!("{}: {error}", folder.relative));
                        }
                    }
                }
                if saved.elapsed() >= SAVE_EVERY && state.sync_shutter(&batch).is_ok() {
                    batch = ShutterDb::default();
                    saved = Instant::now();
                }
            },
        );
        result.cancelled = cancel.load(Ordering::Relaxed);
        result.cameras = state.sync_shutter(&batch)?;
        Ok(result)
    })
    .await
    .map_err(|e| format!("Reading the photo share stopped unexpectedly: {e}"))?
}

#[tauri::command]
pub fn cancel_library_scan(state: State<'_, AppState>) {
    state.library.cancel();
}
