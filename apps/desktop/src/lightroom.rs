//! Lightroom Classic catalogs for imported shoots, with the originals added in place
//! from the NAS.
//!
//! Each shoot gets its own catalog on this computer (Lightroom can't open catalogs on a
//! network drive): `<catalog folder>/<year>/<shoot>/<shoot>.lrcat`, a copy of the empty
//! template. A trip catalog holds several shoots that had no catalog yet:
//! `<catalog folder>/<year>/_trips/<first day>_<name>/`, and each of its shoots gets
//! `_catalog/trip.txt` on the NAS so opening any of them opens the trip.
//!
//! Next to each catalog, `<name>.card-intake.txt` lists its shoot folders. The Card
//! Intake plug-in, which this app installs in Lightroom's Modules folder, reads it when
//! the catalog opens and adds new raw photos and DxO DNGs from those folders.
//! Upload heartbeats let that pass continue as files arrive. An early catalog's
//! local identity is saved with its import even when NAS folder resolution changes.
//!
//! **Finish** copies a closed catalog with its Smart Previews and mask data to the NAS
//! (the shoot's `_catalog` folder, or `<year>/_trips/<trip>/`), checks the copy, and
//! removes the local one. Opening the shoot again copies it back.

use super::AppState;
use card_intake_core::{import::ImportStatus, naming, nas};
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LightroomSettings {
    pub executable: Option<PathBuf>,
    pub template: Option<PathBuf>,
    pub catalog_root: Option<PathBuf>,
    /// A folder of develop presets and profiles (`.xmp`) installed for every catalog.
    pub presets: Option<PathBuf>,
}

#[derive(Serialize)]
pub struct LightroomSetup {
    settings: LightroomSettings,
    /// Where the plug-in is installed for Lightroom to load.
    plugin: Option<PathBuf>,
    /// Presets and profiles from `settings.presets` installed for Lightroom.
    presets_installed: usize,
    presets_error: Option<String>,
}

/// The plug-in, built into the app and written to Lightroom's Modules folder.
const PLUGIN: &[(&str, &str)] = &[
    (
        "Info.lua",
        include_str!("../../../lrplugin/CardIntake.lrplugin/Info.lua"),
    ),
    (
        "Init.lua",
        include_str!("../../../lrplugin/CardIntake.lrplugin/Init.lua"),
    ),
    (
        "Intake.lua",
        include_str!("../../../lrplugin/CardIntake.lrplugin/Intake.lua"),
    ),
    (
        "AddNewFiles.lua",
        include_str!("../../../lrplugin/CardIntake.lrplugin/AddNewFiles.lua"),
    ),
    (
        "PluginInfo.lua",
        include_str!("../../../lrplugin/CardIntake.lrplugin/PluginInfo.lua"),
    ),
    (
        "StartupService.lua",
        include_str!("../../../lrplugin/CardIntake.lrplugin/StartupService.lua"),
    ),
    (
        "PluginShutdown.lua",
        include_str!("../../../lrplugin/CardIntake.lrplugin/PluginShutdown.lua"),
    ),
    (
        "UrlHandler.lua",
        include_str!("../../../lrplugin/CardIntake.lrplugin/UrlHandler.lua"),
    ),
];

/// The plug-in's own address (`URLHandler` in its `Info.lua`). Opening it asks the
/// catalog Lightroom has open to add what's new.
const PLUGIN_URL: &str = "lightroom://io.github.semmmer.cardintake/add";

/// A trip's folder next to the year's shoots, on the NAS and on this computer.
const TRIPS: &str = "_trips";
/// In a shoot's `_catalog` folder: the trip catalog the shoot belongs to.
const TRIP_NOTE: &str = "trip.txt";

/// Catalogs opened for the one running upload. Owned by ImportControl so stop,
/// errors, worker unwind and normal window exit all remove the heartbeat.
#[derive(Default)]
pub(super) struct UploadCatalogs {
    entries: Vec<(PathBuf, Vec<PathBuf>)>,
    refreshed: Option<Instant>,
    errors: Vec<String>,
}

fn uploading_path(catalog: &Path) -> PathBuf {
    catalog.with_extension("card-intake.uploading")
}

fn write_heartbeat(catalog: &Path) -> Result<(), String> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    atomic_write(&uploading_path(catalog), format!("{seconds}\n").as_bytes())
}

impl UploadCatalogs {
    fn register(&mut self, catalog: PathBuf, folders: Vec<PathBuf>) -> Result<(), String> {
        write_heartbeat(&catalog)?;
        if let Some(entry) = self.entries.iter_mut().find(|e| e.0 == catalog) {
            entry.1 = folders;
        } else {
            self.entries.push((catalog, folders));
        }
        Ok(())
    }
    pub fn refresh(&mut self) {
        if self
            .refreshed
            .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
        {
            return;
        }
        self.refreshed = Some(Instant::now());
        for (catalog, _) in &self.entries {
            if let Err(error) = write_heartbeat(catalog)
                && !self.errors.contains(&error)
            {
                self.errors.push(error);
            }
        }
    }
    pub fn finish(&mut self, result: &mut card_intake_core::import::ImportResult) {
        for (path, anticipated) in &self.entries {
            if !result.lightroom_catalogs.contains(path) {
                result.lightroom_catalogs.push(path.clone());
            }
            let catalog = Catalog {
                name: path.file_stem().unwrap().to_string_lossy().into_owned(),
                dir: path.parent().unwrap().to_path_buf(),
                nas_dir: PathBuf::new(),
                trip: false,
            };
            let nas = if result.offline && result.sent.is_none() {
                result.upload_to.as_ref().unwrap_or(&result.nas_root)
            } else {
                &result.nas_root
            };
            let days: Vec<_> = anticipated
                .iter()
                .filter_map(|p| p.file_name())
                .map(|n| n.to_string_lossy().chars().take(10).collect::<String>())
                .collect();
            let final_folders: Vec<_> = result
                .shoots
                .iter()
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| days.iter().any(|d| n.to_string_lossy().starts_with(d)))
                })
                .map(|s| nas.join(s))
                .collect();
            // Preserve unrelated folders already listed in an existing trip.
            if let Ok(text) = fs::read_to_string(catalog.handoff()) {
                let mut folders: Vec<_> = text
                    .lines()
                    .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
                    .map(PathBuf::from)
                    .filter(|p| !anticipated.contains(p))
                    .collect();
                folders.extend(final_folders.iter().cloned());
                if let Err(error) = replace_handoff(&catalog, &folders) {
                    self.errors.push(error);
                }
            }
            if catalog
                .dir
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|n| n == TRIPS)
            {
                let year = catalog
                    .dir
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::file_name)
                    .unwrap()
                    .to_string_lossy();
                let note = format!("{year}/{TRIPS}/{}", catalog.name);
                for folder in final_folders.into_iter().filter(|p| p.is_dir()) {
                    let local_root = catalog.dir.ancestors().nth(3).unwrap();
                    let assigned = Catalog::for_shoot(local_root, &folder);
                    match assigned {
                        Ok(assigned)
                            if (assigned.trip && assigned.name != catalog.name)
                                || (!assigned.trip
                                    && (assigned.lrcat().exists()
                                        || assigned.nas_lrcat().exists())) =>
                        {
                            self.errors.push(format!("{} already has a catalog; its catalog association was kept. The new trip's folder list still includes its photos.", folder.display()));
                            continue;
                        }
                        Err(error) => {
                            self.errors.push(error);
                            continue;
                        }
                        _ => {}
                    }
                    let outcome = fs::create_dir_all(folder.join("_catalog"))
                        .map_err(|e| e.to_string())
                        .and_then(|()| {
                            atomic_write(&folder.join("_catalog").join(TRIP_NOTE), note.as_bytes())
                        });
                    if let Err(error) = outcome {
                        self.errors.push(error);
                    }
                }
            }
        }
        self.clear();
        result.warnings.extend(
            self.errors
                .drain(..)
                .map(|e| format!("Lightroom upload handoff: {e}")),
        );
    }
    pub fn clear(&mut self) {
        for (path, _) in self.entries.drain(..) {
            if let Err(error) = fs::remove_file(uploading_path(&path))
                && error.kind() != io::ErrorKind::NotFound
            {
                self.errors.push(error.to_string());
            }
        }
    }
}
impl Drop for UploadCatalogs {
    fn drop(&mut self) {
        self.clear();
    }
}

fn installed_executable() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let path = PathBuf::from("/Applications/Adobe Lightroom Classic/Adobe Lightroom Classic.app");
    #[cfg(not(target_os = "macos"))]
    let path = PathBuf::from(std::env::var_os("ProgramFiles")?)
        .join("Adobe")
        .join("Adobe Lightroom Classic")
        .join("Lightroom.exe");
    path.exists().then_some(path)
}

