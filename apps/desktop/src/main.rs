// Hide the console window in Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod delivery;
mod immich;
mod imports;
mod library;
mod lightroom;
mod settings;

use card_intake_core::{
    cameras,
    card_details::{self, CardDetails},
    destinations::{self, DateDestination},
    eject, exif,
    exiftool::{self, ShutterSample},
    grouping::{self, ShootGroup},
    health::{self, CardHealth, HealthOutcome},
    ledger::{CameraSighting, CardSighting, Ledger},
    nas::{self, Location, NasCheck},
    scan::{self, MediaFile, MediaKind, TimeSource},
    shutter::{self, CameraView, ShutterDb},
    storage::{self, DriveIdentity},
    volume_info::{self, VolumeInfo},
    volumes::{self, CardVolume},
};
use chrono::Local;
use serde::Serialize;
use settings::Settings;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::{AppHandle, Manager, State};

struct AppState {
    ledger: Mutex<Ledger>,
    ledger_path: PathBuf,
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    exiftool: Option<PathBuf>,
    smartctl: Option<PathBuf>,
    imports: Arc<imports::ImportControl>,
    pending_path: PathBuf,
    local_data: PathBuf,
    /// The user's Pictures folder: Lightroom catalogs go in `Lightroom/Card Intake` there.
    pictures: Option<PathBuf>,
    /// This computer's copy of the shutter count database.
    shutter: Mutex<shutter::Store>,
    /// Readings of cameras not in My Cameras, from cards and NAS photos read since the
    /// app started. Never saved: registering a camera stores its readings.
    new_cameras: Mutex<ShutterDb>,
    library: library::LibraryControl,
    delivering: Arc<std::sync::atomic::AtomicBool>,
    /// Serialises starting an import with starting a delivery.
    activity_gate: Mutex<()>,
}

impl AppState {
    fn save_ledger(&self, ledger: &Ledger) -> Result<(), String> {
        ledger
            .save(&self.ledger_path)
            .map_err(|e| format!("Couldn't save the list of cards and cameras: {e}"))
    }

    /// The photo share, when it's connected and confirmed to be a network share.
    fn shared_root(&self) -> Option<PathBuf> {
        let settings = self.settings.lock().ok()?.clone();
        let status = nas_status_for(&settings);
        (status.connected && status.network == Some(true)).then(|| PathBuf::from(status.root))
    }

    /// Adds readings to this computer's database, then syncs it with the shared one
    /// in the photo share (when connected): only rows changed since the last sync go
    /// either way. Readings of cameras not in My Cameras are kept in memory instead,
    /// and stored once the camera is registered, here or on another computer.
    fn sync_shutter(&self, add: &ShutterDb) -> Result<CamerasResponse, String> {
        self.update_shutter(add, &[]).map(|(response, _)| response)
    }

    fn update_shutter(
        &self,
        add: &ShutterDb,
        samples: &[ShutterSample],
    ) -> Result<(CamerasResponse, usize), String> {
        let mut store = self.shutter.lock().map_err(|e| e.to_string())?;
        let read = |e: std::io::Error| format!("Couldn't read the shutter counts: {e}");
        let root = self.shared_root();
        let mut shared = SharedStatus::default();
        // Fetch other computers' history and removals before checking raw readings.
        if let Some(root) = &root {
            shared.path = Some(shutter::shared_path(root).display().to_string());
            match shutter::sync_shared(root, &mut store) {
                Ok(()) => shared.synced = true,
                Err(e) => shared.error = Some(e.to_string()),
            }
        }
        let mut new_cameras = self.new_cameras.lock().map_err(|e| e.to_string())?;
        let mut readings = std::mem::take(&mut *new_cameras);
        readings.merge(add);
        store.retain_current(&mut readings).map_err(read)?;
        let mut history = store.load().map_err(read)?;
        history.merge(&readings);
        let (mut raw, left_out) = history.read(samples);
        store.retain_current(&mut raw).map_err(read)?;
        readings.merge(&raw);
        *new_cameras = readings.split_off_new(&store.my_cameras().map_err(read)?);
        let changed = store
            .add(&readings)
            .map_err(|e| format!("Couldn't save the shutter counts: {e}"))?;
        if changed && let Some(root) = &root {
            match shutter::sync_shared(root, &mut store) {
                Ok(()) => shared.synced = true,
                Err(e) => shared.error = Some(e.to_string()),
            }
        }
        shared.folders = store.folder_count().map_err(read)?;
        Ok((
            CamerasResponse {
                cameras: store.load().map_err(read)?.views(),
                new_cameras: new_cameras.views(),
                shared,
            },
            left_out,
        ))
    }

