//! Lightroom Classic catalogs for imported shoots, with the originals added in place
//! from the NAS.
//!
//! A shoot's catalog is `<catalog folder>/<year>/<shoot>/<shoot>.lrcat` on this
//! computer (Lightroom can't open catalogs on a network drive). "Open all shoots in
//! one catalog" uses the first shoot's catalog. When a shoot is finished its catalog
//! can be kept on the NAS in `<shoot>/_catalog/`; opening the shoot again copies it
//! back here first.
use super::AppState;
use card_intake_core::{import::ImportStatus, nas};
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LightroomSettings {
    pub executable: Option<PathBuf>,
    pub template: Option<PathBuf>,
    pub catalog_root: Option<PathBuf>,
}

#[derive(Serialize)]
pub struct LightroomSetup {
    settings: LightroomSettings,
    plugin_path: PathBuf,
}

fn installed_executable() -> Option<PathBuf> {
    let program_files = std::env::var_os("ProgramFiles")?;
    let path = PathBuf::from(program_files)
        .join("Adobe")
        .join("Adobe Lightroom Classic")
        .join("Lightroom.exe");
    path.is_file().then_some(path)
}

fn resolved(state: &AppState) -> Result<LightroomSettings, String> {
    let mut settings = state
        .settings
        .lock()
        .map_err(|e| e.to_string())?
        .lightroom
        .clone();
    settings.executable = settings.executable.or_else(installed_executable);
    settings.catalog_root = settings
        .catalog_root
        .or_else(|| Some(state.local_data.join("Catalogs")));
    Ok(settings)
}

fn install_plugin(local: &Path) -> Result<PathBuf, String> {
    let path = local.join("Lightroom/CardIntake.lrplugin");
    fs::create_dir_all(&path).map_err(|e| e.to_string())?;
    for (name, content) in [
        (
            "Info.lua",
            include_str!("../../../lrplugin/CardIntake.lrplugin/Info.lua"),
        ),
        (
            "Init.lua",
            include_str!("../../../lrplugin/CardIntake.lrplugin/Init.lua"),
        ),
        (
            "Shutdown.lua",
            include_str!("../../../lrplugin/CardIntake.lrplugin/Shutdown.lua"),
        ),
    ] {
        let file = path.join(name);
        if fs::read_to_string(&file).ok().as_deref() != Some(content) {
            atomic_write(&file, content.as_bytes())?;
        }
    }
    Ok(path)
}

#[tauri::command]
pub async fn get_lightroom_settings(app: AppHandle) -> Result<LightroomSetup, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        Ok(LightroomSetup {
            settings: resolved(&state)?,
            plugin_path: install_plugin(&state.local_data)?,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn save_lightroom_settings(
    settings: LightroomSettings,
    app: AppHandle,
) -> Result<LightroomSetup, String> {
    tauri::async_runtime::spawn_blocking(move || {
        if let Some(exe) = &settings.executable
            && (!exe.is_absolute() || !exe.is_file())
        {
            return Err("Choose the full path to Lightroom.exe.".into());
        }
        if let Some(template) = &settings.template {
            check_template(template)?;
        }
        if let Some(root) = &settings.catalog_root {
            check_local(root)?;
        }
        let state = app.state::<AppState>();
        {
            let mut saved = state.settings.lock().map_err(|e| e.to_string())?;
            let mut next = saved.clone();
            next.lightroom = settings;
            next.save(&state.settings_path).map_err(|e| e.to_string())?;
            *saved = next;
        }
        Ok(LightroomSetup {
            settings: resolved(&state)?,
            plugin_path: install_plugin(&state.local_data)?,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

fn check_local(path: &Path) -> Result<(), String> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("Use a full local path for Lightroom's working catalogs.".into());
    }
    let ancestor = path
        .ancestors()
        .find(|p| p.exists())
        .ok_or("The local catalog drive is unavailable.")?;
    let resolved = fs::canonicalize(ancestor).map_err(|e| e.to_string())?;
    if !matches!(nas::location(&resolved), Some(nas::Location::Local(_))) {
        return Err("Lightroom's working catalogs must be on this computer's local disk.".into());
    }
    Ok(())
}

fn check_template(path: &Path) -> Result<(), String> {
    check_local(path)?;
    if !path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("lrcat"))
        || !path.is_file()
    {
        return Err("Choose an empty Lightroom Classic .lrcat catalog as the template.".into());
    }
    for suffix in [".lock", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_os_string();
        file.push(suffix);
        if Path::new(&file).exists() {
            return Err("Close the template catalog in Lightroom before using it.".into());
        }
    }
    let mut header = [0; 16];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut header))
        .map_err(|e| e.to_string())?;
    if &header != b"SQLite format 3\0" {
        return Err("The template is not a Lightroom catalog database.".into());
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut tmp = tempfile::NamedTempFile::new_in(path.parent().ok_or("Missing folder")?)
        .map_err(|e| e.to_string())?;
    tmp.write_all(bytes).map_err(|e| e.to_string())?;
    tmp.as_file().sync_all().map_err(|e| e.to_string())?;
    tmp.persist(path).map_err(|e| e.to_string())?;
    Ok(())
}

fn copy_closed_file(from: &Path, to: &Path) -> Result<(), String> {
    let mut options = File::options();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1); // Do not copy a catalog another process can write.
    }
    let mut source = options
        .open(from)
        .map_err(|e| format!("Close the template catalog before copying it: {e}"))?;
    let before = source.metadata().map_err(|e| e.to_string())?;
    let mut target = File::create(to).map_err(|e| e.to_string())?;
    std::io::copy(&mut source, &mut target).map_err(|e| e.to_string())?;
    target.sync_all().map_err(|e| e.to_string())?;
    let after = source.metadata().map_err(|e| e.to_string())?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err(
            "The template changed while copying. Close it in Lightroom and try again.".into(),
        );
    }
    Ok(())
}

