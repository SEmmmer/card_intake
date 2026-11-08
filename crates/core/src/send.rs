//! Send offline shoots to the NAS, and remember exactly which laptop files were
//! verified so a later deletion cannot remove new edits or extra files.

use crate::{
    backup::Target,
    copies::{self, CopyCounts, CopyFile},
    destinations,
    import::{self, ImportProgress, ImportStatus},
    scan,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub shoot: PathBuf,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sent {
    pub at: DateTime<Utc>,
    pub nas_root: PathBuf,
    pub counts: CopyCounts,
    pub sources: Vec<Source>,
    pub kept_sidecars: Vec<PathBuf>,
    pub found_in: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifiedFile {
    pub relative: PathBuf,
    pub size: u64,
    pub modified: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub offline_root: PathBuf,
    pub shoots: Vec<PathBuf>,
    pub files: Vec<VerifiedFile>,
}

impl Manifest {
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let dir = path.parent().ok_or("Missing manifest folder")?;
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let mut temp = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
        use std::io::Write;
        temp.write_all(&serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        temp.as_file().sync_all().map_err(|e| e.to_string())?;
        temp.persist(path).map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self, String> {
        serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())
    }
}

#[derive(Debug, Serialize)]
pub struct SendResult {
    pub status: ImportStatus,
    pub counts: CopyCounts,
    pub error: Option<String>,
    pub sent: Option<Sent>,
    pub manifest: Manifest,
}

pub(crate) fn shoot_date(shoot: &Path) -> Result<NaiveDate, String> {
    let parts: Vec<_> = shoot.components().collect();
    if parts.len() != 2
        || parts
            .iter()
            .any(|p| !matches!(p, std::path::Component::Normal(_)))
    {
        return Err("Invalid shoot folder in import history.".into());
    }
    let name = shoot.file_name().unwrap().to_string_lossy();
    let date = name.get(..10).ok_or("Invalid shoot date")?;
    let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").map_err(|e| e.to_string())?;
    if shoot.parent().unwrap() != Path::new(&date.format("%Y").to_string()) {
        return Err("Invalid shoot year.".into());
    }
    Ok(date)
}

struct Planned {
    file: CopyFile,
    target: PathBuf,
    existing: bool,
    laptop: bool,
    drive: Option<Target>,
    /// Already copied and verified on the NAS while the card was being read.
    relayed: bool,
}

/// A photo or clip an import sent to the NAS while it was still reading the card.
#[derive(Debug, Clone)]
pub struct Relayed {
    /// Where it is on the NAS, relative to the NAS root.
    pub target: PathBuf,
    /// Copied now, rather than found identical on the NAS.
    pub copied: bool,
    /// The date's other NAS folder that already held it, when it did.
    pub found_in: Option<PathBuf>,
    pub size: u64,
    pub modified: SystemTime,
}

/// The key a laptop file is known by: its path, the same on every OS and in any case.
pub fn key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

/// The date folders already on the NAS, looked up once per date and shared by the
/// import's senders.
pub type DateFolders = Mutex<HashMap<NaiveDate, Arc<Vec<destinations::FolderIndex>>>>;

fn date_folders(
    nas: &Path,
    date: NaiveDate,
    cache: &DateFolders,
) -> Arc<Vec<destinations::FolderIndex>> {
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .entry(date)
        .or_insert_with(|| Arc::new(destinations::date_folders(nas, date)))
        .clone()
}

/// Where a photo or clip goes on the NAS: its own path, unless it isn't there and an
/// identical copy is already in another folder for the same date (an earlier import
/// under another title), which is then used. Returns that folder too.
fn nas_target(
    nas: &Path,
    shoot: &Path,
    file: &CopyFile,
    others: &[destinations::FolderIndex],
    cancel: &AtomicBool,
) -> Result<(PathBuf, Option<PathBuf>), String> {
    let target = file.relative.clone();
    if nas.join(&target).exists() {
        return Ok((target, None));
    }
    let inside = file
        .relative
        .strip_prefix(shoot)
        .map_err(|e| e.to_string())?;
    for folder in others {
        let other = folder.path.join(inside);
        let relative = other.strip_prefix(nas).map_err(|e| e.to_string())?;
        copies::inspect(nas, relative)?;
        match fs::symlink_metadata(&other) {
            Ok(meta) if meta.is_file() && meta.len() == file.size => {}
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.to_string()),
        }
        let mut candidate = file.clone();
        candidate.relative = relative.to_path_buf();
        if copies::compare(&candidate, nas, false, cancel)? == Some(true) {
            let folder = candidate.relative.components().take(2).collect::<PathBuf>();
            return Ok((candidate.relative, Some(folder)));
        }
        copies::cancelled(cancel)?;
    }
    Ok((target, None))
}

/// Sends one photo or clip of an import to the NAS as soon as its laptop copy is
/// verified, with Send's rules: an identical copy (here or in the date's other
/// folders) is skipped, a different photo with the same name stops the import, and a
/// new one is copied and read back against the card's checksum.
pub fn relay_file(
    nas: &Path,
    file: &CopyFile,
    require_network: bool,
    cache: &DateFolders,
    cancel: &AtomicBool,
    progress: impl FnMut(u64),
) -> Result<Relayed, String> {
    copies::cancelled(cancel)?;
    import::check_destination(nas, require_network)?;
    let shoot: PathBuf = file.relative.components().take(2).collect();
    let date = shoot_date(&shoot)?;
    let others = date_folders(nas, date, cache);
    let (target, found_in) = nas_target(nas, &shoot, file, &others, cancel)?;
    copies::inspect(nas, &target)?;
    let mut candidate = file.clone();
    candidate.relative = target.clone();
    let copied = match copies::compare(&candidate, nas, false, cancel)? {
        Some(true) => false,
        Some(false) => return Err(copies::conflict(&nas.join(&target))),
        None => {
            import::check_destination(nas, require_network)?;
            let mut progress = progress;
            copies::copy(&candidate, nas, cancel, |_, n| progress(n))?;
            true
        }
    };
    Ok(Relayed {
        target,
        copied,
        found_in,
        size: file.size,
        modified: file.modified,
    })
}

fn sidecar(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("xmp") || e.eq_ignore_ascii_case("xml"))
}