    /// Whether a camera is in My Cameras.
    fn is_my_camera(&self, model: &str, serial: Option<&str>, internal: Option<&str>) -> bool {
        self.shutter
            .lock()
            .ok()
            .and_then(|store| store.my_cameras().ok())
            .is_some_and(|mine| mine.owns(model, serial, internal))
    }
}

#[derive(Serialize, Default)]
struct CamerasResponse {
    /// My Cameras, with their shutter count history.
    cameras: Vec<CameraView>,
    /// Cameras whose photos were read but that aren't registered. Their readings are
    /// kept only until the app closes.
    new_cameras: Vec<CameraView>,
    shared: SharedStatus,
}

/// The shared shutter count database in the photo share.
#[derive(Serialize, Default)]
struct SharedStatus {
    /// Where it is, when the share is connected.
    path: Option<String>,
    /// Read (and, if this computer had anything new, written) just now.
    synced: bool,
    error: Option<String>,
    /// Photo-share folders whose shutter counts have been read.
    folders: usize,
}

#[derive(Serialize)]
struct CardReport {
    root: PathBuf,
    volume: VolumeInfo,
    /// How the card is connected and, when the reader shows the card itself, its own
    /// serial number.
    drive: DriveIdentity,
    /// The card's health log, when it could be read without asking (NVMe cards).
    health: Option<CardHealth>,
    /// What the card and its reader reveal: reader, link speed, slot, size class,
    /// lock switch, partition and file system.
    details: CardDetails,
    /// The card's record in the ledger.
    card_key: Option<String>,
    photos: usize,
    photo_bytes: u64,
    videos: usize,
    video_bytes: u64,
    times: TimeCounts,
    groups: Vec<ShootGroup>,
    /// Per capture date: the shoot folders already on the NAS and the card's files
    /// in them. Empty when the photo share isn't connected.
    destinations: Vec<DateDestination>,
    camera: Option<CameraSummary>,
    scan_ms: u128,
}

#[derive(Serialize)]
struct TimeCounts {
    exif: usize,
    sidecar: usize,
    file_time: usize,
}

#[derive(Serialize)]
struct CameraSummary {
    make: Option<String>,
    model: String,
    serial: Option<String>,
    /// Sony's internal serial number, which identifies a body whose photos don't carry
    /// the serial number.
    internal_serial: Option<String>,
    firmware: Option<String>,
    lens: Option<String>,
    shutter_count: Option<u64>,
    electronic_count: Option<u64>,
    /// Why there's no shutter count, when there isn't one.
    shutter_note: Option<String>,
}

#[derive(Serialize)]
struct NasStatus {
    root: String,
    /// Where the path came from: "settings", "environment" or "default".
    source: &'static str,
    connected: bool,
    /// True on a network share, false on a local disk, null when unknown.
    network: Option<bool>,
    free_bytes: Option<u64>,
}

/// Camera cards that are mounted right now. The UI polls this to spot a new card.
#[tauri::command]
async fn list_cards() -> Result<Vec<CardVolume>, String> {
    tauri::async_runtime::spawn_blocking(volumes::find_camera_cards)
        .await
        .map_err(|e| e.to_string())
}