/// The local catalog for a shoot folder on the NAS (`.../2026/2026-11-06_X`).
fn catalog_for(root: &Path, folder: &Path) -> Result<PathBuf, String> {
    let name = folder
        .file_name()
        .ok_or("Invalid shoot folder.")?
        .to_string_lossy()
        .into_owned();
    let year = folder
        .parent()
        .and_then(Path::file_name)
        .map(|y| y.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(root.join(year).join(&name).join(format!("{name}.lrcat")))
}

/// Where a finished shoot's catalog is kept on the NAS.
fn nas_catalog_for(folder: &Path) -> Option<PathBuf> {
    let name = folder.file_name()?.to_string_lossy().into_owned();
    Some(folder.join("_catalog").join(format!("{name}.lrcat")))
}

/// Copies a catalog folder kept on the NAS back to this computer. Lightroom keeps
/// previews and settings in folders next to the catalog, so the whole folder goes.
fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(from).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct LightroomLaunch {
    catalog: PathBuf,
    request_id: String,
}

fn prepare(settings: &LightroomSettings, folders: &[PathBuf]) -> Result<LightroomLaunch, String> {
    if folders.is_empty() {
        return Err("This import has no shoot folders.".into());
    }
    for folder in folders {
        if !folder.is_absolute()
            || !folder.is_dir()
            || folder.to_str().is_none_or(|s| s.contains(['\r', '\n']))
        {
            return Err(format!(
                "Reconnect the photo share: {} is unavailable.",
                folder.display()
            ));
        }
    }
    let root = settings
        .catalog_root
        .as_ref()
        .ok_or("Set a local catalog folder in Lightroom setup.")?;
    check_local(root)?;
    fs::create_dir_all(root).map_err(|e| e.to_string())?;
    let catalog = catalog_for(root, &folders[0])?;
    let dir = catalog.parent().unwrap().to_path_buf();
    if !catalog.exists() {
        if dir.exists()
            && fs::read_dir(&dir)
                .map_err(|e| e.to_string())?
                .next()
                .is_some()
        {
            return Err(format!(
                "{} already has other files. Move them away before creating the shoot's catalog.",
                dir.display()
            ));
        }
        let parent = dir.parent().unwrap();
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        // Build the catalog in a temporary folder and rename it into place, so a
        // half-copied catalog never looks like a working one.
        let staging = tempfile::Builder::new()
            .prefix(".catalog-")
            .tempdir_in(parent)
            .map_err(|e| e.to_string())?;
        let file_name = catalog.file_name().unwrap();
        match nas_catalog_for(&folders[0]).filter(|p| p.is_file()) {
            // Reopening a finished shoot: bring its catalog back from the NAS.
            Some(stored) => {
                copy_dir(stored.parent().unwrap(), staging.path())?;
                if !staging.path().join(file_name).is_file() {
                    return Err("The catalog on the NAS is incomplete.".into());
                }
            }
            None => {
                let template = settings
                    .template
                    .as_ref()
                    .ok_or("Set an empty, closed .lrcat template in Lightroom setup first.")?;
                check_template(template)?;
                copy_closed_file(template, &staging.path().join(file_name))?;
                check_template(template)?;
            }
        }
        if dir.exists() {
            fs::remove_dir(&dir).map_err(|e| e.to_string())?;
        }
        fs::rename(staging.keep(), &dir)
            .map_err(|e| format!("Couldn't create the shoot's catalog: {e}"))?;
    }
    if !fs::canonicalize(&dir)
        .map_err(|e| e.to_string())?
        .starts_with(fs::canonicalize(root).map_err(|e| e.to_string())?)
    {
        return Err("The catalog folder points outside the configured catalog folder.".into());
    }
    let request_id = format!(
        "{:x}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos(),
        std::process::id()
    );
    let mut request = format!("# Card Intake v1\n# request-id {request_id}\n");
    for folder in folders {
        request.push_str(&folder.to_string_lossy());
        request.push('\n');
    }
    atomic_write(
        &catalog.with_extension("card-intake-request.txt"),
        request.as_bytes(),
    )?;
    Ok(LightroomLaunch {
        catalog,
        request_id,
    })
}

#[tauri::command]
pub async fn open_in_lightroom(
    import_id: NaiveDateTime,
    shoot: Option<usize>,
    app: AppHandle,
) -> Result<LightroomLaunch, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        if state.imports.is_active() {
            return Err("Wait for the import to finish before opening Lightroom.".into());
        }
        let record = state
            .ledger
            .lock()
            .map_err(|e| e.to_string())?
            .imports
            .iter()
            .find(|r| r.started_at == import_id && r.status == ImportStatus::Complete)
            .cloned()
            .ok_or("Choose a completed import from the history.")?;
        let selected = match shoot {
            Some(i) => vec![record.shoots.get(i).ok_or("Shoot not found")?.clone()],
            None => record.shoots.clone(),
        };
        let mut folders = Vec::new();
        for relative in selected {
            if relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            {
                return Err("Invalid shoot path in import history.".into());
            }
            folders.push(record.nas_root.join(relative));
        }
        let settings = resolved(&state)?;
        let exe = settings
            .executable
            .as_ref()
            .filter(|p| p.is_file())
            .ok_or("Set Lightroom.exe in Lightroom setup first.")?;
        install_plugin(&state.local_data)?;
        let launch = prepare(&settings, &folders)?;
        std::process::Command::new(exe)
            .arg(&launch.catalog)
            .spawn()
            .map_err(|e| format!("Couldn't open Lightroom: {e}"))?;
        Ok(launch)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Debug, Serialize)]