fn sibling_key(path: &Path) -> String {
    let stem = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    let stem = if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("xml"))
    {
        stem.strip_suffix("m01").unwrap_or(&stem)
    } else {
        &stem
    };
    format!(
        "{}/{}",
        path.parent().unwrap_or(Path::new("")).display(),
        stem
    )
    .replace('\\', "/")
    .to_lowercase()
}

/// `require_network = false` is for temporary test folders. The desktop always
/// requires a confirmed network share, rechecked before every file.
pub fn send(
    offline: &Path,
    shoots: &[PathBuf],
    drives: &[Target],
    nas: &Path,
    require_network: bool,
    cancel: &AtomicBool,
    progress: impl FnMut(ImportProgress),
) -> SendResult {
    let source = Manifest {
        offline_root: offline.into(),
        shoots: shoots.to_vec(),
        files: Vec::new(),
    };
    send_import(&source, drives, nas, require_network, cancel, progress)
}

/// An offline import supplies its original file list. A missing source file must
/// stop Send rather than let a partly deleted folder count as completely sent.
pub fn send_import(
    source: &Manifest,
    drives: &[Target],
    nas: &Path,
    require_network: bool,
    cancel: &AtomicBool,
    progress: impl FnMut(ImportProgress),
) -> SendResult {
    send_import_with(
        source,
        drives,
        nas,
        require_network,
        cancel,
        progress,
        &HashMap::new(),
        false,
    )
}

