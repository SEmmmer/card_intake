// Hide the console window in Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod settings;

use card_intake_core::{
    exif, exiftool,
    grouping::{self, ShootGroup},
    health::{self, HealthOutcome},
    ledger::{CameraSighting, CardSighting, Ledger},
    nas::{self, Location, NasCheck},
    scan::{self, MediaFile, MediaKind, TimeSource},
    volume_info::{self, VolumeInfo},
    volumes::{self, CardVolume},
};
use chrono::Local;
use serde::Serialize;
use settings::Settings;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;
use tauri::{Manager, State};

struct AppState {
    ledger: Mutex<Ledger>,
    ledger_path: PathBuf,
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    exiftool: Option<PathBuf>,
    smartctl: Option<PathBuf>,
}

impl AppState {
    fn save_ledger(&self, ledger: &Ledger) -> Result<(), String> {
        ledger
            .save(&self.ledger_path)
            .map_err(|e| format!("Couldn't save the list of cards and cameras: {e}"))
    }
}

#[derive(Serialize)]
struct CardReport {
    root: PathBuf,
    volume: VolumeInfo,
    photos: usize,
    photo_bytes: u64,
    videos: usize,
    video_bytes: u64,
    times: TimeCounts,
    groups: Vec<ShootGroup>,
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
    firmware: Option<String>,
    lens: Option<String>,
    shutter_count: Option<u64>,
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
fn list_cards() -> Vec<CardVolume> {
    volumes::find_camera_cards()
}

/// Reads a card (never writes to it), then remembers the card and camera.
#[tauri::command]
async fn read_card(root: String, state: State<'_, AppState>) -> Result<CardReport, String> {
    let exiftool = state.exiftool.clone();
    let root = PathBuf::from(root);
    let report =
        tauri::async_runtime::spawn_blocking(move || build_report(&root, exiftool.as_deref()))
            .await
            .map_err(|e| e.to_string())??;

    let now = Local::now().naive_local();
    let mut ledger = state.ledger.lock().map_err(|e| e.to_string())?;
    if let Some(id) = &report.volume.id {
        ledger.record_card(
            CardSighting {
                id: id.clone(),
                capacity_bytes: report.volume.total_bytes,
                filesystem: report.volume.filesystem.clone(),
                reader: report.volume.reader.clone(),
            },
            now,
        );
    }
    if let Some(camera) = &report.camera {
        ledger.record_camera(
            CameraSighting {
                model: camera.model.clone(),
                serial: camera.serial.clone(),
                firmware: camera.firmware.clone(),
                shutter_count: camera.shutter_count,
            },
            now,
        );
    }
    state.save_ledger(&ledger)?;
    Ok(report)
}

/// Reads the card's own health log. On Windows this asks for admin rights.
#[tauri::command]
async fn read_card_health(
    root: String,
    state: State<'_, AppState>,
) -> Result<HealthOutcome, String> {
    let smartctl = state.smartctl.clone();
    let (volume, outcome) = tauri::async_runtime::spawn_blocking(move || {
        let volume = volume_info::volume_info(Path::new(&root));
        let outcome = health::read_health(&volume, smartctl.as_deref());
        (volume, outcome)
    })
    .await
    .map_err(|e| e.to_string())?;

    if let (HealthOutcome::Read { health }, Some(id)) = (&outcome, &volume.id) {
        let mut ledger = state.ledger.lock().map_err(|e| e.to_string())?;
        if ledger.record_health(id, (**health).clone(), Local::now().naive_local()) {
            state.save_ledger(&ledger)?;
        }
    }
    Ok(outcome)
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
        .map(|s| s.clone())
        .map_err(|e| e.to_string())
}

/// Saves the NAS path. An empty path goes back to the default.
#[tauri::command]
fn save_nas_root(nas_root: String, state: State<'_, AppState>) -> Result<NasStatus, String> {
    let mut settings = state.settings.lock().map_err(|e| e.to_string())?;
    let trimmed = nas_root.trim();
    settings.nas_root = (!trimmed.is_empty()).then(|| trimmed.to_string());
    settings
        .save(&state.settings_path)
        .map_err(|e| format!("Couldn't save the settings: {e}"))?;
    Ok(nas_status_for(&settings))
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
fn nas_status(state: State<'_, AppState>) -> Result<NasStatus, String> {
    let settings = state.settings.lock().map_err(|e| e.to_string())?;
    Ok(nas_status_for(&settings))
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

fn build_report(root: &Path, exiftool: Option<&Path>) -> Result<CardReport, String> {
    let started = Instant::now();
    let files = scan::scan_card(root).map_err(|e| format!("Couldn't read the card: {e}"))?;

    let photos = files.iter().filter(|f| f.kind == MediaKind::Photo);
    let videos = files.iter().filter(|f| f.kind == MediaKind::Video);
    let count = |source| files.iter().filter(|f| f.time_source == source).count();

    Ok(CardReport {
        root: root.to_path_buf(),
        volume: volume_info::volume_info(root),
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

    let (shutter_count, shutter_note) = match (newest_photo, exiftool) {
        (None, _) => (None, Some("No photos on this card to read it from.".into())),
        (Some(_), None) => (
            None,
            Some("Needs ExifTool. Run scripts/fetch-exiftool.sh, then restart the app.".into()),
        ),
        (Some(photo), Some(tool)) => match exiftool::camera_details(tool, &photo.path) {
            Ok(details) => match details.shutter_count {
                Some(count) => (Some(count), None),
                None => (
                    None,
                    Some("ExifTool can't read the shutter count for this model yet.".into()),
                ),
            },
            Err(e) => (None, Some(e.to_string())),
        },
    };

    Some(CameraSummary {
        make: basic.make,
        model,
        serial,
        firmware: basic.software.as_deref().map(firmware_version),
        lens: basic.lens,
        shutter_count,
        shutter_note,
    })
}

/// "ILCE-7RM6 v1.01" becomes "1.01".
fn firmware_version(software: &str) -> String {
    let last = software.split_whitespace().last().unwrap_or(software);
    last.trim_start_matches(['v', 'V']).to_string()
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
///     card_intake_desktop --health <card>      the card's health log
///     card_intake_desktop --check-nas <path>   the NAS folder check
fn run_cli(args: &[String]) -> Option<i32> {
    let (flag, arg) = (args.get(1)?.as_str(), args.get(2));
    let print = |value: serde_json::Result<String>| println!("{}", value.unwrap_or_default());
    match (flag, arg) {
        ("--report", Some(root)) => {
            match build_report(Path::new(root), find_exiftool(None).as_deref()) {
                Ok(report) => print(serde_json::to_string_pretty(&report)),
                Err(e) => {
                    eprintln!("{e}");
                    return Some(1);
                }
            }
        }
        ("--health", Some(root)) => {
            let volume = volume_info::volume_info(Path::new(root));
            print(serde_json::to_string_pretty(&health::read_health(
                &volume,
                find_smartctl(None).as_deref(),
            )));
        }
        ("--check-nas", Some(path)) => print(serde_json::to_string_pretty(&nas::check_nas(path))),
        ("--report" | "--health" | "--check-nas", None) => {
            eprintln!("usage: card_intake_desktop {flag} <path>");
            return Some(2);
        }
        _ => return None,
    }
    Some(0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(code) = run_cli(&args) {
        std::process::exit(code);
    }

    tauri::Builder::default()
        .setup(|app| {
            let ledger_path = app.path().app_data_dir()?.join("ledger.json");
            let settings_path = app.path().app_config_dir()?.join("settings.json");
            let resource_dir = app.path().resource_dir().ok();
            app.manage(AppState {
                ledger: Mutex::new(Ledger::load(&ledger_path)?),
                ledger_path,
                settings: Mutex::new(Settings::load(&settings_path)?),
                settings_path,
                exiftool: find_exiftool(resource_dir.as_deref()),
                smartctl: find_smartctl(resource_dir.as_deref()),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_cards,
            read_card,
            read_card_health,
            get_ledger,
            get_settings,
            save_nas_root,
            check_nas,
            nas_status
        ])
        .run(tauri::generate_context!())
        .expect("Card Intake failed to start");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_version_drops_the_model_name() {
        assert_eq!(firmware_version("ILCE-7RM6 v1.01"), "1.01");
        assert_eq!(firmware_version("2.00"), "2.00");
    }

    #[test]
    fn saved_nas_path_beats_the_default() {
        if std::env::var_os("CARD_INTAKE_NAS").is_some() {
            return;
        }
        assert_eq!(nas_root(&Settings::default()).1, "default");
        let settings = Settings {
            nas_root: Some("/Volumes/photos2".into()),
        };
        assert_eq!(
            nas_root(&settings),
            ("/Volumes/photos2".to_string(), "settings")
        );
    }
}
