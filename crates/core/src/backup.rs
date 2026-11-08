//! A drive is identified by one marker at its volume root, never by its letter.

use crate::{
    copies::{self, CopyCounts, CopyFile},
    import::{ImportProgress, ImportStatus},
    nas,
    offline::{self, DiskFacts, FolderCheck},
    volumes,
};
use chrono::{DateTime, Utc};
use nas::Status;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Drive {
    pub uuid: String,
    pub label: String,
    pub added_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Marker {
    pub format: u32,
    pub uuid: String,
    pub label: String,
    pub created_at: DateTime<Utc>,
    pub computer: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Target {
    pub uuid: String,
    pub label: String,
    pub root: PathBuf,
}

impl Target {
    pub fn folder(&self) -> PathBuf {
        self.root.join(format!(".card-intake-{}", self.uuid))
    }
    pub fn check(&self) -> Result<(), String> {
        let marker = read_marker(&self.root)?;
        if marker.as_ref().is_none_or(|m| m.uuid != self.uuid) {
            return Err(format!(
                "{} isn't connected, or a different drive is there now.",
                self.label
            ));
        }
        if !matches!(nas::location(&self.root), Some(nas::Location::Local(_)))
            || volumes::camera_card(&self.root).is_some()
        {
            return Err(
                "A backup must be a local drive, not a camera card or network share.".into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DriveView {
    #[serde(flatten)]
    pub drive: Drive,
    pub root: Option<PathBuf>,
    pub free_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub filesystem: Option<String>,
}

pub fn marker_folders(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut result = Vec::new();
    for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".card-intake-")
        {
            let meta = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
            if meta.is_dir() || copies::is_link(&meta) {
                result.push(entry.path());
            }
        }
    }
    Ok(result)
}

pub fn read_marker(root: &Path) -> Result<Option<Marker>, String> {
    let folders = marker_folders(root)?;
    if folders.len() > 1 {
        return Err(
            "Two or more Card Intake marker folders are on this volume. Choose another drive."
                .into(),
        );
    }
    let Some(folder) = folders.first() else {
        return Ok(None);
    };
    copies::inspect(root, folder.strip_prefix(root).map_err(|e| e.to_string())?)?;
    copies::inspect(folder, Path::new("drive.json"))?;
    let marker: Marker = serde_json::from_slice(
        &fs::read(folder.join("drive.json"))
            .map_err(|e| format!("Couldn't read drive.json: {e}"))?,
    )
    .map_err(|e| e.to_string())?;
    let uuid = Uuid::parse_str(&marker.uuid).map_err(|e| e.to_string())?;
    if marker.format != 1
        || uuid.get_version_num() != 4
        || uuid.to_string() != marker.uuid
        || folder.file_name().unwrap().to_string_lossy() != format!(".card-intake-{}", marker.uuid)
    {
        return Err(
            "This drive's Card Intake marker isn't supported or doesn't match its folder.".into(),
        );
    }
    Ok(Some(marker))
}

/// Duplicate UUIDs on two connected volumes are ambiguous and never selected.
pub fn connected(drives: &[Drive], mounts: &[PathBuf]) -> Vec<DriveView> {
    drives
        .iter()
        .map(|drive| {
            let found: Vec<_> = mounts
                .iter()
                .filter(|root| {
                    read_marker(root)
                        .ok()
                        .flatten()
                        .is_some_and(|m| m.uuid == drive.uuid)
                })
                .collect();
            let root = (found.len() == 1).then(|| found[0].clone());
            let facts = root.as_deref().and_then(|r| offline::disk(r).ok());
            DriveView {
                drive: drive.clone(),
                root,
                free_bytes: facts.as_ref().and_then(|f| f.free),
                total_bytes: facts.as_ref().and_then(|f| f.total),
                filesystem: facts.and_then(|f| f.filesystem),
            }
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct InitCheck {
    #[serde(flatten)]
    pub check: FolderCheck,
    pub drive: Option<Drive>,
}

pub fn check_init(path: &Path, offline: &Path) -> InitCheck {
    match offline::disk(path) {
        Ok(facts) if path.is_dir() => {
            let offline_root = offline::disk(offline).ok().map(|f| f.root);
            check_init_with(offline, &facts, offline_root.as_deref(), nas::write_test)
        }
        _ => {
            let mut check = FolderCheck::new(path, None);
            check.add(
                Status::Fail,
                "Volume not found",
                "Choose an existing volume or a folder on it.",
            );
            InitCheck { check, drive: None }
        }
    }
}

/// The disk facts and round trip are seams for fixtures; production always gets
/// them from the operating system and performs the full 64 MB test.
pub fn check_init_with(
    offline: &Path,
    facts: &DiskFacts,
    offline_volume: Option<&Path>,
    test: impl FnOnce(&Path) -> Result<f64, String>,
) -> InitCheck {
    let root = &facts.root;
    let mut check = FolderCheck::new(root, Some(facts));
    let mut drive = None;
    if !root.is_dir() {
        check.add(Status::Fail, "Volume not found", "Connect the drive first.");
    }
    if !facts.local {
        check.add(
            Status::Fail,
            "Network share",
            "Choose a local backup drive.",
        );
    }
    if facts.system {
        check.add(
            Status::Fail,
            "System volume",
            "Choose a different volume for backup.",
        );
    }
    if offline_volume.is_none()
        || offline_volume.is_some_and(|p| fs::canonicalize(p).ok() == fs::canonicalize(root).ok())
    {
        check.add(
            Status::Fail,
            "Offline folder's volume",
            "The backup must be on a different volume from the offline folder.",
        );
    }
    if volumes::camera_card(root).is_some() {
        check.add(
            Status::Fail,
            "Camera card",
            "Never use a camera card as a backup drive.",
        );
    }
    let marker = match read_marker(root) {
        Ok(marker) => marker,
        Err(error) => {
            check.add(Status::Fail, "Drive marker", error);
            None
        }
    };
    if !check.ok {
        return InitCheck { check, drive };
    }
    let target = marker
        .as_ref()
        .map(|m| root.join(format!(".card-intake-{}", m.uuid)));
    let needed = (|| {
        let files = if offline.is_dir() {
            copies::files(offline, Path::new(""))?
        } else {
            Vec::new()
        };
        let mut needed = 0;
        for file in files {
            let present = target.as_ref().is_some_and(|t| {
                copies::inspect(t, &file.relative).is_ok()
                    && fs::metadata(t.join(&file.relative))
                        .is_ok_and(|m| m.is_file() && m.len() == file.size)
            });
            if !present {
                needed += file.size;
            }
        }
        copies::check_space(needed, facts.free, "the backup drive")?;
        Ok::<_, String>(needed)
    })();
    match needed {
        Ok(n) => check.add(
            Status::Pass,
            "Enough space",
            format!("{n} bytes needed, {} free", facts.free.unwrap_or(0)),
        ),
        Err(error) => check.add(Status::Fail, "Not enough space or unreadable files", error),
    }
    if !check.ok {
        return InitCheck { check, drive };
    }
    match test(target.as_deref().unwrap_or(root)) {
        Ok(_) => check.add(
            Status::Pass,
            "Test file written, read back and deleted",
            "64 MB came back identical.",
        ),
        Err(error) => check.add(Status::Fail, "Test file failed", error),
    }
    if facts
        .filesystem
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("FAT32"))
    {
        check.add(
            Status::Warn,
            "FAT32",
            "Files over 4 GB, such as long clips, can't go on this drive",
        );
    }
    if check.ok {
        let initialized = marker.is_none();
        let marker = marker.unwrap_or_else(|| Marker {
            format: 1,
            uuid: Uuid::new_v4().to_string(),
            label: facts
                .label
                .clone()
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| "Backup drive".into()),
            created_at: Utc::now(),
            computer: std::env::var("COMPUTERNAME")
                .or_else(|_| std::env::var("HOSTNAME"))
                .unwrap_or_else(|_| "This computer".into()),
        });
        let folder = root.join(format!(".card-intake-{}", marker.uuid));
        let saved = (|| {
            if initialized {
                // Recheck after the write test, before creating the one marker.
                if !marker_folders(root)?.is_empty() {
                    return Err("The drive changed while checking. Check again.".into());
                }
                fs::create_dir(&folder).map_err(|e| e.to_string())?;
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(folder.join("drive.json"))
                    .map_err(|e| e.to_string())?;
                use std::io::Write;
                file.write_all(&serde_json::to_vec_pretty(&marker).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
                file.sync_all().map_err(|e| e.to_string())?;
            }
            Ok::<_, String>(())
        })();
        match saved {
            Ok(()) => {
                check.add(
                    Status::Pass,
                    if initialized {
                        format!("Initialized: created .card-intake-{}", marker.uuid)
                    } else {
                        format!("Card Intake drive {}", marker.label)
                    },
                    root.display().to_string(),
                );
                drive = Some(Drive {
                    uuid: marker.uuid,
                    label: marker.label,
                    added_at: Utc::now(),
                });
            }
            Err(error) => check.add(Status::Fail, "Couldn't initialize drive", error),
        }
    }
    InitCheck { check, drive }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupResult {
    pub label: String,
    pub uuid: String,
    pub status: ImportStatus,
    pub counts: CopyCounts,
    pub error: Option<String>,
}

pub fn preflight(files: &[CopyFile], target: &Target, cancel: &AtomicBool) -> Result<(), String> {
    target.check()?;
    let facts = offline::disk(&target.root)?;
    preflight_with(
        files,
        &target.folder(),
        facts.free,
        facts.filesystem.as_deref(),
        cancel,
    )
}

pub fn preflight_with(
    files: &[CopyFile],
    folder: &Path,
    free: Option<u64>,
    filesystem: Option<&str>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut missing = Vec::new();
    for file in files {
        copies::cancelled(cancel)?;
        if !copies::existing(file, folder, true, cancel)? {
            missing.push(file);
        }
    }
    copies::check_large(missing.iter().copied(), filesystem)?;
    copies::check_space(
        missing.iter().map(|f| f.size).sum(),
        free,
        "the backup drive",
    )
}

pub fn back_up(
    files: &[CopyFile],
    target: &Target,
    cancel: &AtomicBool,
    mut progress: impl FnMut(ImportProgress),
) -> BackupResult {
    let mut p = copies::Progress::new(files, &target.label);
    let outcome = (|| {
        preflight(files, target, cancel)?;
        for file in files {
            copies::cancelled(cancel)?;
            target.check()?;
            p.emit("backup", file, 0, &mut progress);
            if copies::existing(file, &target.folder(), true, cancel)? {
                p.counts.skipped += 1;
            } else {
                copies::copy(file, &target.folder(), cancel, |_, n| {
                    p.emit("backup", file, n, &mut progress)
                })?;
                p.counts.copied += 1;
            }
            p.counts.verified += 1;
            p.counts.bytes += file.size;
            p.emit("file_complete", file, 0, &mut progress);
        }
        Ok::<_, String>(())
    })();
    BackupResult {
        label: target.label.clone(),
        uuid: target.uuid.clone(),
        status: if outcome.is_ok() {
            ImportStatus::Complete
        } else if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            ImportStatus::Cancelled
        } else {
            ImportStatus::Failed
        },
        counts: p.counts,
        error: outcome.err(),
    }
}

#[cfg(test)]
mod tests;