/// Adobe's per-user folder: Lightroom's Modules and Camera Raw's presets are in it.
fn adobe_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    return std::env::var_os("APPDATA").map(|p| PathBuf::from(p).join("Adobe"));
    #[cfg(target_os = "macos")]
    return std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join("Library/Application Support/Adobe"));
    #[allow(unreachable_code)]
    None
}

fn resolved(state: &AppState) -> Result<LightroomSettings, String> {
    let mut settings = state
        .settings
        .lock()
        .map_err(|e| e.to_string())?
        .lightroom
        .clone();
    settings.executable = settings.executable.or_else(installed_executable);
    settings.catalog_root = settings.catalog_root.or_else(|| {
        Some(match &state.pictures {
            Some(pictures) => pictures.join("Lightroom").join("Card Intake"),
            None => state.local_data.join("Catalogs"),
        })
    });
    Ok(settings)
}

/// Writes the plug-in into Lightroom's Modules folder, which Lightroom loads by itself
/// at start-up. Also removes the copy older builds asked you to add in Plug-in Manager,
/// so Lightroom doesn't load the plug-in twice.
fn install_plugin(adobe: &Path, local_data: &Path) -> Result<PathBuf, String> {
    let dir = adobe
        .join("Lightroom")
        .join("Modules")
        .join("CardIntake.lrplugin");
    fs::create_dir_all(&dir).map_err(|e| format!("Couldn't install the Lightroom plug-in: {e}"))?;
    for (name, content) in PLUGIN {
        let file = dir.join(name);
        if fs::read_to_string(&file).ok().as_deref() != Some(content) {
            atomic_write(&file, content.as_bytes())?;
        }
    }
    let _ = fs::remove_file(dir.join("Shutdown.lua"));
    let _ = fs::remove_dir_all(local_data.join("Lightroom").join("CardIntake.lrplugin"));
    Ok(dir)
}

/// Copies every `.xmp` preset and profile under `from` into Camera Raw's settings
/// folder, where every Lightroom catalog finds them. Changed files are updated; nothing
/// is deleted.
fn install_presets(from: &Path, adobe: &Path) -> Result<usize, String> {
    let target = adobe.join("CameraRaw").join("Settings").join("Card Intake");
    let mut count = 0;
    for entry in walkdir::WalkDir::new(from)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if !entry.file_type().is_file()
            || !path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("xmp"))
        {
            continue;
        }
        let dest = target.join(path.strip_prefix(from).map_err(|e| e.to_string())?);
        let bytes = fs::read(path).map_err(|e| format!("Couldn't read {}: {e}", path.display()))?;
        if fs::read(&dest).ok().as_deref() != Some(bytes.as_slice()) {
            fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
            atomic_write(&dest, &bytes)?;
        }
        count += 1;
    }
    if count == 0 {
        return Err(format!(
            "{} has no .xmp presets or profiles.",
            from.display()
        ));
    }
    Ok(count)
}

/// Installs the plug-in and presets, and reports the setup.
fn setup(state: &AppState) -> Result<LightroomSetup, String> {
    let settings = resolved(state)?;
    let adobe = adobe_dir();
    let plugin = match &adobe {
        Some(adobe) => Some(install_plugin(adobe, &state.local_data)?),
        None => None,
    };
    // A presets folder that's gone missing is reported, but doesn't stop Lightroom
    // opening: the presets installed before are still there.
    let (presets_installed, presets_error) = match (&settings.presets, &adobe) {
        (Some(from), Some(adobe)) => match install_presets(from, adobe) {
            Ok(count) => (count, None),
            Err(e) => (0, Some(e)),
        },
        _ => (0, None),
    };
    Ok(LightroomSetup {
        settings,
        plugin,
        presets_installed,
        presets_error,
    })
}

#[tauri::command]
pub async fn get_lightroom_settings(app: AppHandle) -> Result<LightroomSetup, String> {
    tauri::async_runtime::spawn_blocking(move || setup(&app.state::<AppState>()))
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
            && (!exe.is_absolute() || !exe.exists())
        {
            return Err("Choose the full path to Lightroom Classic.".into());
        }
        if let Some(template) = &settings.template {
            check_template(template)?;
        }
        if let Some(root) = &settings.catalog_root {
            check_local(root)?;
        }
        if let Some(presets) = &settings.presets
            && !presets.is_dir()
        {
            return Err(format!("{} isn't a folder.", presets.display()));
        }
        let state = app.state::<AppState>();
        {
            let mut saved = state.settings.lock().map_err(|e| e.to_string())?;
            let mut next = saved.clone();
            next.lightroom = settings;
            next.save(&state.settings_path).map_err(|e| e.to_string())?;
            *saved = next;
        }
        setup(&state)
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

/// Lightroom keeps these next to a catalog it has open (or didn't close cleanly).
fn open_markers(lrcat: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    [".lock", "-wal", "-shm"].into_iter().map(|suffix| {
        let mut file = lrcat.as_os_str().to_os_string();
        file.push(suffix);
        PathBuf::from(file)
    })
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
    if open_markers(path).any(|p| p.exists()) {
        return Err("Close the template catalog in Lightroom before using it.".into());
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

/// Copies the template into `to`, refusing a template another process could be writing.
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
    io::copy(&mut source, &mut target).map_err(|e| e.to_string())?;
    target.sync_all().map_err(|e| e.to_string())?;
    let after = source.metadata().map_err(|e| e.to_string())?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err(
            "The template changed while copying. Close it in Lightroom and try again.".into(),
        );
    }
    Ok(())
}

fn hash_file(path: &Path) -> io::Result<u64> {
    let mut file = File::open(path)?;
    let mut hasher = xxhash_rust::xxh64::Xxh64::new(0);
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hasher.digest());
        }
        hasher.update(&buf[..n]);
    }
}

/// Copies a file or folder and reads every copied file back to check it.
fn copy_verified(from: &Path, to: &Path) -> Result<(), String> {
    let fail = |e: io::Error| format!("Couldn't copy {}: {e}", from.display());
    if from.is_dir() {
        fs::create_dir_all(to).map_err(fail)?;
        for entry in fs::read_dir(from).map_err(fail)? {
            let entry = entry.map_err(fail)?;
            copy_verified(&entry.path(), &to.join(entry.file_name()))?;
        }
        return Ok(());
    }
    {
        let mut source = File::open(from).map_err(fail)?;
        let mut target = File::create(to).map_err(fail)?;
        io::copy(&mut source, &mut target).map_err(fail)?;
        target.sync_all().map_err(fail)?;
    }
    if hash_file(from).map_err(fail)? != hash_file(to).map_err(fail)? {
        return Err(format!(
            "The copy of {} doesn't match. Check the connection and try again.",
            from.display()
        ));
    }
    Ok(())
}

fn remove_any(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn name_of(path: Option<&Path>) -> Result<String, String> {
    path.and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| "Invalid shoot folder.".to_string())
}

/// A catalog Card Intake manages.
#[derive(Debug, Clone, PartialEq)]
struct Catalog {
    /// `2026-11-06_Yosemite`, or a trip's `2026-11-03_Japan`.
    name: String,
    /// The local folder holding `<name>.lrcat`.
    dir: PathBuf,
    /// Where Finish keeps it on the NAS.
    nas_dir: PathBuf,
    trip: bool,
}

impl Catalog {
    /// A shoot folder's own catalog (`<share>/<year>/<shoot>`).
    fn shoot(root: &Path, folder: &Path) -> Result<Catalog, String> {
        let name = name_of(Some(folder))?;
        let year = name_of(folder.parent())?;
        Ok(Catalog {
            dir: root.join(year).join(&name),
            nas_dir: folder.join("_catalog"),
            name,
            trip: false,
        })
    }

    /// A trip catalog, kept in `<share>/<year>/_trips/<name>` when finished.
    fn trip(root: &Path, share: &Path, year: &str, name: &str) -> Catalog {
        Catalog {
            dir: root.join(year).join(TRIPS).join(name),
            nas_dir: share.join(year).join(TRIPS).join(name),
            name: name.to_string(),
            trip: true,
        }
    }