/// Send, for an import that already sent some of its photos while reading the card:
/// `relayed` files that haven't changed since are taken as verified without reading
/// them again, and with `only_listed` only the import's own files are sent, not other
/// files in the same shoot folders.
#[allow(clippy::too_many_arguments)]
pub fn send_import_with(
    source: &Manifest,
    drives: &[Target],
    nas: &Path,
    require_network: bool,
    cancel: &AtomicBool,
    mut progress: impl FnMut(ImportProgress),
    relayed: &HashMap<String, Relayed>,
    only_listed: bool,
) -> SendResult {
    let offline = &source.offline_root;
    let shoots = &source.shoots;
    let mut result = SendResult {
        status: ImportStatus::Failed,
        counts: CopyCounts::default(),
        error: None,
        sent: None,
        manifest: Manifest {
            offline_root: offline.into(),
            shoots: shoots.to_vec(),
            files: Vec::new(),
        },
    };
    let outcome = (|| {
        copies::cancelled(cancel)?;
        import::check_destination(nas, require_network)?;
        let mut plan = Vec::new();
        let mut sources = Vec::new();
        let mut found_in = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let listed: BTreeSet<_> = source.files.iter().map(|f| key(&f.relative)).collect();
        for shoot in shoots {
            let date = shoot_date(shoot)?;
            copies::inspect(offline, shoot)
                .or_else(|e| if !offline.exists() { Ok(()) } else { Err(e) })?;
            let (root, source, laptop, source_drive) = if offline.join(shoot).is_dir() {
                (
                    offline.to_path_buf(),
                    "This computer".to_string(),
                    true,
                    None,
                )
            } else {
                let drive = drives
                    .iter()
                    .find(|d| d.check().is_ok() && d.folder().join(shoot).is_dir())
                    .ok_or_else(|| {
                        format!(
                            "{} isn't on this computer or a connected backup drive.",
                            shoot.display()
                        )
                    })?;
                (
                    drive.folder(),
                    format!("{} ({})", drive.label, drive.uuid),
                    false,
                    Some(drive.clone()),
                )
            };
            sources.push(Source {
                shoot: shoot.clone(),
                source,
            });
            let mut files = copies::files(&root, shoot)?;
            if only_listed {
                files.retain(|f| listed.contains(&key(&f.relative)));
            }
            // Resolve originals first, so a sidecar follows a photo already under
            // another title for the same date.
            files.sort_by_key(|f| {
                (
                    if scan::uses_raw_sidecar(&f.relative) {
                        0
                    } else if sidecar(&f.relative) {
                        2
                    } else {
                        1
                    },
                    f.relative.clone(),
                )
            });
            let mut siblings = HashMap::new();
            let others = destinations::date_folders(nas, date);
            for file in files {
                copies::cancelled(cancel)?;
                // Sent while the card was read, and unchanged since: already verified.
                if laptop
                    && let Some(done) = relayed.get(&key(&file.relative))
                    && done.size == file.size
                    && done.modified == file.modified
                {
                    if let Some(folder) = &done.found_in {
                        found_in.insert(folder.clone());
                    }
                    siblings
                        .entry(sibling_key(&file.relative))
                        .or_insert_with(|| done.target.parent().unwrap().to_path_buf());
                    if !seen.insert(done.target.to_string_lossy().to_lowercase()) {
                        return Err(format!(
                            "Two source files would use {}. Nothing was copied.",
                            done.target.display()
                        ));
                    }
                    plan.push(Planned {
                        target: done.target.clone(),
                        file,
                        existing: true,
                        laptop,
                        drive: None,
                        relayed: true,
                    });
                    continue;
                }
                let mut target = file.relative.clone();
                if sidecar(&file.relative) {
                    if let Some(parent) = siblings.get(&sibling_key(&file.relative)) {
                        target = Path::new(parent).join(file.relative.file_name().unwrap());
                    }
                } else {
                    let (resolved, folder) = nas_target(nas, shoot, &file, &others, cancel)?;
                    target = resolved;
                    if let Some(folder) = folder {
                        found_in.insert(folder);
                    }
                }
                if scan::kind_of(&file.relative).is_some() {
                    siblings
                        .entry(sibling_key(&file.relative))
                        .or_insert_with(|| target.parent().unwrap().to_path_buf());
                }
                copies::inspect(nas, &target)?;
                let mut candidate = file.clone();
                candidate.relative = target.clone();
                let existing = match copies::compare(&candidate, nas, false, cancel)? {
                    None => false,
                    Some(true) => true,
                    Some(false) if sidecar(&file.relative) => true,
                    Some(false) => return Err(copies::conflict(&nas.join(&target))),
                };
                // A RAW+JPEG pair may share a sidecar; duplicate final names
                // still have to agree on contents before they can be skipped.
                let key = target.to_string_lossy().to_lowercase();
                if !seen.insert(key) {
                    return Err(format!(
                        "Two source files would use {}. Nothing was copied.",
                        target.display()
                    ));
                }
                plan.push(Planned {
                    file,
                    target,
                    existing,
                    laptop,
                    drive: source_drive.clone(),
                    relayed: false,
                });
            }
        }
        let present: BTreeSet<_> = plan.iter().map(|p| key(&p.file.relative)).collect();
        for file in &source.files {
            if !present.contains(&key(&file.relative)) {
                return Err(format!(
                    "{} is missing from this copy of the shoot. Restore it from a backup drive before sending.",
                    file.relative.display()
                ));
            }
        }
        if plan.is_empty() {
            return Err("No files were found in this import's shoot folders.".into());
        }
        let needed = plan
            .iter()
            .filter(|p| !p.existing)
            .map(|p| p.file.size)
            .sum();
        copies::check_space(needed, crate::nas::space(nas).map(|s| s.0), "the NAS")?;
        let files: Vec<_> = plan.iter().map(|p| p.file.clone()).collect();
        let mut p = copies::Progress::new(&files, "NAS");
        let mut kept_sidecars = Vec::new();
        for entry in plan {
            copies::cancelled(cancel)?;
            if entry.relayed {
                let mut file = entry.file.clone();
                file.relative = entry.target.clone();
                result.manifest.files.push(VerifiedFile {
                    relative: entry.file.relative,
                    size: file.size,
                    modified: file.modified,
                });
                p.counts.verified += 1;
                p.counts.bytes += file.size;
                p.emit("file_complete", &file, 0, &mut progress);
                result.counts = p.counts.clone();
                continue;
            }
            import::check_destination(nas, require_network)?;
            if let Some(drive) = &entry.drive {
                drive.check()?;
            }
            if entry.laptop {
                crate::offline::validate(offline)?;
            }
            copies::unchanged(&entry.file)?;
            let mut file = entry.file.clone();
            file.relative = entry.target.clone();
            p.emit("sending", &file, 0, &mut progress);
            let kept = match copies::compare(&file, nas, false, cancel)? {
                Some(false) if sidecar(&file.relative) => {
                    kept_sidecars.push(entry.target);
                    p.counts.skipped += 1;
                    true
                }
                Some(false) => return Err(copies::conflict(&nas.join(&file.relative))),
                Some(true) => {
                    p.counts.skipped += 1;
                    false
                }
                None => {
                    copies::copy(&file, nas, cancel, |_, n| {
                        p.emit("sending", &file, n, &mut progress)
                    })?;
                    p.counts.copied += 1;
                    false
                }
            };
            if entry.laptop && !kept {
                result.manifest.files.push(VerifiedFile {
                    relative: entry.file.relative,
                    size: file.size,
                    modified: file.modified,
                });
            }
            p.counts.verified += 1;
            p.counts.bytes += file.size;
            p.emit("file_complete", &file, 0, &mut progress);
            result.counts = p.counts.clone();
        }
        result.sent = Some(Sent {
            at: Utc::now(),
            nas_root: nas.into(),
            counts: result.counts.clone(),
            sources,
            kept_sidecars,
            found_in: found_in.into_iter().collect(),
        });
        Ok::<_, String>(())
    })();
    result.status = if outcome.is_ok() {
        ImportStatus::Complete
    } else if cancel.load(Ordering::Relaxed) {
        ImportStatus::Cancelled
    } else {
        ImportStatus::Failed
    };
    result.error = outcome.err();
    result
}

