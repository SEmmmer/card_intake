//! Deliver: finished JPEGs into the shoot's folder among the finished photos (e.g.
//! `MyPhoto\2026-11-06_夜景`), after the user has seen and confirmed them, then an
//! Immich rescan.

use super::{AppState, immich, lightroom};
use card_intake_core::deliver::{self, Confirmed, DeliveryResult};
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tauri::{AppHandle, Manager, State};

#[derive(Serialize)]
pub struct DeliverySetup {
    deliver_to: String,
    url: String,
    has_key: bool,
    library_id: String,
    libraries: Vec<immich::Library>,
    error: Option<String>,
}

fn setup(
    deliver_to: &str,
    immich: &immich::ImmichSettings,
    libraries: Vec<immich::Library>,
    error: Option<String>,
) -> DeliverySetup {
    DeliverySetup {
        deliver_to: deliver_to.into(),
        url: immich.url.clone(),
        has_key: !immich.api_key.is_empty(),
        library_id: immich.library_id.clone(),
        libraries,
        error,
    }
}

#[tauri::command]
pub async fn get_delivery_settings(app: AppHandle) -> Result<DeliverySetup, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let saved = state.settings.lock().map_err(|e| e.to_string())?.clone();
        let to = saved.deliver_to.unwrap_or_default();
        let s = saved.immich;
        if s.url.is_empty() {
            return Ok(setup(&to, &s, vec![], None));
        }
        match immich::Client::new(&s).and_then(|c| c.libraries()) {
            Ok(list) => Ok(setup(&to, &s, list, None)),
            Err(e) => Ok(setup(&to, &s, vec![], Some(e))),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Deserialize)]
pub struct DeliverySettingsInput {
    deliver_to: String,
    #[serde(flatten)]
    immich: immich::ImmichSettings,
}

#[tauri::command]
pub async fn save_delivery_settings(
    settings: DeliverySettingsInput,
    app: AppHandle,
) -> Result<DeliverySetup, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let to = settings.deliver_to.trim().to_string();
        if !to.is_empty() && !Path::new(&to).is_dir() {
            return Err(format!(
                "{to} isn't a folder that can be reached. Check the path and the NAS."
            ));
        }
        let mut s = settings.immich;
        if s.api_key.is_empty() {
            s.api_key = state
                .settings
                .lock()
                .map_err(|e| e.to_string())?
                .immich
                .api_key
                .clone();
        }
        let libraries = if s.url.trim().is_empty() {
            s = immich::ImmichSettings::default();
            Vec::new()
        } else {
            immich::Client::new(&s)?.libraries()?
        };
        if s.library_id.is_empty() && libraries.len() == 1 {
            s.library_id = libraries[0].id.clone();
        }
        if !s.library_id.is_empty() && !libraries.iter().any(|l| l.id == s.library_id) {
            return Err("Choose an external library from this server.".into());
        }
        let mut saved = state.settings.lock().map_err(|e| e.to_string())?;
        let mut updated = saved.clone();
        updated.deliver_to = (!to.is_empty()).then(|| to.clone());
        updated.immich = s.clone();
        updated
            .save(&state.settings_path)
            .map_err(|e| e.to_string())?;
        *saved = updated;
        Ok(setup(&to, &s, libraries, None))
    })
    .await
    .map_err(|e| e.to_string())?
}