    /// The catalog a shoot opens in: its trip's, when it belongs to one.
    fn for_shoot(root: &Path, folder: &Path) -> Result<Catalog, String> {
        let own = Self::shoot(root, folder)?;
        let remote_trip = trip_of(folder)?;
        // A trip can be opened before the first photo has made its NAS folder.
        if remote_trip.is_none()
            && let Ok(note) = fs::read_to_string(own.dir.join(".card-intake-trip.txt"))
        {
            let parts: Vec<_> = Path::new(note.trim())
                .components()
                .map(|c| c.as_os_str().to_owned())
                .collect();
            if parts.len() == 3
                && parts[1] == TRIPS
                && parts.iter().all(|p| {
                    Path::new(p)
                        .components()
                        .all(|c| matches!(c, Component::Normal(_)))
                })
            {
                let share = folder
                    .parent()
                    .and_then(Path::parent)
                    .ok_or("Invalid shoot folder")?;
                return Ok(Self::trip(
                    root,
                    share,
                    &parts[0].to_string_lossy(),
                    &parts[2].to_string_lossy(),
                ));
            }
        }
        match remote_trip {
            Some((year, name)) => {
                let share = folder
                    .parent()
                    .and_then(Path::parent)
                    .ok_or("Invalid shoot folder.")?;
                Ok(Catalog::trip(root, share, &year, &name))
            }
            None => Self::shoot_from_archive(root, folder),
        }
    }

    fn shoot_from_archive(root: &Path, folder: &Path) -> Result<Catalog, String> {
        let mut own = Self::shoot(root, folder)?;
        if own.lrcat().exists() || own.nas_lrcat().exists() {
            return Ok(own);
        }
        let day: String = own.name.chars().take(10).collect();
        let entries = match fs::read_dir(&own.nas_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(own),
            Err(e) => return Err(e.to_string()),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            if entry.file_type().map_err(|e| e.to_string())?.is_file()
                && path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("lrcat"))
                && let Some(name) = path
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .filter(|n| n.starts_with(&day))
            {
                names.push(name.to_string());
            }
        }
        match names.as_slice() {
            [] => {}
            [name] => {
                own.dir = root.join(name_of(folder.parent())?).join(name);
                own.name.clone_from(name);
            }
            _ => {
                return Err(format!(
                    "{} has several catalogs. Choose the import that created the catalog.",
                    folder.display()
                ));
            }
        }
        Ok(own)
    }

    /// An early catalog keeps its local identity when originals are found in an
    /// earlier NAS folder. Archive it beside the actual files, with the same name.
    fn for_record(
        root: &Path,
        folder: &Path,
        record: &card_intake_core::import::ImportResult,
    ) -> Result<Catalog, String> {
        let day = folder
            .file_name()
            .ok_or("Missing shoot folder")?
            .to_string_lossy()
            .chars()
            .take(10)
            .collect::<String>();
        for path in &record.lightroom_catalogs {
            let name = name_of(path.file_stem().map(Path::new))?;
            let dir = path.parent().ok_or("Missing catalog folder")?.to_path_buf();
            let trip = dir
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|n| n == TRIPS);
            if !trip && !name.starts_with(&day) {
                continue;
            }
            check_local(path)?;
            let nas = if record.offline && record.sent.is_none() {
                record.upload_to.as_ref().unwrap_or(&record.nas_root)
            } else {
                &record.nas_root
            };
            let nas_dir = if trip {
                nas.join(name.get(..4).ok_or("Invalid catalog year")?)
                    .join(TRIPS)
                    .join(&name)
            } else {
                let candidates: Vec<_> = record
                    .shoots
                    .iter()
                    .filter(|s| {
                        s.file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with(&day))
                    })
                    .collect();
                let primary = candidates
                    .iter()
                    .find(|s| s.file_name().is_some_and(|n| n == name.as_str()))
                    .copied()
                    .or_else(|| candidates.first().copied())
                    .ok_or("No shoot for this catalog")?;
                nas.join(primary).join("_catalog")
            };
            return Ok(Catalog {
                name,
                dir,
                nas_dir,
                trip,
            });
        }
        Self::for_shoot(root, folder)
    }

    fn lrcat(&self) -> PathBuf {
        self.dir.join(format!("{}.lrcat", self.name))
    }

    fn nas_lrcat(&self) -> PathBuf {
        self.nas_dir.join(format!("{}.lrcat", self.name))
    }

    /// The shoot folders the plug-in adds photos from.
    fn handoff(&self) -> PathBuf {
        self.dir.join(format!("{}.card-intake.txt", self.name))
    }

    /// What holds the edits, copied by Finish and Reopen: the catalog, its AI mask
    /// data, its Smart Previews and its list of shoot folders.
    fn kept(&self) -> [String; 4] {
        let n = &self.name;
        [
            format!("{n}.lrcat"),
            format!("{n}.lrcat-data"),
            format!("{n} Smart Previews.lrdata"),
            format!("{n}.card-intake.txt"),
        ]
    }

    /// Previews Lightroom rebuilds, removed with the local copy.
    fn rebuilt(&self) -> [String; 3] {
        let n = &self.name;
        [
            format!("{n} Previews.lrdata"),
            format!("{n} Helper.lrdata"),
            format!("{n} Sync.lrdata"),
        ]
    }

    fn is_open(&self) -> bool {
        open_markers(&self.lrcat()).any(|p| p.exists())
    }

    /// "open" in Lightroom, "local" (on this computer), "nas" (finished) or "none".
    fn state(&self) -> &'static str {
        if self.lrcat().is_file() {
            if self.is_open() { "open" } else { "local" }
        } else if self.nas_lrcat().is_file() {
            "nas"
        } else {
            "none"
        }
    }
}

/// The trip a shoot belongs to, as (year, name), from its `_catalog/trip.txt`.
fn trip_of(folder: &Path) -> Result<Option<(String, String)>, String> {
    let note = folder.join("_catalog").join(TRIP_NOTE);
    let text = match fs::read_to_string(&note) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Couldn't read {}: {e}", note.display())),
    };
    let parts: Vec<&str> = text.trim().split('/').collect();
    match parts[..] {
        [year, TRIPS, name]
            if !year.is_empty()
                && !name.is_empty()
                && [year, name].iter().all(|p| {
                    Path::new(p)
                        .components()
                        .all(|c| matches!(c, Component::Normal(_)))
                }) =>
        {
            Ok(Some((year.to_string(), name.to_string())))
        }
        _ => Err(format!("{} doesn't name a trip.", note.display())),
    }
}

#[derive(Debug, Serialize)]
pub struct LightroomLaunch {
    catalog: PathBuf,
    name: String,
    /// Lightroom already has it open, so the plug-in's start-up won't run again:
    /// it is asked through its URL to add what's new.
    already_open: bool,
}

/// Creates the catalog (from the template, or from its finished copy on the NAS) when
/// it isn't on this computer, and records its shoot folders for the plug-in.
fn prepare(
    settings: &LightroomSettings,
    catalog: &Catalog,
    folders: &[PathBuf],
) -> Result<LightroomLaunch, String> {
    let lrcat = catalog.lrcat();
    let already_open = catalog.is_open();
    if !lrcat.exists() {
        fs::create_dir_all(&catalog.dir).map_err(|e| e.to_string())?;
        if catalog.nas_lrcat().is_file() {
            bring_back(catalog)?;
        } else {
            let template = settings
                .template
                .as_ref()
                .ok_or("Set an empty, closed .lrcat template in Lightroom setup first.")?;
            check_template(template)?;
            // Copy under a name Lightroom won't open, then rename: a half-copied
            // template never looks like a catalog.
            let tmp = catalog.dir.join(format!(".{}.lrcat.part", catalog.name));
            copy_closed_file(template, &tmp)?;
            check_template(template)?;
            fs::rename(&tmp, &lrcat).map_err(|e| format!("Couldn't create the catalog: {e}"))?;
        }
    }
    write_handoff(catalog, folders)?;
    Ok(LightroomLaunch {
        catalog: lrcat,
        name: catalog.name.clone(),
        already_open,
    })
}

/// Adds `folders` to the catalog's list of shoot folders.
fn write_handoff(catalog: &Catalog, folders: &[PathBuf]) -> Result<(), String> {
    let path = catalog.handoff();
    let mut listed: Vec<String> = fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    for folder in folders {
        let folder = folder.to_string_lossy().into_owned();
        if !listed.contains(&folder) {
            listed.push(folder);
        }
    }
    replace_handoff(
        catalog,
        &listed.into_iter().map(PathBuf::from).collect::<Vec<_>>(),
    )
}

fn replace_handoff(catalog: &Catalog, folders: &[PathBuf]) -> Result<(), String> {
    let path = catalog.handoff();
    let mut text = String::from(
        "# Card Intake: when this catalog opens, Lightroom adds new raw photos and DxO DNGs from these shoot folders.\n",
    );
    for folder in folders {
        text.push_str(&folder.to_string_lossy());
        text.push('\n');
    }
    if fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
        return Ok(());
    }
    atomic_write(&path, text.as_bytes())
}