pub struct CatalogState {
    /// The import, by start time.
    import_id: NaiveDateTime,
    /// "local" (on this computer), "nas" (kept in the shoot's `_catalog` folder) or
    /// "none" (not created yet).
    state: &'static str,
    catalog: PathBuf,
}

/// Where each completed import's catalog is, for the history table.
#[tauri::command]
pub async fn lightroom_catalogs(app: AppHandle) -> Result<Vec<CatalogState>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let root = resolved(&state)?.catalog_root.ok_or("No catalog folder")?;
        let imports = state
            .ledger
            .lock()
            .map_err(|e| e.to_string())?
            .imports
            .clone();
        let mut states = Vec::new();
        for record in imports
            .iter()
            .filter(|r| r.status == ImportStatus::Complete)
        {
            let Some(first) = record.shoots.first() else {
                continue;
            };
            let folder = record.nas_root.join(first);
            let catalog = catalog_for(&root, &folder)?;
            let state = if catalog.is_file() {
                "local"
            } else if nas_catalog_for(&folder).is_some_and(|p| p.is_file()) {
                "nas"
            } else {
                "none"
            };
            states.push(CatalogState {
                import_id: record.started_at,
                state,
                catalog,
            });
        }
        Ok(states)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Debug, Serialize)]
pub struct LightroomStatus {
    status: String,
    added: usize,
    skipped: usize,
    failed: usize,
    detail: String,
}