#[derive(Debug, Default, Serialize)]
pub struct Deleted {
    pub deleted: usize,
    pub kept: Vec<PathBuf>,
    pub errors: Vec<String>,
    pub complete: bool,
}

/// Only this explicit action deletes photos. The manifest is kept in app data;
/// every file is checked again, and the year folder and backup drives stay put.
pub fn delete_laptop(manifest: &Manifest) -> Result<Deleted, String> {
    let root = &manifest.offline_root;
    if !root.exists() {
        return Ok(Deleted {
            complete: true,
            ..Default::default()
        });
    }
    crate::offline::validate(root)?;
    for shoot in &manifest.shoots {
        shoot_date(shoot)?;
        copies::inspect(root, shoot)?;
    }
    let mut result = Deleted::default();
    for file in &manifest.files {
        if !manifest
            .shoots
            .iter()
            .any(|s| file.relative.starts_with(s) && file.relative != *s)
        {
            return Err("A manifest file is outside its shoot folders.".into());
        }
        copies::inspect(root, &file.relative)?;
        let path = root.join(&file.relative);
        match fs::symlink_metadata(&path) {
            Ok(meta)
                if meta.is_file()
                    && !copies::is_link(&meta)
                    && meta.len() == file.size
                    && meta.modified().ok() == Some(file.modified) =>
            {
                match fs::remove_file(&path) {
                    Ok(()) => result.deleted += 1,
                    Err(e) => result.errors.push(format!("{}: {e}", path.display())),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => result.errors.push(format!("{}: {e}", path.display())),
            _ => result.kept.push(file.relative.clone()),
        }
    }
    for shoot in &manifest.shoots {
        if !root.join(shoot).is_dir() {
            continue;
        }
        for item in walkdir::WalkDir::new(root.join(shoot))
            .contents_first(true)
            .follow_links(false)
        {
            let item = item.map_err(|e| e.to_string())?;
            if item.file_type().is_dir() {
                // remove_dir only succeeds when empty, including the shoot itself.
                let _ = fs::remove_dir(item.path());
            } else {
                let relative = item
                    .path()
                    .strip_prefix(root)
                    .map_err(|e| e.to_string())?
                    .to_path_buf();
                if !result.kept.contains(&relative) {
                    result.kept.push(relative);
                }
            }
        }
    }
    result.complete = result.errors.is_empty() && result.kept.is_empty();
    Ok(result)
}

#[cfg(test)]
mod tests;