/// Copies a finished catalog back from the NAS.
fn bring_back(catalog: &Catalog) -> Result<(), String> {
    for item in catalog.kept() {
        let from = catalog.nas_dir.join(&item);
        if !from.exists() {
            continue;
        }
        let tmp = catalog.dir.join(format!(".{item}.part"));
        remove_any(&tmp).map_err(|e| e.to_string())?;
        copy_verified(&from, &tmp)?;
        let to = catalog.dir.join(&item);
        remove_any(&to).map_err(|e| e.to_string())?;
        fs::rename(&tmp, &to).map_err(|e| format!("Couldn't reopen the catalog: {e}"))?;
    }
    Ok(())
}

/// Opens a trip catalog for these shoots, named `<first day>_<name>`. Only shoots
/// without a catalog of their own can join.
fn prepare_trip(
    settings: &LightroomSettings,
    root: &Path,
    folders: &[PathBuf],
    name: &str,
) -> Result<LightroomLaunch, String> {
    let name = naming::clean_shoot_name(name);
    if name.is_empty() {
        return Err("Name the trip, e.g. Japan.".into());
    }
    let first = folders
        .iter()
        .min_by_key(|f| f.file_name())
        .ok_or("No shoots")?;
    let first_name = name_of(Some(first))?;
    let day = first_name
        .get(..10)
        .filter(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").is_ok())
        .ok_or("The shoot folders aren't named by date.")?;
    let year = name_of(first.parent())?;
    let share = first
        .parent()
        .and_then(Path::parent)
        .ok_or("Invalid shoot folder.")?;
    let trip = Catalog::trip(root, share, &year, &format!("{day}_{name}"));
    let note = format!("{year}/{TRIPS}/{}", trip.name);
    for folder in folders {
        let shoot = name_of(Some(folder))?;
        let assigned = Catalog::for_shoot(root, folder)?;
        if assigned.trip && assigned.lrcat() != trip.lrcat() {
            return Err(format!("{shoot} is already in the trip {}.", assigned.name));
        }
        match trip_of(folder)? {
            Some((y, n)) if y == year && n == trip.name => {}
            Some((_, other)) => {
                return Err(format!("{shoot} is already in the trip {other}."));
            }
            None => {
                let own = Catalog::shoot(root, folder)?;
                if own.lrcat().exists() || own.nas_lrcat().exists() {
                    return Err(format!(
                        "{shoot} already has its own catalog. Only shoots without one can join a trip."
                    ));
                }
            }
        }
    }
    let launch = prepare(settings, &trip, folders)?;
    for folder in folders {
        let own = Catalog::shoot(root, folder)?;
        fs::create_dir_all(&own.dir).map_err(|e| e.to_string())?;
        atomic_write(&own.dir.join(".card-intake-trip.txt"), note.as_bytes())?;
        // A listed folder is created on the NAS only when a photo lands there.
        if !folder.is_dir() {
            continue;
        }
        let dir = folder.join("_catalog");
        if fs::read_to_string(dir.join(TRIP_NOTE)).ok().as_deref() != Some(note.as_str()) {
            fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            atomic_write(&dir.join(TRIP_NOTE), note.as_bytes())?;
        }
    }
    Ok(launch)
}

#[derive(Debug, Serialize)]
pub struct Finished {
    name: String,
    /// The catalog's folder on the NAS.
    stored: PathBuf,
}

/// Copies a closed catalog to the NAS, checks the copy, and removes the local one.
fn finish(catalog: &Catalog) -> Result<Finished, String> {
    let marker = uploading_path(&catalog.lrcat());
    let heartbeat = fs::read_to_string(&marker)
        .ok()
        .and_then(|s| s.lines().next()?.parse::<u64>().ok());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    if heartbeat.is_some_and(|t| now.saturating_sub(t) <= 45) {
        return Err("Wait for this catalog's NAS upload to finish before Finish.".into());
    }
    // A crashed app's expired marker is neither live state nor a catalog asset.
    if marker.exists() {
        fs::remove_file(marker).map_err(|e| e.to_string())?;
    }
    if !catalog.lrcat().is_file() {
        return Err(format!("{} isn't on this computer.", catalog.name));
    }
    if catalog.is_open() {
        return Err(format!(
            "Close {} in Lightroom first (open another catalog or quit Lightroom), then Finish.",
            catalog.name
        ));
    }
    let items: Vec<String> = catalog
        .kept()
        .into_iter()
        .filter(|i| catalog.dir.join(i).exists())
        .collect();
    let id = std::process::id();
    let staging = catalog.nas_dir.join(format!(".finishing-{id}"));
    let replaced = catalog.nas_dir.join(format!(".replaced-{id}"));
    let nas = |e: io::Error| format!("Couldn't write to the NAS: {e}");
    remove_any(&staging).map_err(nas)?;
    remove_any(&replaced).map_err(nas)?;
    fs::create_dir_all(&staging).map_err(nas)?;
    for item in &items {
        copy_verified(&catalog.dir.join(item), &staging.join(item))?;
    }
    // Swap the new copy in, replacing the copy from an earlier Finish.
    for item in &items {
        let target = catalog.nas_dir.join(item);
        if target.exists() {
            fs::create_dir_all(&replaced).map_err(nas)?;
            fs::rename(&target, replaced.join(item)).map_err(nas)?;
        }
        fs::rename(staging.join(item), &target).map_err(nas)?;
    }
    remove_any(&replaced).map_err(nas)?;
    remove_any(&staging).map_err(nas)?;
    if !catalog.nas_lrcat().is_file() {
        return Err("The catalog didn't arrive on the NAS. The local copy was kept.".into());
    }
    // The NAS has it now. Lightroom's backups and anything else stay on this computer.
    for item in catalog.kept().iter().chain(catalog.rebuilt().iter()) {
        remove_any(&catalog.dir.join(item))
            .map_err(|e| format!("Stored on the NAS, but couldn't remove the local copy: {e}"))?;
    }
    let _ = fs::remove_dir(&catalog.dir);
    Ok(Finished {
        name: catalog.name.clone(),
        stored: catalog.nas_dir.clone(),
    })
}

fn launch_lightroom(exe: &Path, lrcat: &Path) -> Result<(), String> {
    let mut command = if cfg!(target_os = "macos") && exe.extension().is_some_and(|e| e == "app") {
        let mut open = std::process::Command::new("open");
        open.arg("-a").arg(exe);
        open
    } else {
        std::process::Command::new(exe)
    };
    command
        .arg(lrcat)
        .spawn()
        .map_err(|e| format!("Couldn't open Lightroom: {e}"))?;
    Ok(())
}

/// A catalog that's already open doesn't start up again, so the plug-in is asked
/// through its URL instead. Best effort: Library > Plug-in Extras does the same.
fn ask_plugin_to_add() {
    #[cfg(windows)]
    let mut command = {
        let mut command = card_intake_core::process::command(Path::new("rundll32.exe"));
        command.arg("url.dll,FileProtocolHandler").arg(PLUGIN_URL);
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(PLUGIN_URL);
        command
    };
    let _ = command.spawn();
}

/// Where an import's shoot folder is on the NAS now.
#[derive(Debug, PartialEq)]
pub(super) enum Located {
    Found(PathBuf),
    /// The photo share itself can't be reached.
    Offline(String),
    /// The share is there but the folder isn't, and it can't be found again.
    Missing(String),
}

impl Located {
    pub(super) fn into_result(self) -> Result<PathBuf, String> {
        match self {
            Located::Found(folder) => Ok(folder),
            Located::Offline(e) | Located::Missing(e) => Err(e),
        }
    }
}

/// Finds a shoot folder from import history. A folder renamed on the NAS (or one an
/// older version recorded but never created, because its files were already in another
/// folder for the date) is found again when it's the only folder for that date.
pub(super) fn locate(nas_root: &Path, relative: &Path) -> Located {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Located::Missing("Invalid shoot path in import history.".into());
    }
    let usable = |p: &Path| p.to_str().is_some_and(|s| !s.contains(['\r', '\n']));
    let folder = nas_root.join(relative);
    if folder.is_dir() {
        return if usable(&folder) {
            Located::Found(folder)
        } else {
            Located::Missing(format!("{} can't be used in a catalog.", folder.display()))
        };
    }
    if !nas_root.is_dir() {
        return Located::Offline(format!(
            "Reconnect the photo share: {} is unavailable.",
            nas_root.display()
        ));
    }
    let gone = format!(
        "{} isn't on the NAS any more. It may have been renamed or deleted.",
        relative.display()
    );
    let name = folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some(day) = name
        .get(..10)
        .filter(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").is_ok())
    else {
        return Located::Missing(gone);
    };
    let Some(parent) = folder.parent() else {
        return Located::Missing(gone);
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Located::Missing(gone);
    };
    let mut same_day: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n == day
                || n.strip_prefix(day)
                    .is_some_and(|rest| rest.starts_with('_'))
        })
        .map(|e| e.path())
        .collect();
    same_day.sort();
    match &same_day[..] {
        [only] if usable(only) => Located::Found(only.clone()),
        [] | [_] => Located::Missing(gone),
        several => Located::Missing(format!(
            "{} isn't on the NAS any more, and {} has several folders for {day} ({}), so Card Intake can't tell which one it is now.",
            relative.display(),
            parent.display(),
            several
                .iter()
                .filter_map(|p| p.file_name())
                .map(|n| n.to_string_lossy())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Each shoot folder of a completed import, in history order.
fn record_folders(state: &AppState, import_id: NaiveDateTime) -> Result<Vec<Located>, String> {
    if let Some(record) = state.imports.ready_import(import_id) {
        let nas = record.upload_to.ok_or("Missing NAS destination")?;
        return Ok(record
            .shoots
            .iter()
            .map(|s| Located::Found(nas.join(s)))
            .collect());
    }
    let record = state
        .ledger
        .lock()
        .map_err(|e| e.to_string())?
        .imports
        .iter()
        .find(|r| {
            r.started_at == import_id
                && r.status == ImportStatus::Complete
                && (!r.offline || r.sent.is_some())
        })
        .cloned()
        .ok_or("Choose a completed import from the history.")?;
    if record.shoots.is_empty() {
        return Err("This import has no shoot folders.".into());
    }
    Ok(record
        .shoots
        .iter()
        .map(|relative| locate(&record.nas_root, relative))
        .collect())
}

/// One shoot folder of a completed import. Other shoots of the import don't matter.
pub(super) fn import_folder(
    state: &AppState,
    import_id: NaiveDateTime,
    shoot: usize,
) -> Result<PathBuf, String> {
    record_folders(state, import_id)?
        .into_iter()
        .nth(shoot)
        .ok_or("Shoot not found")?
        .into_result()
}

/// All of a completed import's shoot folders, for a new trip holding them all.
fn import_folders(state: &AppState, import_id: NaiveDateTime) -> Result<Vec<PathBuf>, String> {
    record_folders(state, import_id)?
        .into_iter()
        .map(Located::into_result)
        .collect()
}

fn import_record(
    state: &AppState,
    id: NaiveDateTime,
) -> Result<card_intake_core::import::ImportResult, String> {
    if let Some(record) = state.imports.ready_import(id) {
        return Ok(record);
    }
    state
        .ledger
        .lock()
        .map_err(|e| e.to_string())?
        .imports
        .iter()
        .find(|r| r.started_at == id)
        .cloned()
        .ok_or("Import no longer in history".into())
}

fn remember_catalog(
    state: &AppState,
    record: &mut card_intake_core::import::ImportResult,
    path: &Path,
) -> Result<(), String> {
    let mut next = record.clone();
    if !next.lightroom_catalogs.iter().any(|p| p == path) {
        next.lightroom_catalogs.push(path.to_path_buf());
    }
    let mut ledger = state.ledger.lock().map_err(|e| e.to_string())?;
    let mut updated = ledger.clone();
    updated.record_import(next.clone());
    state.save_ledger(&updated)?;
    *ledger = updated;
    *record = next;
    Ok(())
}

/// A recovered import can finish with Send after its original process died.
/// Reconcile the saved early catalog too, preserving a trip's unrelated shoots.
pub(super) fn refresh_saved_handoffs(
    record: &mut card_intake_core::import::ImportResult,
    original_shoots: &[PathBuf],
) {
    let original_nas = record.upload_to.as_ref().unwrap_or(&record.nas_root);
    let mut uploads = UploadCatalogs::default();
    for path in &record.lightroom_catalogs {
        let name = path.file_stem().unwrap_or_default().to_string_lossy();
        let trip = path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .is_some_and(|n| n == TRIPS);
        let day: String = name.chars().take(10).collect();
        let anticipated = original_shoots
            .iter()
            .filter(|s| {
                trip || s
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(&day))
            })
            .map(|s| original_nas.join(s))
            .collect();
        uploads.entries.push((path.clone(), anticipated));
    }
    uploads.finish(record);
}

fn catalog_root(settings: &LightroomSettings) -> Result<PathBuf, String> {
    let root = settings
        .catalog_root
        .clone()
        .ok_or("Set a local catalog folder in Lightroom setup.")?;
    check_local(&root)?;
    Ok(root)
}

/// Opens a shoot's catalog (its trip's, if it's in one), or a new trip catalog for all
/// of the import's shoots when `trip` names one.
#[tauri::command]
pub async fn open_in_lightroom(
    import_id: NaiveDateTime,
    shoot: Option<usize>,
    trip: Option<String>,
    app: AppHandle,
) -> Result<LightroomLaunch, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let during_upload = state.imports.ready_import(import_id).is_some();
        if state.imports.is_active() && !during_upload && !state.imports.card_is_done() {
            return Err(
                "Wait for the card to be read and checked before opening Lightroom.".into(),
            );
        }
        // A trip needs every shoot of the import; one shoot needs only its own folder.
        let folders = match trip {
            Some(_) => import_folders(&state, import_id)?,
            None => vec![import_folder(&state, import_id, shoot.unwrap_or(0))?],
        };
        let record = import_record(&state, import_id)?;
        let setup = setup(&state)?;
        let settings = setup.settings;
        let exe = settings
            .executable
            .clone()
            .filter(|p| p.exists())
            .ok_or("Set Lightroom Classic in Lightroom setup first.")?;
        let root = catalog_root(&settings)?;
        let launch = match trip {
            Some(name) => prepare_trip(&settings, &root, &folders, &name)?,
            None => {
                let catalog = Catalog::for_record(&root, &folders[0], &record)?;
                prepare(&settings, &catalog, &folders)?
            }
        };
        // Registration and completion share the ready lock: completion cannot
        // remove the marker just before this command writes it again.
        let registered = {
            let mut ready = state.imports.ready.lock().map_err(|e| e.to_string())?;
            if let Some(record) = ready.as_mut().filter(|r| r.started_at == import_id) {
                state
                    .imports
                    .catalogs
                    .lock()
                    .map_err(|e| e.to_string())?
                    .register(launch.catalog.clone(), folders.clone())?;
                remember_catalog(&state, record, &launch.catalog)?;
                true
            } else {
                false
            }
        };
        if during_upload && !registered {
            // Template preparation can outlast the upload. Its final history is
            // durable before ready is cleared, so correct the list before launch.
            let mut final_result = state
                .ledger
                .lock()
                .map_err(|e| e.to_string())?
                .imports
                .iter()
                .find(|r| r.started_at == import_id)
                .cloned()
                .ok_or("Import no longer in history")?;
            let warnings = final_result.warnings.len();
            let mut handoff = UploadCatalogs::default();
            handoff.entries.push((launch.catalog.clone(), folders));
            handoff.finish(&mut final_result);
            if final_result.warnings.len() > warnings {
                return Err(final_result.warnings[warnings..].join(" "));
            }
            remember_catalog(&state, &mut final_result, &launch.catalog)?;
        }
        launch_lightroom(&exe, &launch.catalog)?;
        if launch.already_open {
            ask_plugin_to_add();
        }
        Ok(launch)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn finish_catalog(
    import_id: NaiveDateTime,
    shoot: usize,
    app: AppHandle,
) -> Result<Finished, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        if state.imports.is_active() {
            return Err("Wait for the NAS upload to finish before Finish.".into());
        }
        let folder = import_folder(&state, import_id, shoot)?;
        let root = catalog_root(&resolved(&state)?)?;
        finish(&Catalog::for_record(
            &root,
            &folder,
            &import_record(&state, import_id)?,
        )?)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Debug, Serialize)]