#[tauri::command]
pub async fn lightroom_status(
    catalog: PathBuf,
    request_id: String,
    app: AppHandle,
) -> Result<Option<LightroomStatus>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let settings = resolved(&app.state::<AppState>())?;
        let root = settings.catalog_root.ok_or("No catalog folder")?;
        let canonical = fs::canonicalize(&catalog).map_err(|e| e.to_string())?;
        if !canonical.starts_with(fs::canonicalize(root).map_err(|e| e.to_string())?) {
            return Err("Catalog is outside the working catalog folder.".into());
        }
        let text = match fs::read_to_string(catalog.with_extension("card-intake-status.txt")) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        let mut lines = text.lines();
        if lines.next() != Some(request_id.as_str()) {
            return Ok(None);
        }
        let status = lines.next().unwrap_or("running").to_string();
        let added = lines.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let skipped = lines.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let failed = lines.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        Ok(Some(LightroomStatus {
            status,
            added,
            skipped,
            failed,
            detail: lines.collect::<Vec<_>>().join("\n"),
        }))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_the_shoots_catalog_from_the_template_and_reuses_it() {
        let dir = tempfile::tempdir().unwrap();
        let template = dir.path().join("Empty.lrcat");
        fs::write(&template, b"SQLite format 3\0test template").unwrap();
        let folder = dir.path().join("photos/2026/2026-11-07_東京");
        let second = dir.path().join("photos/2026/2026-11-08_京都");
        fs::create_dir_all(&folder).unwrap();
        fs::create_dir_all(&second).unwrap();
        let settings = LightroomSettings {
            executable: None,
            template: Some(template.clone()),
            catalog_root: Some(dir.path().join("Catalogs")),
        };
        let first = prepare(&settings, std::slice::from_ref(&folder)).unwrap();
        assert_eq!(
            first.catalog,
            dir.path()
                .join("Catalogs/2026/2026-11-07_東京/2026-11-07_東京.lrcat")
        );
        assert!(
            fs::read_to_string(first.catalog.with_extension("card-intake-request.txt"))
                .unwrap()
                .contains("東京")
        );
        // Lightroom's edits are never replaced; "open all" adds to the first shoot's catalog.
        fs::write(&first.catalog, b"user edits").unwrap();
        let all = prepare(&settings, &[folder.clone(), second.clone()]).unwrap();
        assert_eq!(first.catalog, all.catalog);
        assert_ne!(first.request_id, all.request_id);
        assert_eq!(fs::read(&all.catalog).unwrap(), b"user edits");
        let request =
            fs::read_to_string(all.catalog.with_extension("card-intake-request.txt")).unwrap();
        assert!(request.contains("京都"));
        assert_eq!(
            fs::read(template).unwrap(),
            b"SQLite format 3\0test template"
        );
    }

    #[test]
    fn reopening_a_finished_shoot_copies_its_catalog_back_from_the_nas() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("photos/2026/2026-10-31");
        let stored = folder.join("_catalog");
        fs::create_dir_all(stored.join("2026-10-31 Previews.lrdata")).unwrap();
        fs::write(stored.join("2026-10-31.lrcat"), b"finished edits").unwrap();
        fs::write(stored.join("2026-10-31 Previews.lrdata/p"), b"preview").unwrap();
        let settings = LightroomSettings {
            catalog_root: Some(dir.path().join("Catalogs")),
            ..Default::default()
        };
        let launch = prepare(&settings, &[folder]).unwrap();
        assert_eq!(fs::read(&launch.catalog).unwrap(), b"finished edits");
        let previews = launch
            .catalog
            .parent()
            .unwrap()
            .join("2026-10-31 Previews.lrdata/p");
        assert_eq!(fs::read(previews).unwrap(), b"preview");
    }

    #[test]
    fn refuses_open_templates_and_unavailable_shoots() {
        let dir = tempfile::tempdir().unwrap();
        let template = dir.path().join("Empty.lrcat");
        fs::write(&template, b"SQLite format 3\0test").unwrap();
        fs::write(dir.path().join("Empty.lrcat.lock"), b"lock").unwrap();
        assert!(check_template(&template).unwrap_err().contains("Close"));
        let settings = LightroomSettings {
            template: Some(template),
            catalog_root: Some(dir.path().join("Catalogs")),
            ..Default::default()
        };
        assert!(
            prepare(&settings, &[dir.path().join("missing")])
                .unwrap_err()
                .contains("Reconnect")
        );
        assert!(!dir.path().join("Catalogs").exists());
    }
}