/// Safely ejects the card on screen when you ask, as "Eject the card when done" does
/// after an import. `expected_id` is the volume ID the screen shows: a card swapped in
/// since then is left alone.
#[tauri::command]
async fn eject_card(
    root: String,
    expected_id: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    if state.imports.is_active() {
        return Err("Wait for the import to finish, or stop it, before ejecting the card.".into());
    }
    let root = PathBuf::from(root);
    tauri::async_runtime::spawn_blocking(move || {
        if expected_id.is_some() && volume_info::volume_info(&root).id != expected_id {
            return Err(
                "A different card is in that drive now, so it wasn't ejected. Eject it in Explorer."
                    .to_string(),
            );
        }
        eject::eject_card(&root)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Reads a card (never writes to it), then remembers the card, and the camera when
/// it's in My Cameras. A card seen for the first time is new: it's kept while the app
/// runs, and saved once registered.
#[tauri::command]
async fn read_card(root: String, state: State<'_, AppState>) -> Result<CardReport, String> {
    let exiftool = state.exiftool.clone();
    let root = PathBuf::from(root);
    let nas = state.shared_root();
    let mut report = tauri::async_runtime::spawn_blocking(move || {
        build_report(&root, exiftool.as_deref(), nas.as_deref())
    })
    .await
    .map_err(|e| e.to_string())??;

    let now = Local::now().naive_local();
    let mut ledger = state.ledger.lock().map_err(|e| e.to_string())?;
    report.card_key = ledger.record_card(
        sighting(&report.volume, &report.drive, &report.details),
        now,
    );
    if let (Some(key), Some(health)) = (&report.card_key, &report.health) {
        ledger.record_health(key, health.clone(), now);
    }
    if let Some(camera) = &report.camera
        && state.is_my_camera(
            &camera.model,
            camera.serial.as_deref(),
            camera.internal_serial.as_deref(),
        )
    {
        ledger.record_camera(
            CameraSighting {
                model: camera.model.clone(),
                serial: camera.serial.clone(),
                firmware: camera.firmware.clone(),
                // This single-photo preview has not passed the history check.
                shutter_count: None,
            },
            now,
        );
    }
    state.save_ledger(&ledger)?;
    Ok(report)
}

#[derive(Serialize)]
struct HealthResponse {
    #[serde(flatten)]
    outcome: HealthOutcome,
    /// The card's record in the ledger. It can change with the reading: a card known
    /// by its serial number takes over the record of the card as inserted.
    card_key: Option<String>,
}

/// Reads the card's own health log. On Windows this may ask for admin rights.
#[tauri::command]
async fn read_card_health(
    root: String,
    state: State<'_, AppState>,
) -> Result<HealthResponse, String> {
    let smartctl = state.smartctl.clone();
    let (volume, outcome) = tauri::async_runtime::spawn_blocking(move || {
        let root = Path::new(&root);
        let volume = volume_info::volume_info(root);
        let outcome = health::read_health(root, &volume, smartctl.as_deref());
        (volume, outcome)
    })
    .await
    .map_err(|e| e.to_string())?;

    let mut card_key = None;
    if let HealthOutcome::Read { health } = &outcome {
        let mut ledger = state.ledger.lock().map_err(|e| e.to_string())?;
        card_key = ledger.record_reading(
            volume.id.as_deref(),
            (**health).clone(),
            Local::now().naive_local(),
        );
        if card_key.is_some() {
            state.save_ledger(&ledger)?;
        }
    }
    Ok(HealthResponse { outcome, card_key })
}

/// What the ledger needs to recognise a card.
fn sighting(volume: &VolumeInfo, drive: &DriveIdentity, details: &CardDetails) -> CardSighting {
    CardSighting {
        volume_id: volume.id.clone(),
        hardware_serial: drive.card_serial.clone(),
        // With the card itself on the bus, the product name is the card's model.
        model: drive.card_serial.as_ref().and(drive.product.clone()),
        bus: drive.bus.clone(),
        sd_class: details.sd_class.clone(),
        capacity_bytes: volume.total_bytes,
        filesystem: volume.filesystem.clone(),
        reader: volume.reader.clone(),
    }
}

/// Reads the shutter count of every photo on the card, adds them to the shutter
/// count database and returns every camera's history.
#[tauri::command]
async fn read_card_shutter(
    root: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<CamerasResponse, String> {
    let Some(exiftool) = state.exiftool.clone() else {
        return Err(exiftool_note());
    };
    let samples = tauri::async_runtime::spawn_blocking(move || {
        let files = scan::scan_card(Path::new(&root))
            .map_err(|e| format!("Couldn't read the card: {e}"))?;
        let photos = exiftool::one_per_shot(
            files
                .into_iter()
                .filter(|f| f.kind == MediaKind::Photo)
                .map(|f| f.path),
        );
        shutter_samples_parallel(&exiftool, &photos)
    })
    .await
    .map_err(|e| e.to_string())??;
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<AppState>()
            .update_shutter(&ShutterDb::default(), &samples)
            .map(|(response, _)| response)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Adds a camera to My Cameras, with what was already read from its photos. The
/// registration is shared through the photo share like the counts.
#[tauri::command]
async fn register_camera(
    model: String,
    serial: Option<String>,
    internal_serial: Option<String>,
    app: AppHandle,
) -> Result<CamerasResponse, String> {
    let model = model.trim().to_string();
    if model.is_empty() {
        return Err("A camera needs its model to be registered.".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        state
            .shutter
            .lock()
            .map_err(|e| e.to_string())?
            .register(
                &model,
                serial.as_deref().filter(|s| !s.trim().is_empty()),
                internal_serial.as_deref().filter(|s| !s.trim().is_empty()),
            )
            .map_err(|e| format!("Couldn't register the camera: {e}"))?;
        // Moves the readings kept for it into the database.
        state.sync_shutter(&ShutterDb::default())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Removes a camera and its history everywhere this database syncs.
#[tauri::command]
async fn remove_camera(
    model: String,
    serial: Option<String>,
    internal_serial: Option<String>,
    app: AppHandle,
) -> Result<CamerasResponse, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        {
            let mut store = state.shutter.lock().map_err(|e| e.to_string())?;
            store
                .remove(&model, serial.as_deref(), internal_serial.as_deref())
                .map_err(|e| format!("Couldn't remove the camera: {e}"))?;
            let mut camera = shutter::MyCameras::default();
            camera.add(&model, serial.as_deref(), internal_serial.as_deref());
            state
                .new_cameras
                .lock()
                .map_err(|e| e.to_string())?
                .cameras
                .retain(|c| !camera.owns_camera(c));
        }
        // The ledger's old camera previews aren't part of the shared history.
        let mut ledger = state.ledger.lock().map_err(|e| e.to_string())?;
        ledger
            .cameras
            .retain(|c| c.model != model || c.serial != serial);
        state.save_ledger(&ledger)?;
        drop(ledger);
        state.sync_shutter(&ShutterDb::default())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Adds a card to My Cards, with what this app learned about it since it was inserted.
#[tauri::command]
fn register_card(card_key: String, state: State<'_, AppState>) -> Result<Ledger, String> {
    let mut ledger = state.ledger.lock().map_err(|e| e.to_string())?;
    if !ledger.register_card(&card_key) {
        return Err("This card isn't known any more. Insert it again to register it.".into());
    }
    state.save_ledger(&ledger)?;
    Ok(ledger.clone())
}

/// Removes this computer's card record. Its import results stay in the ledger.
#[tauri::command]
fn remove_card(card_key: String, state: State<'_, AppState>) -> Result<Ledger, String> {
    let mut ledger = state.ledger.lock().map_err(|e| e.to_string())?;
    let mut updated = ledger.clone();
    updated.remove_card(&card_key);
    state.save_ledger(&updated)?;
    *ledger = updated;
    Ok(ledger.clone())
}

/// Every camera's shutter count history, merged with the shared database when the
/// photo share is connected.
#[tauri::command]
async fn get_cameras(app: AppHandle) -> Result<CamerasResponse, String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<AppState>().sync_shutter(&ShutterDb::default())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Splits the photos across a few ExifTool runs at once; one run reads a card's
/// photos about four times slower.
fn shutter_samples_parallel(
    exiftool: &Path,
    photos: &[PathBuf],
) -> Result<Vec<ShutterSample>, String> {
    let per_run = photos.len().div_ceil(4).max(50);
    std::thread::scope(|scope| {
        let runs: Vec<_> = photos
            .chunks(per_run)
            .map(|chunk| scope.spawn(move || exiftool::shutter_samples(exiftool, chunk)))
            .collect();
        let mut samples = Vec::new();
        for run in runs {
            samples.extend(
                run.join()
                    .map_err(|_| "ExifTool stopped unexpectedly.".to_string())?
                    .map_err(|e| e.to_string())?,
            );
        }
        Ok(samples)
    })
}

fn exiftool_note() -> String {
    if cfg!(windows) {
        "Needs ExifTool for Windows. Put exiftool.exe on PATH or set CARD_INTAKE_EXIFTOOL, then restart the app.".into()
    } else {
        "Needs ExifTool. Run scripts/fetch-exiftool.sh, then restart the app.".into()
    }
}

#[tauri::command]
fn get_ledger(state: State<'_, AppState>) -> Result<Ledger, String> {
    state
        .ledger
        .lock()
        .map(|l| l.clone())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> Result<Settings, String> {
    state
        .settings
        .lock()
        .map(|s| {
            let mut safe = s.clone();
            safe.immich.api_key.clear();
            safe
        })
        .map_err(|e| e.to_string())
}

/// Remembers which cameras and cards are folded in My Cameras and My Cards.
#[tauri::command]
fn save_folded(folded: Vec<String>, state: State<'_, AppState>) -> Result<(), String> {
    let mut settings = state.settings.lock().map_err(|e| e.to_string())?;
    let mut updated = settings.clone();
    updated.folded = folded
        .into_iter()
        .filter(|k| !k.is_empty() && k.len() <= 200)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .take(1000)
        .collect();
    updated
        .save(&state.settings_path)
        .map_err(|e| format!("Couldn't save the settings: {e}"))?;
    *settings = updated;
    Ok(())
}

/// Saves the NAS path. An empty path goes back to the default.
#[tauri::command]
async fn save_nas_root(nas_root: String, state: State<'_, AppState>) -> Result<NasStatus, String> {
    let saved = {
        let mut settings = state.settings.lock().map_err(|e| e.to_string())?;
        let trimmed = nas_root.trim();
        let mut updated = settings.clone();
        updated.nas_root = (!trimmed.is_empty()).then(|| trimmed.to_string());
        updated
            .save(&state.settings_path)
            .map_err(|e| format!("Couldn't save the settings: {e}"))?;
        *settings = updated.clone();
        updated
    };
    tauri::async_runtime::spawn_blocking(move || nas_status_for(&saved))
        .await
        .map_err(|e| e.to_string())
}

/// Checks a NAS path without saving it: exists, network share, test file round trip,
/// free space, and whether it looks like the photo share.
#[tauri::command]
async fn check_nas(path: String) -> Result<NasCheck, String> {
    tauri::async_runtime::spawn_blocking(move || nas::check_nas(&path))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn nas_status(state: State<'_, AppState>) -> Result<NasStatus, String> {
    let settings = state.settings.lock().map_err(|e| e.to_string())?.clone();
    tauri::async_runtime::spawn_blocking(move || nas_status_for(&settings))
        .await
        .map_err(|e| e.to_string())
}

fn nas_status_for(settings: &Settings) -> NasStatus {
    let (root, source) = nas_root(settings);
    let path = Path::new(&root);
    let connected = path.is_dir();
    NasStatus {
        connected,
        network: connected
            .then(|| nas::location(path))
            .flatten()
            .map(|l| matches!(l, Location::Network(_))),
        free_bytes: connected.then(|| nas::space(path)).flatten().map(|s| s.0),
        root,
        source,
    }
}

/// `CARD_INTAKE_NAS` wins, then the saved setting, then a per-system default.
fn nas_root(settings: &Settings) -> (String, &'static str) {
    if let Ok(path) = std::env::var("CARD_INTAKE_NAS") {
        return (path, "environment");
    }
    if let Some(path) = &settings.nas_root {
        return (path.clone(), "settings");
    }
    let default = if cfg!(windows) {
        r"\\NAS\photos"
    } else if cfg!(target_os = "macos") {
        "/Volumes/photos"
    } else {
        "/mnt/photos"
    };
    (default.to_string(), "default")
}

/// Everything the app reads from a card. With the photo share, it also finds each
/// date's existing folders and the files already in them.
fn build_report(
    root: &Path,
    exiftool: Option<&Path>,
    nas: Option<&Path>,
) -> Result<CardReport, String> {
    let started = Instant::now();
    let files = scan::scan_card(root).map_err(|e| format!("Couldn't read the card: {e}"))?;

    let photos = files.iter().filter(|f| f.kind == MediaKind::Photo);
    let videos = files.iter().filter(|f| f.kind == MediaKind::Video);
    let count = |source| files.iter().filter(|f| f.time_source == source).count();

    let drive = storage::identify(root);
    // Read on insertion when it needs no asking: on the NVMe bus, or through the
    // reader's bridge when the app already runs as admin (the card then answered).
    let direct = drive.bus.as_deref() == Some("NVMe")
        || (drive.bridge.is_some() && drive.card_serial.is_some());
    let health = direct
        .then(|| storage::read_nvme_health(root).ok())
        .flatten();
    Ok(CardReport {
        root: root.to_path_buf(),
        volume: volume_info::volume_info(root),
        drive,
        health,
        details: card_details::card_details(root),
        card_key: None,
        photos: photos.clone().count(),
        photo_bytes: photos.map(|f| f.size).sum(),
        videos: videos.clone().count(),
        video_bytes: videos.map(|f| f.size).sum(),
        times: TimeCounts {
            exif: count(TimeSource::Exif),
            sidecar: count(TimeSource::Sidecar),
            file_time: count(TimeSource::FileTime),
        },
        groups: grouping::group_by_date(&files),
        destinations: nas
            .map(|n| destinations::find(n, &files))
            .unwrap_or_default(),
        camera: camera_summary(&files, exiftool),
        scan_ms: started.elapsed().as_millis(),
    })
}

/// The camera that shot the newest photo. Falls back to a clip's sidecar when the
/// card holds only video.
fn camera_summary(files: &[MediaFile], exiftool: Option<&Path>) -> Option<CameraSummary> {
    let newest_photo = files.iter().rev().find(|f| f.kind == MediaKind::Photo);
    let basic = newest_photo
        .and_then(|f| exif::read_basic(&f.path).ok())
        .unwrap_or_default();
    let clip = files.iter().rev().find(|f| f.kind == MediaKind::Video);

    let model = basic
        .model
        .clone()
        .or_else(|| clip.and_then(|c| scan::sidecar_model(&c.path)))?;
    let serial = basic
        .serial
        .clone()
        .or_else(|| clip.and_then(|c| scan::sidecar_serial(&c.path)));

    let mut electronic_count = None;
    let mut internal_serial = None;
    let (shutter_count, shutter_note) = match (newest_photo, exiftool) {
        (None, _) => (None, Some("No photos on this card to read it from.".into())),
        (Some(_), None) => (None, Some(exiftool_note())),
        (Some(photo), Some(tool)) => match exiftool::camera_details(tool, &photo.path) {
            Ok(details) => {
                electronic_count = details.electronic_count;
                internal_serial = details.internal_serial;
                match details.shutter_count {
                    Some(count) => (Some(count), None),
                    None => (
                        None,
                        Some(no_shutter_note(&model).unwrap_or_else(|| {
                            "ExifTool can't read the shutter count for this model yet.".into()
                        })),
                    ),
                }
            }
            Err(e) => (None, Some(e.to_string())),
        },
    };

    Some(CameraSummary {
        make: basic.make,
        model,
        serial,
        internal_serial,
        firmware: basic.software.as_deref().map(exiftool::firmware_version),
        lens: basic.lens,
        shutter_count,
        electronic_count,
        shutter_note,
    })
}

/// Why a camera has no shutter count, when the reason is the body itself: a Nikon Z 9
/// counts its electronic shutter, and its photos here didn't carry that count.
fn no_shutter_note(model: &str) -> Option<String> {
    let info = cameras::model_info(model)?;
    (!info.has_mechanical_shutter).then(|| {
        format!(
            "The {} has no mechanical shutter: its count is the electronic shutter's, \
             and ExifTool found none in this card's photos.",
            info.name
        )
    })
}

/// A helper program shipped with the app (`resources/<folder>/`), installed in a usual
/// place, or on the PATH. `env_var` overrides all of them.
fn find_tool(
    env_var: &str,
    folder: &str,
    name: &str,
    usual_places: &[&str],
    resource_dir: Option<&Path>,
) -> Option<PathBuf> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os(env_var) {
        candidates.push(PathBuf::from(path));
    }
    if let Some(dir) = resource_dir {
        candidates.push(dir.join("resources").join(folder).join(&file));
    }
    if cfg!(debug_assertions) {
        candidates.push(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("resources")
                .join(folder)
                .join(&file),
        );
    }
    candidates.extend(usual_places.iter().map(PathBuf::from));
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join(&file)));
    }
    candidates.into_iter().find(|p| p.is_file())
}

fn find_exiftool(resource_dir: Option<&Path>) -> Option<PathBuf> {
    find_tool(
        "CARD_INTAKE_EXIFTOOL",
        "exiftool",
        "exiftool",
        &[],
        resource_dir,
    )
}

fn find_smartctl(resource_dir: Option<&Path>) -> Option<PathBuf> {
    let usual: &[&str] = if cfg!(windows) {
        &[r"C:\Program Files\smartmontools\bin\smartctl.exe"]
    } else {
        &["/usr/local/sbin/smartctl", "/opt/homebrew/sbin/smartctl"]
    };
    find_tool(
        "CARD_INTAKE_SMARTCTL",
        "smartctl",
        "smartctl",
        usual,
        resource_dir,
    )
}

/// Command-line checks that print JSON and exit, without opening a window or saving anything:
///
///     card_intake_desktop --report <card>      what the app reads from a card
///     card_intake_desktop --health <card>      the card's health log (may ask for admin rights)
///     card_intake_desktop --check-nas <path>   the NAS folder check
///     card_intake_desktop --shutter <folder>   shutter counts of every photo in a folder
fn run_cli(args: &[String]) -> Option<i32> {
    let (flag, arg) = (args.get(1)?.as_str(), args.get(2));
    let print = |value: serde_json::Result<String>| println!("{}", value.unwrap_or_default());
    match (flag, arg) {
        ("--report", Some(root)) => {
            match build_report(Path::new(root), find_exiftool(None).as_deref(), None) {
                Ok(report) => print(serde_json::to_string_pretty(&report)),
                Err(e) => {
                    eprintln!("{e}");
                    return Some(1);
                }
            }
        }
        ("--health", Some(root)) => {
            let root = Path::new(root);
            let volume = volume_info::volume_info(root);
            print(serde_json::to_string_pretty(&serde_json::json!({
                "drive": storage::identify(root),
                "details": card_details::card_details(root),
                "health": health::read_health(root, &volume, find_smartctl(None).as_deref()),
            })));
        }
        ("--shutter", Some(folder)) => {
            let Some(exiftool) = find_exiftool(None) else {
                eprintln!("{}", exiftool_note());
                return Some(1);
            };
            let photos = exiftool::one_per_shot(
                walkdir::WalkDir::new(folder)
                    .into_iter()
                    .filter_map(Result::ok)
                    .filter(|e| {
                        e.file_type().is_file() && scan::kind_of(e.path()) == Some(MediaKind::Photo)
                    })
                    .map(|e| e.into_path()),
            );
            let started = Instant::now();
            match shutter_samples_parallel(&exiftool, &photos) {
                Ok(samples) => {
                    let mut db = ShutterDb::default();
                    let left_out = db.add_read(&samples);
                    eprintln!(
                        "{} photos, {} with a shutter count, read in {} ms",
                        photos.len(),
                        samples.len(),
                        started.elapsed().as_millis()
                    );
                    if left_out > 0 {
                        eprintln!(
                            "{left_out} counts left out: they don't fit the camera's other photos"
                        );
                    }
                    print(serde_json::to_string_pretty(&db.views()));
                }
                Err(e) => {
                    eprintln!("{e}");
                    return Some(1);
                }
            }
        }
        ("--deliver-plan", Some(folder)) => {
            let Some(tool) = find_exiftool(None) else {
                eprintln!("Deliver needs ExifTool. Set CARD_INTAKE_EXIFTOOL to its full path.");
                return Some(1);
            };
            // Saved settings are only read. The CLI exits before Tauri setup can open any
            // settings, history or shutter database for writing.
            #[cfg(windows)]
            let config = std::env::var_os("APPDATA").map(PathBuf::from);
            #[cfg(not(windows))]
            let config = std::env::var_os("HOME")
                .map(|p| PathBuf::from(p).join("Library/Application Support"));
            let saved = config
                .and_then(|c| {
                    Settings::load(&c.join("io.github.semmmer.cardintake/settings.json")).ok()
                })
                .unwrap_or_default();
            let Some(photos) = args.get(3).cloned().or(saved.deliver_to.clone()) else {
                eprintln!(
                    "usage: card_intake_desktop --deliver-plan <shoot> <finished photos folder> [catalog]"
                );
                return Some(2);
            };
            let shoot = Path::new(folder);
            let target = Path::new(&photos).join(shoot.file_name().unwrap_or_default());
            let catalog = args.get(4).map(PathBuf::from).or_else(|| {
                let root = saved.lightroom.catalog_root.clone().or_else(|| {
                    #[cfg(windows)]
                    let home = std::env::var_os("USERPROFILE");
                    #[cfg(not(windows))]
                    let home = std::env::var_os("HOME");
                    home.map(|p| PathBuf::from(p).join("Pictures/Lightroom/Card Intake"))
                })?;
                lightroom::delivery_target(&root, shoot).ok()?.catalog
            });
            match card_intake_core::deliver::plan(shoot, &tool, catalog.as_deref(), &target) {
                Ok(result) => print(serde_json::to_string_pretty(&result)),
                Err(e) => {
                    eprintln!("{e}");
                    return Some(1);
                }
            }
        }
        ("--check-nas", Some(path)) => print(serde_json::to_string_pretty(&nas::check_nas(path))),
        ("--report" | "--health" | "--check-nas" | "--shutter" | "--deliver-plan", None) => {
            eprintln!("usage: card_intake_desktop {flag} <path>");
            return Some(2);
        }
        _ => return None,
    }
    Some(0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // The copy of the app Windows started with admin rights to read a card.
    if let Some(code) = health::serve_admin_request(&args) {
        std::process::exit(code);
    }
    if let Some(code) = run_cli(&args) {
        std::process::exit(code);
    }

    tauri::Builder::default()
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let ledger_path = data_dir.join("ledger.json");
            let mut shutter = shutter::Store::open(&data_dir.join(shutter::SHARED_DB))
                .unwrap_or_else(|e| {
                    eprintln!("{e} Keeping shutter counts in memory for this session.");
                    shutter::Store::in_memory()
                });
            // Counts kept by an older build of this app.
            if let Err(e) = shutter.import_json(&data_dir.join(shutter::SHARED_FILE)) {
                eprintln!("Couldn't bring in the old shutter counts: {e}");
            }
            let settings_path = app.path().app_config_dir()?.join("settings.json");
            let resource_dir = app.path().resource_dir().ok();
            app.manage(AppState {
                ledger: Mutex::new(Ledger::load(&ledger_path)?),
                ledger_path,
                settings: Mutex::new(Settings::load(&settings_path)?),
                settings_path,
                exiftool: find_exiftool(resource_dir.as_deref()),
                smartctl: find_smartctl(resource_dir.as_deref()),
                imports: Arc::new(imports::ImportControl::default()),
                pending_path: data_dir.join("pending-import.json"),
                local_data: app.path().app_local_data_dir()?,
                pictures: app.path().picture_dir().ok(),
                shutter: Mutex::new(shutter),
                new_cameras: Mutex::new(ShutterDb::default()),
                library: library::LibraryControl::default(),
                delivering: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                activity_gate: Mutex::new(()),
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<AppState>();
                state.library.cancel();
                if state.imports.is_active() {
                    // Let the worker remove its partial file and save history.
                    // The completed/stopped screen remains visible; a second close exits.
                    let _ = state.imports.cancel();
                    api.prevent_close();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_cards,
            eject_card,
            read_card,
            read_card_health,
            get_ledger,
            get_settings,
            save_nas_root,
            check_nas,
            nas_status,
            read_card_shutter,
            get_cameras,
            register_camera,
            register_card,
            remove_camera,
            remove_card,
            library::scan_library,
            library::cancel_library_scan,
            imports::start_import,
            imports::get_pending_import,
            lightroom::get_lightroom_settings,
            lightroom::save_lightroom_settings,
            lightroom::open_in_lightroom,
            lightroom::finish_catalog,
            lightroom::lightroom_catalogs,
            delivery::get_delivery_settings,
            delivery::save_delivery_settings,
            delivery::plan_delivery,
            delivery::deliver,
            save_folded,
            delivery::delivery_states,
            imports::cancel_import
        ])
        .run(tauri::generate_context!())
        .expect("Card Intake failed to start");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn a_cards_report_lists_the_folders_its_dates_already_have_on_the_nas() {
        let dir = tempfile::tempdir().unwrap();
        let (card, nas) = (dir.path().join("card"), dir.path().join("nas"));
        let sidecar = br#"<NonRealTimeMeta><CreationDate value="2026-11-06T18:00:00+09:00"/></NonRealTimeMeta>"#;
        for clip in ["C0001", "C0002"] {
            write(
                &card.join(format!("M4ROOT/CLIP/{clip}.MP4")),
                format!("{clip} bytes").as_bytes(),
            );
            write(&card.join(format!("M4ROOT/CLIP/{clip}M01.XML")), sidecar);
        }
        // An earlier import of the same day already holds the first clip.
        write(
            &nas.join("2026/2026-11-06_夜景/video/C0001.MP4"),
            b"C0001 bytes",
        );

        let report = build_report(&card, None, Some(&nas)).unwrap();
        assert_eq!(report.videos, 2);
        assert_eq!(report.destinations.len(), 1);
        let day = &report.destinations[0];
        assert_eq!(day.date.to_string(), "2026-11-06");
        assert_eq!(day.suggested_title.as_deref(), Some("夜景"));
        assert_eq!((day.videos_on_nas, day.video_bytes_on_nas), (1, 11));
        assert_eq!(day.folders[0].name, "2026-11-06_夜景");

        // Without a connected share there is nothing to look up.
        assert!(
            build_report(&card, None, None)
                .unwrap()
                .destinations
                .is_empty()
        );
    }

    #[test]
    fn a_nikon_z_9_card_says_its_count_is_electronic_when_none_is_found() {
        let note = no_shutter_note("NIKON Z 9").unwrap();
        assert!(note.contains("Z 9"), "{note}");
        assert!(note.contains("electronic"), "{note}");
        // Other bodies keep the generic explanation when ExifTool finds nothing.
        assert_eq!(no_shutter_note("ILCE-7RM6"), None);
        assert_eq!(no_shutter_note("NIKON Z 8"), None);
    }

    #[test]
    fn saved_nas_path_beats_the_default() {
        if std::env::var_os("CARD_INTAKE_NAS").is_some() {
            return;
        }
        assert_eq!(nas_root(&Settings::default()).1, "default");
        let settings = Settings {
            nas_root: Some("/Volumes/photos2".into()),
            ..Default::default()
        };
        assert_eq!(
            nas_root(&settings),
            ("/Volumes/photos2".to_string(), "settings")
        );
    }
}