pub struct CatalogState {
    /// The import, by start time.
    import_id: NaiveDateTime,
    /// The import's shoots in this catalog, by index.
    shoots: Vec<usize>,
    name: String,
    trip: bool,
    /// The working catalog, including an early catalog's original local name.
    catalog: Option<PathBuf>,
    /// "open" in Lightroom, "local", "nas" (finished), "none" (not created yet) or
    /// "missing" (the shoot folder is gone from the NAS).
    state: &'static str,
    /// Why a "missing" shoot can't be opened.
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

pub(super) struct DeliveryTarget {
    pub folders: Vec<PathBuf>,
    pub catalog: Option<PathBuf>,
}

/// Read-only resolution, shared by Deliver and the CLI plan. A trip's own handoff
/// is authoritative, including shoots imported on other cards or computers.
pub(super) fn delivery_target(root: &Path, folder: &Path) -> Result<DeliveryTarget, String> {
    let c = Catalog::for_shoot(root, folder)?;
    delivery_from_catalog(c, folder, false)
}

pub(super) fn delivery_target_for_import(
    state: &AppState,
    import_id: NaiveDateTime,
    folder: &Path,
) -> Result<DeliveryTarget, String> {
    let record = import_record(state, import_id)?;
    let c = Catalog::for_record(&delivery_root(state)?, folder, &record)?;
    let day = folder
        .file_name()
        .ok_or("Missing shoot folder")?
        .to_string_lossy()
        .chars()
        .take(10)
        .collect::<String>();
    let grouped = record.lightroom_catalogs.contains(&c.lrcat())
        && record
            .shoots
            .iter()
            .filter(|s| {
                s.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(&day))
            })
            .count()
            > 1;
    delivery_from_catalog(c, folder, grouped)
}

