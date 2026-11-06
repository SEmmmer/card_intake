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
];

/// A trip's folder next to the year's shoots, on the NAS and on this computer.
const TRIPS: &str = "_trips";
/// In a shoot's `_catalog` folder: the trip catalog the shoot belongs to.
const TRIP_NOTE: &str = "trip.txt";

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
        match trip_of(folder)? {
            Some((year, name)) => {
                let share = folder
                    .parent()
                    .and_then(Path::parent)
                    .ok_or("Invalid shoot folder.")?;
                Ok(Catalog::trip(root, share, &year, &name))
            }
            None => Catalog::shoot(root, folder),
        }
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
    /// Lightroom already has it open, so the plug-in won't run by itself: use
    /// Library > Plug-in Extras > Add new files from NAS.
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
    let mut text = String::from(
        "# Card Intake: when this catalog opens, Lightroom adds new raw photos and DxO DNGs from these shoot folders.\n",
    );
    for folder in &listed {
        text.push_str(folder);
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

/// The completed import's shoot folders on the NAS.
fn import_folders(state: &AppState, import_id: NaiveDateTime) -> Result<Vec<PathBuf>, String> {
    let record = state
        .ledger
        .lock()
        .map_err(|e| e.to_string())?
        .imports
        .iter()
        .find(|r| r.started_at == import_id && r.status == ImportStatus::Complete)
        .cloned()
        .ok_or("Choose a completed import from the history.")?;
    let mut folders = Vec::new();
    for relative in &record.shoots {
        if relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err("Invalid shoot path in import history.".into());
        }
        let folder = record.nas_root.join(relative);
        if !folder.is_dir() || folder.to_str().is_none_or(|s| s.contains(['\r', '\n'])) {
            return Err(format!(
                "Reconnect the photo share: {} is unavailable.",
                folder.display()
            ));
        }
        folders.push(folder);
    }
    if folders.is_empty() {
        return Err("This import has no shoot folders.".into());
    }
    Ok(folders)
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
        if state.imports.is_active() {
            return Err("Wait for the import to finish before opening Lightroom.".into());
        }
        let folders = import_folders(&state, import_id)?;
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
                let folder = folders.get(shoot.unwrap_or(0)).ok_or("Shoot not found")?;
                let catalog = Catalog::for_shoot(&root, folder)?;
                prepare(&settings, &catalog, std::slice::from_ref(folder))?
            }
        };
        launch_lightroom(&exe, &launch.catalog)?;
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
        let folders = import_folders(&state, import_id)?;
        let root = catalog_root(&resolved(&state)?)?;
        let folder = folders.get(shoot).ok_or("Shoot not found")?;
        finish(&Catalog::for_shoot(&root, folder)?)
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
    /// "open" in Lightroom, "local", "nas" (finished) or "none" (not created yet).
    state: &'static str,
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
            .filter(|r| r.status == ImportStatus::Complete)
        {
            let mine: &mut Vec<CatalogState> = &mut Vec::new();
            for (i, relative) in record.shoots.iter().enumerate() {
                let folder = record.nas_root.join(relative);
                let Ok(catalog) = Catalog::for_shoot(&root, &folder) else {
                    continue;
                };
                match mine
                    .iter_mut()
                    .find(|c| c.name == catalog.name && c.trip == catalog.trip)
                {
                    Some(same) => same.shoots.push(i),
                    None => mine.push(CatalogState {
                        import_id: record.started_at,
                        shoots: vec![i],
                        state: catalog.state(),
                        name: catalog.name,
                        trip: catalog.trip,
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