struct Active(Arc<AtomicBool>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The shoot folders a Deliver button covers (all of a trip's), with the catalog to
/// read stars from and where the finished photos go.
struct Job {
    shoots: Vec<PathBuf>,
    catalog: Option<PathBuf>,
    photos: PathBuf,
}

fn job(state: &AppState, import_id: NaiveDateTime, shoot: usize) -> Result<Job, String> {
    let photos = state
        .settings
        .lock()
        .map_err(|e| e.to_string())?
        .deliver_to
        .clone()
        .map(PathBuf::from)
        .ok_or(
            "Set the finished photos folder (e.g. \\\\NAS\\Picture\\MyPhoto) under Deliver first.",
        )?;
    let folder = lightroom::import_folder(state, import_id, shoot)?;
    let target = lightroom::delivery_target(&lightroom::delivery_root(state)?, &folder)?;
    Ok(Job {
        shoots: target.folders,
        catalog: target.catalog,
        photos,
    })
}

fn target_for(photos: &Path, shoot: &Path) -> Result<PathBuf, String> {
    let name = shoot.file_name().ok_or("Invalid shoot folder.")?;
    Ok(photos.join(name))
}

#[derive(Serialize, Deserialize)]
pub struct Planned {
    pub shoots: Vec<DeliveryResult>,
    pub errors: Vec<String>,
}

/// What Deliver would add, with thumbnails, for the user to confirm. Writes nothing.
#[tauri::command]
pub async fn plan_delivery(
    import_id: NaiveDateTime,
    shoot: usize,
    app: AppHandle,
) -> Result<Planned, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let tool = state.exiftool.as_deref().ok_or("Deliver needs ExifTool. Install it or set CARD_INTAKE_EXIFTOOL to exiftool.exe, then reopen Card Intake.")?;
        let job = job(&state, import_id, shoot)?;
        let mut planned = Planned {
            shoots: Vec::new(),
            errors: Vec::new(),
        };
        for folder in &job.shoots {
            let planned_shoot = target_for(&job.photos, folder).and_then(|target| {
                deliver::plan(folder, tool, job.catalog.as_deref(), &target).map_err(|e| e.to_string())
            });
            match planned_shoot {
                Ok(mut r) => {
                    deliver::add_thumbnails(tool, &mut r);
                    planned.shoots.push(r);
                }
                Err(e) => planned.errors.push(format!("{}: {e}", folder.display())),
            }
        }
        Ok(planned)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The photos the user confirmed for one shoot folder.
#[derive(Deserialize)]
pub struct ConfirmedShoot {
    shoot: PathBuf,
    photos: Vec<Confirmed>,
}

#[derive(Serialize, Deserialize)]
pub struct Delivered {
    pub shoots: Vec<DeliveryResult>,
    pub rescanning: bool,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

#[tauri::command]
pub async fn deliver(
    import_id: NaiveDateTime,
    shoot: usize,
    confirmed: Vec<ConfirmedShoot>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Delivered, String> {
    let active = {
        let _gate = state.activity_gate.lock().map_err(|e| e.to_string())?;
        if state.imports.is_active() {
            return Err("Wait for the import to finish before delivering.".into());
        }
        if state.delivering.swap(true, Ordering::SeqCst) {
            return Err("A delivery is already running. Wait for it to finish.".into());
        }
        Active(state.delivering.clone())
    };
    tauri::async_runtime::spawn_blocking(move || {
        let _active = active;
        let state = app.state::<AppState>();
        let tool = state.exiftool.as_deref().ok_or("Deliver needs ExifTool. Install it or set CARD_INTAKE_EXIFTOOL to exiftool.exe, then reopen Card Intake.")?;
        let job = job(&state, import_id, shoot)?;
        if confirmed.iter().any(|c| !job.shoots.contains(&c.shoot)) {
            return Err("The photos to deliver changed since you checked them. Press Deliver again to see the new list.".into());
        }
        let mut result = Delivered {
            shoots: Vec::new(),
            rescanning: false,
            warnings: Vec::new(),
            errors: Vec::new(),
        };
        for c in confirmed.iter().filter(|c| !c.photos.is_empty()) {
            let done = target_for(&job.photos, &c.shoot).and_then(|target| {
                deliver::deliver(&c.shoot, tool, job.catalog.as_deref(), &target, &c.photos)
                    .map_err(|e| e.to_string())
            });
            match done {
                Ok(r) => result.shoots.push(r),
                Err(e) => result.errors.push(format!("{}: {e}", c.shoot.display())),
            }
        }
        let s = state.settings.lock().map_err(|e| e.to_string())?.immich.clone();
        if !s.url.is_empty()
            && !s.library_id.is_empty()
            && result.shoots.iter().any(|r| r.delivered > 0)
        {
            match immich::Client::new(&s).and_then(|c| c.scan(&s.library_id)) {
                Ok(()) => result.rescanning = true,
                Err(e) => result.warnings.push(format!(
                    "The photos are delivered, but Immich couldn't rescan: {e}"
                )),
            }
        }
        Ok(result)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
pub struct HistoryState {
    import_id: NaiveDateTime,
    shoot: usize,
    count: usize,
    last_delivered: Option<chrono::DateTime<chrono::Utc>>,
    error: Option<String>,
}

#[tauri::command]
pub async fn delivery_states(app: AppHandle) -> Result<Vec<HistoryState>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let records = state
            .ledger
            .lock()
            .map_err(|e| e.to_string())?
            .imports
            .clone();
        let root = lightroom::delivery_root(&state)?;
        let mut list = Vec::new();
        for record in records
            .iter()
            .filter(|r| r.status == card_intake_core::import::ImportStatus::Complete)
        {
            for (shoot, relative) in record.shoots.iter().enumerate() {
                let mut entry = HistoryState {
                    import_id: record.started_at,
                    shoot,
                    count: 0,
                    last_delivered: None,
                    error: None,
                };
                let folder = match lightroom::locate(&record.nas_root, relative) {
                    lightroom::Located::Found(folder) => folder,
                    lightroom::Located::Offline(_) => record.nas_root.join(relative),
                    lightroom::Located::Missing(e) => {
                        entry.error = Some(e);
                        list.push(entry);
                        continue;
                    }
                };
                match lightroom::delivery_target(&root, &folder) {
                    Ok(target) => {
                        for folder in target.folders {
                            match deliver::state(&folder) {
                                Ok(s) => {
                                    entry.count += s.count;
                                    entry.last_delivered =
                                        entry.last_delivered.max(s.last_delivered);
                                }
                                Err(e) => entry.error = Some(e.to_string()),
                            }
                        }
                    }
                    Err(e) => entry.error = Some(e),
                }
                list.push(entry);
            }
        }
        Ok(list)
    })
    .await
    .map_err(|e| e.to_string())?
}