fn delivery_from_catalog(
    c: Catalog,
    folder: &Path,
    grouped: bool,
) -> Result<DeliveryTarget, String> {
    let catalog = if c.lrcat().is_file() {
        Some(c.lrcat())
    } else {
        c.nas_lrcat().is_file().then(|| c.nas_lrcat())
    };
    let folders = if c.trip || grouped {
        let local = c.handoff();
        let list = if local.is_file() {
            local
        } else {
            c.nas_dir.join(format!("{}.card-intake.txt", c.name))
        };
        let text = fs::read_to_string(&list).map_err(|e| {
            format!(
                "Couldn't read the trip's shoot list {}: {e}",
                list.display()
            )
        })?;
        let mut folders = Vec::new();
        for line in text
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.starts_with('#'))
        {
            let path = PathBuf::from(line);
            if !path.is_absolute()
                || path.parent() != folder.parent()
                || path.components().any(|p| matches!(p, Component::ParentDir))
            {
                return Err(
                    "The trip's shoot list contains a folder outside this year and share.".into(),
                );
            }
            if !folders.contains(&path) {
                folders.push(path);
            }
        }
        if folders.is_empty() {
            return Err("The trip's shoot list is empty.".into());
        }
        folders
    } else {
        vec![folder.to_path_buf()]
    };
    Ok(DeliveryTarget { folders, catalog })
}

pub(super) fn delivery_root(state: &AppState) -> Result<PathBuf, String> {
    catalog_root(&resolved(state)?)
}

/// Where each completed import's catalogs are, for the history table. An import whose
/// shoots share a trip lists the trip once.
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
            .filter(|r| r.status == ImportStatus::Complete && (!r.offline || r.sent.is_some()))
        {
            let mine: &mut Vec<CatalogState> = &mut Vec::new();
            for (i, relative) in record.shoots.iter().enumerate() {
                let folder = match locate(&record.nas_root, relative) {
                    Located::Found(folder) => folder,
                    // Offline: show what this computer knows about the catalog.
                    Located::Offline(_) => record.nas_root.join(relative),
                    Located::Missing(note) => {
                        mine.push(CatalogState {
                            import_id: record.started_at,
                            shoots: vec![i],
                            name: relative
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                            trip: false,
                            catalog: None,
                            state: "missing",
                            note: Some(note),
                        });
                        continue;
                    }
                };
                let Ok(catalog) = Catalog::for_record(&root, &folder, record) else {
                    continue;
                };
                match mine.iter_mut().find(|c| {
                    c.state != "missing" && c.name == catalog.name && c.trip == catalog.trip
                }) {
                    Some(same) => same.shoots.push(i),
                    None => mine.push(CatalogState {
                        import_id: record.started_at,
                        shoots: vec![i],
                        state: catalog.state(),
                        catalog: Some(catalog.lrcat()),
                        name: catalog.name,
                        trip: catalog.trip,
                        note: None,
                    }),
                }
            }
            states.append(mine);
        }
        Ok(states)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_heartbeat_precedes_lightroom_refreshes_and_is_removed_for_every_outcome() {
        for status in [
            ImportStatus::Complete,
            ImportStatus::Cancelled,
            ImportStatus::Failed,
        ] {
            let f = fixture();
            let anticipated = f.share.join("2026/2026-11-10_Test");
            let catalog = Catalog::shoot(&f.root, &anticipated).unwrap();
            let launch =
                prepare(&f.settings, &catalog, std::slice::from_ref(&anticipated)).unwrap();
            assert!(
                !anticipated.exists(),
                "preparing a catalog must not create a shoot on the NAS"
            );
            let mut uploads = UploadCatalogs::default();
            uploads
                .register(launch.catalog.clone(), vec![anticipated.clone()])
                .unwrap();
            let marker = uploading_path(&launch.catalog);
            let heartbeat: u64 = fs::read_to_string(&marker)
                .unwrap()
                .lines()
                .next()
                .unwrap()
                .parse()
                .unwrap();
            assert!(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    .saturating_sub(heartbeat)
                    <= 1
            );
            assert!(finish(&catalog).unwrap_err().contains("upload"));
            fs::write(&marker, b"1\n").unwrap();
            uploads.refreshed = Some(Instant::now() - Duration::from_secs(6));
            uploads.refresh();
            assert_ne!(fs::read_to_string(&marker).unwrap(), "1\n");
            let final_folder = f.shoot("2026-11-10_Earlier");
            let mut result = crate::imports::tests::recovery_result(f._dir.path());
            result.upload_to = Some(f.share.clone());
            result.shoots = vec![
                PathBuf::from("2026/2026-11-10_Earlier"),
                PathBuf::from("2026/2026-11-11_Unrelated"),
            ];
            result.status = status;
            uploads.finish(&mut result);
            assert!(!marker.exists());
            assert_eq!(
                listed(&catalog)
                    .into_iter()
                    .map(PathBuf::from)
                    .collect::<Vec<_>>(),
                vec![final_folder]
            );
            assert!(result.warnings.is_empty(), "{:?}", result.warnings);
            finish(&catalog).unwrap();
            assert!(!catalog.nas_dir.join(marker.file_name().unwrap()).exists());
            assert!(catalog.kept().iter().all(|n| !n.ends_with(".uploading")));
        }
    }

    #[test]
    fn dropping_upload_state_removes_its_heartbeat_on_exit() {
        let f = fixture();
        let folder = f.shoot("2026-11-10_Test");
        let catalog = Catalog::shoot(&f.root, &folder).unwrap();
        prepare(&f.settings, &catalog, std::slice::from_ref(&folder)).unwrap();
        let mut uploads = UploadCatalogs::default();
        uploads.register(catalog.lrcat(), vec![folder]).unwrap();
        assert!(uploading_path(&catalog.lrcat()).exists());
        drop(uploads);
        assert!(!uploading_path(&catalog.lrcat()).exists());
    }

    #[test]
    fn an_upload_trip_lists_future_nas_folders_without_creating_them() {
        let f = fixture();
        let folders = vec![
            f.share.join("2026/2026-11-10_First"),
            f.share.join("2026/2026-11-11_Second"),
        ];
        let launch = prepare_trip(&f.settings, &f.root, &folders, "Trip").unwrap();
        assert!(folders.iter().all(|p| !p.exists()));
        for folder in &folders {
            assert_eq!(
                Catalog::for_shoot(&f.root, folder).unwrap().lrcat(),
                launch.catalog
            );
        }
        assert!(prepare_trip(&f.settings, &f.root, &folders, "Another trip").is_err());
    }

    #[test]
    fn an_early_catalog_keeps_its_edits_open_finish_reopen_and_deliver_when_every_photo_is_elsewhere()
     {
        let f = fixture();
        let state = crate::tests::local_state(f._dir.path());
        state.settings.lock().unwrap().lightroom = f.settings.clone();
        let planned = f.share.join("2026/2026-11-10_Test");
        let early = Catalog::shoot(&f.root, &planned).unwrap();
        prepare(&f.settings, &early, std::slice::from_ref(&planned)).unwrap();
        fs::write(
            early.lrcat(),
            b"SQLite format 3\0edits made while uploading",
        )
        .unwrap();
        let actual = f.shoot("2026-11-10_Earlier");
        let mut result = crate::imports::tests::recovery_result(f._dir.path());
        result.offline = false;
        result.uploading = false;
        result.nas_root = f.share.clone();
        result.shoots = vec![PathBuf::from("2026/2026-11-10_Earlier")];
        let mut uploads = UploadCatalogs::default();
        uploads.register(early.lrcat(), vec![planned]).unwrap();
        uploads.finish(&mut result);
        remember_catalog(&state, &mut result, &early.lrcat()).unwrap();
        let loaded = card_intake_core::ledger::Ledger::load(&state.ledger_path)
            .unwrap()
            .imports
            .remove(0);
        assert_eq!(loaded.lightroom_catalogs, vec![early.lrcat()]);
        let resolved = Catalog::for_record(&f.root, &actual, &loaded).unwrap();
        assert_eq!(resolved.lrcat(), early.lrcat());
        assert_eq!(resolved.nas_dir, actual.join("_catalog"));
        assert_eq!(
            delivery_target_for_import(&state, result.started_at, &actual)
                .unwrap()
                .catalog,
            Some(early.lrcat())
        );
        finish(&resolved).unwrap();
        assert_eq!(resolved.state(), "nas");
        assert_eq!(
            fs::read(resolved.nas_lrcat()).unwrap(),
            b"SQLite format 3\0edits made while uploading"
        );
        // Another computer can identify the archived catalog's original name.
        assert_eq!(
            Catalog::for_shoot(&f.root, &actual).unwrap().nas_lrcat(),
            resolved.nas_lrcat()
        );
        prepare(&f.settings, &resolved, std::slice::from_ref(&actual)).unwrap();
        assert_eq!(
            fs::read(resolved.lrcat()).unwrap(),
            b"SQLite format 3\0edits made while uploading"
        );
    }

    #[test]
    fn sending_a_recovered_import_reconciles_its_saved_early_catalog_list() {
        let f = fixture();
        let planned = f.share.join("2026/2026-11-10_Test");
        let catalog = Catalog::shoot(&f.root, &planned).unwrap();
        prepare(&f.settings, &catalog, &[planned]).unwrap();
        let actual = f.shoot("2026-11-10_Earlier");
        let mut result = crate::imports::tests::recovery_result(f._dir.path());
        result.offline = false;
        result.upload_to = Some(f.share.clone());
        result.nas_root = f.share.clone();
        result.lightroom_catalogs = vec![catalog.lrcat()];
        let original = result.shoots.clone();
        result.shoots = vec![PathBuf::from("2026/2026-11-10_Earlier")];
        refresh_saved_handoffs(&mut result, &original);
        assert_eq!(
            listed(&catalog)
                .into_iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>(),
            vec![actual]
        );
        assert!(!uploading_path(&catalog.lrcat()).exists());
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        share: PathBuf,
        settings: LightroomSettings,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let template = dir.path().join("Empty.lrcat");
        fs::write(&template, b"SQLite format 3\0test template").unwrap();
        let root = dir.path().join("Catalogs");
        let share = dir.path().join("photos");
        Fixture {
            settings: LightroomSettings {
                template: Some(template),
                catalog_root: Some(root.clone()),
                ..Default::default()
            },
            root,
            share,
            _dir: dir,
        }
    }

    impl Fixture {
        fn shoot(&self, name: &str) -> PathBuf {
            let folder = self.share.join(&name[..4]).join(name);
            fs::create_dir_all(&folder).unwrap();
            folder
        }
    }

    fn listed(catalog: &Catalog) -> Vec<String> {
        fs::read_to_string(catalog.handoff())
            .unwrap()
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn history_finds_a_shoot_folder_that_was_renamed_or_never_created() {
        let f = fixture();
        let video = f.shoot("2026-11-03_video-test");
        let tokyo = f.shoot("2026-11-06_夜景");
        let rel = |s: &str| PathBuf::from("2026").join(s);

        // Still there: used as is.
        assert_eq!(
            locate(&f.share, &rel("2026-11-06_夜景")),
            Located::Found(tokyo.clone())
        );
        // Recorded as 2026-11-03, but its files were already in 2026-11-03_video-test.
        assert_eq!(
            locate(&f.share, &rel("2026-11-03")),
            Located::Found(video.clone())
        );
        // A titled folder renamed on the NAS.
        assert_eq!(
            locate(&f.share, &rel("2026-11-06_Night")),
            Located::Found(tokyo)
        );
        // Another day's folder never counts.
        assert!(matches!(
            locate(&f.share, &rel("2026-11-04")),
            Located::Missing(m) if m.contains("isn't on the NAS any more")
        ));
        // 2026-11-030 isn't the same day.
        f.shoot("2026-11-030");
        assert!(matches!(
            locate(&f.share, &rel("2026-11-04_x")),
            Located::Missing(_)
        ));
        // Two folders for the day: no guessing.
        f.shoot("2026-11-03_Kyoto");
        assert!(matches!(
            locate(&f.share, &rel("2026-11-03")),
            Located::Missing(m) if m.contains("2026-11-03_Kyoto, 2026-11-03_video-test")
        ));
        // Undated names aren't matched by day.
        assert!(matches!(
            locate(&f.share, &rel("Holiday")),
            Located::Missing(_)
        ));
        assert!(matches!(
            locate(&f.share, Path::new("../2026/2026-11-03_video-test")),
            Located::Missing(m) if m.starts_with("Invalid")
        ));
        // The share itself is gone: say so, and don't call the folder missing.
        assert!(matches!(
            locate(&f.share.join("offline"), &rel("2026-11-03_video-test")),
            Located::Offline(m) if m.starts_with("Reconnect the photo share")
        ));
        assert_eq!(
            locate(&f.share, &rel("2026-11-03_video-test")).into_result(),
            Ok(video)
        );
    }

    #[test]
    fn a_shoot_gets_its_own_catalog_from_the_template_and_keeps_its_edits() {
        let f = fixture();
        let folder = f.shoot("2026-11-07_東京");
        let catalog = Catalog::for_shoot(&f.root, &folder).unwrap();
        let launch = prepare(&f.settings, &catalog, std::slice::from_ref(&folder)).unwrap();
        assert_eq!(
            launch.catalog,
            f.root.join("2026/2026-11-07_東京/2026-11-07_東京.lrcat")
        );
        assert!(!launch.already_open);
        assert_eq!(listed(&catalog), [folder.to_string_lossy()]);
        // Opening again reuses the catalog: Lightroom's edits are never replaced.
        fs::write(&launch.catalog, b"user edits").unwrap();
        fs::write(launch.catalog.with_extension("lrcat.lock"), b"").unwrap();
        let again = prepare(&f.settings, &catalog, std::slice::from_ref(&folder)).unwrap();
        assert!(again.already_open);
        assert_eq!(fs::read(&again.catalog).unwrap(), b"user edits");
        assert_eq!(catalog.state(), "open");
        assert_eq!(listed(&catalog).len(), 1);
    }

    #[test]
    fn deliver_resolves_open_and_finished_catalogs_and_the_whole_trip_list() {
        let f = fixture();
        let a = f.shoot("2026-11-03_A");
        let b = f.shoot("2026-11-05_B");
        let c = f.shoot("2026-11-07_C");
        assert!(delivery_target(&f.root, &a).unwrap().catalog.is_none());
        prepare_trip(
            &f.settings,
            &f.root,
            &[a.clone(), b.clone(), c.clone()],
            "Japan",
        )
        .unwrap();
        let catalog = Catalog::for_shoot(&f.root, &a).unwrap();
        fs::write(catalog.lrcat().with_extension("lrcat.lock"), b"open").unwrap();
        let target = delivery_target(&f.root, &b).unwrap();
        assert_eq!(target.folders, [a.clone(), b.clone(), c.clone()]);
        assert_eq!(target.catalog, Some(catalog.lrcat()));
        // Deliver can resolve the NAS copy and NAS handoff without reopening it.
        fs::create_dir_all(&catalog.nas_dir).unwrap();
        fs::copy(catalog.lrcat(), catalog.nas_lrcat()).unwrap();
        fs::copy(
            catalog.handoff(),
            catalog
                .nas_dir
                .join(format!("{}.card-intake.txt", catalog.name)),
        )
        .unwrap();
        fs::remove_dir_all(&catalog.dir).unwrap();
        let target = delivery_target(&f.root, &b).unwrap();
        assert_eq!(target.folders, [a, b, c]);
        assert_eq!(target.catalog, Some(catalog.nas_lrcat()));
    }

    #[test]
    fn a_trip_holds_shoots_without_a_catalog_and_each_of_them_opens_it() {
        let f = fixture();
        let tokyo = f.shoot("2026-11-03_Tokyo");
        let kyoto = f.shoot("2026-11-05_Kyoto");
        let osaka = f.shoot("2026-11-07_Osaka");
        // Osaka was opened on its own first, so it can't join.
        let own = Catalog::for_shoot(&f.root, &osaka).unwrap();
        prepare(&f.settings, &own, std::slice::from_ref(&osaka)).unwrap();
        let all = [tokyo.clone(), kyoto.clone(), osaka.clone()];
        let err = prepare_trip(&f.settings, &f.root, &all, "Japan").unwrap_err();
        assert!(
            err.contains("2026-11-07_Osaka already has its own catalog"),
            "{err}"
        );
        assert!(!tokyo.join("_catalog").exists());

        let pair = [kyoto.clone(), tokyo.clone()];
        let launch = prepare_trip(&f.settings, &f.root, &pair, " Japan  trip ").unwrap();
        assert_eq!(launch.name, "2026-11-03_Japan-trip");
        assert_eq!(
            launch.catalog,
            f.root
                .join("2026/_trips/2026-11-03_Japan-trip/2026-11-03_Japan-trip.lrcat")
        );
        let trip = Catalog::for_shoot(&f.root, &kyoto).unwrap();
        assert!(trip.trip);
        assert_eq!(trip.lrcat(), launch.catalog);
        assert_eq!(
            trip.nas_dir,
            f.share.join("2026/_trips/2026-11-03_Japan-trip")
        );
        assert_eq!(listed(&trip).len(), 2);
        assert_eq!(
            fs::read_to_string(tokyo.join("_catalog/trip.txt")).unwrap(),
            "2026/_trips/2026-11-03_Japan-trip"
        );
        // Opening the same trip again is fine; another trip isn't.
        prepare_trip(&f.settings, &f.root, &pair, "Japan trip").unwrap();
        let err = prepare_trip(&f.settings, &f.root, &pair, "Other").unwrap_err();
        assert!(
            err.contains("already in the trip 2026-11-03_Japan-trip"),
            "{err}"
        );
    }

    #[test]
    fn finish_stores_the_catalog_on_the_nas_and_reopen_brings_it_back() {
        let f = fixture();
        let folder = f.shoot("2026-10-31_Halloween");
        let catalog = Catalog::for_shoot(&f.root, &folder).unwrap();
        prepare(&f.settings, &catalog, std::slice::from_ref(&folder)).unwrap();
        let dir = &catalog.dir;
        fs::write(catalog.lrcat(), b"edits").unwrap();
        for (path, bytes) in [
            (
                "2026-10-31_Halloween Smart Previews.lrdata/A/x.dng",
                &b"smart"[..],
            ),
            ("2026-10-31_Halloween.lrcat-data/masks/m", b"mask"),
            (
                "2026-10-31_Halloween Previews.lrdata/p.lrprev",
                b"big preview",
            ),
            ("Backups/2026-11-01/old.lrcat", b"backup"),
        ] {
            let file = dir.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, bytes).unwrap();
        }
        // Not while Lightroom has it open.
        fs::write(catalog.lrcat().with_extension("lrcat.lock"), b"").unwrap();
        assert!(finish(&catalog).unwrap_err().contains("Close"));
        fs::remove_file(catalog.lrcat().with_extension("lrcat.lock")).unwrap();

        let done = finish(&catalog).unwrap();
        assert_eq!(done.stored, folder.join("_catalog"));
        let stored = folder.join("_catalog");
        assert_eq!(
            fs::read(stored.join("2026-10-31_Halloween.lrcat")).unwrap(),
            b"edits"
        );
        assert!(
            stored
                .join("2026-10-31_Halloween Smart Previews.lrdata/A/x.dng")
                .is_file()
        );
        assert!(
            stored
                .join("2026-10-31_Halloween.lrcat-data/masks/m")
                .is_file()
        );
        assert!(
            stored
                .join("2026-10-31_Halloween.card-intake.txt")
                .is_file()
        );
        assert!(!stored.join("2026-10-31_Halloween Previews.lrdata").exists());
        // The local copy is gone, apart from Lightroom's backups.
        assert!(!catalog.lrcat().exists());
        assert!(!dir.join("2026-10-31_Halloween Previews.lrdata").exists());
        assert!(dir.join("Backups/2026-11-01/old.lrcat").is_file());
        assert_eq!(catalog.state(), "nas");

        // Reopen copies it back, and finishing again replaces the NAS copy.
        let launch = prepare(&f.settings, &catalog, std::slice::from_ref(&folder)).unwrap();
        assert_eq!(fs::read(&launch.catalog).unwrap(), b"edits");
        assert!(
            dir.join("2026-10-31_Halloween.lrcat-data/masks/m")
                .is_file()
        );
        fs::write(&launch.catalog, b"more edits").unwrap();
        finish(&catalog).unwrap();
        assert_eq!(
            fs::read(stored.join("2026-10-31_Halloween.lrcat")).unwrap(),
            b"more edits"
        );
        let names: Vec<_> = fs::read_dir(&stored)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.starts_with('.')), "{names:?}");
    }

    #[test]
    fn a_finished_trip_goes_to_the_years_trips_folder() {
        let f = fixture();
        let shoots = [
            f.shoot("2026-12-30_Hokkaido"),
            f.shoot("2027-01-02_Hokkaido"),
        ];
        let launch = prepare_trip(&f.settings, &f.root, &shoots, "Snow").unwrap();
        let trip = Catalog::for_shoot(&f.root, &shoots[1]).unwrap();
        assert_eq!(trip.lrcat(), launch.catalog);
        let done = finish(&trip).unwrap();
        assert_eq!(done.stored, f.share.join("2026/_trips/2026-12-30_Snow"));
        assert!(done.stored.join("2026-12-30_Snow.lrcat").is_file());
        assert_eq!(
            Catalog::for_shoot(&f.root, &shoots[0]).unwrap().state(),
            "nas"
        );
    }

    #[test]
    fn installs_the_plug_in_and_presets_for_lightroom() {
        let dir = tempfile::tempdir().unwrap();
        let (adobe, local) = (dir.path().join("Adobe"), dir.path().join("local"));
        let old = local.join("Lightroom/CardIntake.lrplugin");
        fs::create_dir_all(&old).unwrap();
        let plugin = install_plugin(&adobe, &local).unwrap();
        assert_eq!(plugin, adobe.join("Lightroom/Modules/CardIntake.lrplugin"));
        assert!(plugin.join("Info.lua").is_file() && plugin.join("Intake.lua").is_file());
        assert!(!old.exists());
        // Every Lua file in the plug-in's folder is built in, or Lightroom would get a
        // plug-in whose Info.lua names files that were never installed.
        let source =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../lrplugin/CardIntake.lrplugin");
        let mut lua: Vec<String> = fs::read_dir(source)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".lua"))
            .collect();
        lua.sort();
        let mut built_in: Vec<String> = PLUGIN.iter().map(|(n, _)| n.to_string()).collect();
        built_in.sort();
        assert_eq!(built_in, lua);
        let info = fs::read_to_string(plugin.join("Info.lua")).unwrap();
        let id = PLUGIN_URL
            .strip_prefix("lightroom://")
            .and_then(|rest| rest.split('/').next())
            .unwrap();
        assert!(info.contains(&format!("LrToolkitIdentifier = \"{id}\"")));
        assert!(info.contains("URLHandler = \"UrlHandler.lua\""));

        let presets = dir.path().join("Profiles");
        fs::create_dir_all(presets.join("FroPack1")).unwrap();
        fs::write(presets.join("FroPack1/Apollo.xmp"), b"<x:xmpmeta/>").unwrap();
        fs::write(presets.join("FroPack1/~READ ME FIRST.rtf"), b"rtf").unwrap();
        assert_eq!(install_presets(&presets, &adobe).unwrap(), 1);
        let installed = adobe.join("CameraRaw/Settings/Card Intake/FroPack1");
        assert!(installed.join("Apollo.xmp").is_file());
        assert!(!installed.join("~READ ME FIRST.rtf").exists());
        assert_eq!(install_presets(&presets, &adobe).unwrap(), 1);
        assert!(install_presets(&installed.join("none"), &adobe).is_err());
    }

    #[test]
    fn refuses_open_templates() {
        let f = fixture();
        let template = f.settings.template.clone().unwrap();
        fs::write(template.with_extension("lrcat.lock"), b"lock").unwrap();
        assert!(check_template(&template).unwrap_err().contains("Close"));
        let folder = f.shoot("2026-11-07_X");
        let catalog = Catalog::for_shoot(&f.root, &folder).unwrap();
        assert!(prepare(&f.settings, &catalog, &[folder]).is_err());
        assert!(!catalog.lrcat().exists());
    }
}
