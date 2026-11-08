//! Checks local destinations before making folders or writing any photos.

use crate::nas::{CheckItem, Status};
use crate::{backup, copies, nas, volume_info, volumes};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Operating system facts are supplied separately so tests use ordinary folders.
#[derive(Debug, Clone)]
pub struct DiskFacts {
    pub root: PathBuf,
    pub local: bool,
    pub system: bool,
    pub free: Option<u64>,
    pub total: Option<u64>,
    pub filesystem: Option<String>,
    pub label: Option<String>,
}

pub fn existing_parent(path: &Path) -> Result<&Path, String> {
    if !path.is_absolute() {
        return Err("Use a full folder path.".into());
    }
    path.ancestors()
        .find(|p| p.exists())
        .filter(|p| p.is_dir())
        .ok_or_else(|| "The folder's volume isn't connected.".into())
}

pub fn disk(path: &Path) -> Result<DiskFacts, String> {
    let parent = existing_parent(path)?;
    let root =
        volumes::volume_root(parent).ok_or("Couldn't find the volume. Check its connection.")?;
    let info = volume_info::volume_info(&root);
    #[cfg(windows)]
    let system = std::env::var("SystemDrive").ok().is_some_and(|s| {
        std::fs::canonicalize(format!("{s}\\")).ok() == std::fs::canonicalize(&root).ok()
    });
    #[cfg(not(windows))]
    let system = root == Path::new("/");
    Ok(DiskFacts {
        local: matches!(nas::location(parent), Some(nas::Location::Local(_))),
        system,
        free: info.free_bytes,
        total: info.total_bytes,
        filesystem: info.filesystem,
        label: info.name,
        root,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct FolderCheck {
    pub path: PathBuf,
    pub ok: bool,
    pub items: Vec<CheckItem>,
    pub free_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
}

impl FolderCheck {
    pub fn new(path: &Path, facts: Option<&DiskFacts>) -> Self {
        Self {
            path: path.into(),
            ok: true,
            items: Vec::new(),
            free_bytes: facts.and_then(|f| f.free),
            total_bytes: facts.and_then(|f| f.total),
        }
    }
    pub fn add(&mut self, status: Status, title: impl Into<String>, detail: impl Into<String>) {
        if status == Status::Fail {
            self.ok = false;
        }
        self.items.push(CheckItem {
            status,
            title: title.into(),
            detail: detail.into(),
        });
    }
}

pub fn validate_with(path: &Path, facts: &DiskFacts) -> Result<(), String> {
    let parent = existing_parent(path)?;
    if !facts.local {
        return Err("Choose a folder on this computer's own disk, not a network share.".into());
    }
    if volumes::camera_card(&facts.root).is_some()
        || parent
            .ancestors()
            .any(|p| volumes::camera_card(p).is_some())
    {
        return Err("The offline folder can't be on a camera card.".into());
    }
    if !backup::marker_folders(&facts.root)?.is_empty()
        || parent.ancestors().any(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".card-intake-"))
        })
    {
        return Err("The offline folder can't be on a backup drive.".into());
    }
    Ok(())
}

pub fn validate(path: &Path) -> Result<(), String> {
    validate_with(path, &disk(path)?)
}

pub fn check(path: &Path) -> FolderCheck {
    let facts = disk(path);
    let mut check = FolderCheck::new(path, facts.as_ref().ok());
    match facts.and_then(|facts| validate_with(path, &facts).map(|()| facts)) {
        Err(error) => check.add(Status::Fail, "Choose another offline folder", error),
        Ok(facts) => {
            check.add(
                Status::Pass,
                if path.is_dir() {
                    "Folder found"
                } else {
                    "Folder can be created"
                },
                path.display().to_string(),
            );
            check.add(
                Status::Pass,
                "On this computer's own disk",
                "Not a camera card, backup drive or network share.",
            );
            match copies::check_space(0, facts.free, "this computer") {
                Ok(()) => check.add(
                    Status::Pass,
                    "Free space",
                    format!("{} bytes free", facts.free.unwrap_or(0)),
                ),
                Err(error) => check.add(Status::Fail, "Couldn't check free space", error),
            }
        }
    }
    check
}

/// Saving doesn't create the folder. Imports create it only after the local disk
/// has passed these checks, and repeat the checks before each file.
pub fn prepare(path: &Path) -> Result<(), String> {
    validate(path)?;
    std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
    validate(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_network_card_and_backup_volumes_before_creating_anything() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("volume");
        std::fs::create_dir(&root).unwrap();
        let path = root.join("new/offline");
        let mut facts = DiskFacts {
            root: root.clone(),
            local: false,
            system: false,
            free: Some(100),
            total: Some(200),
            filesystem: None,
            label: None,
        };
        assert!(
            validate_with(&path, &facts)
                .unwrap_err()
                .contains("network")
        );
        facts.local = true;
        std::fs::create_dir(root.join("DCIM")).unwrap();
        assert!(
            validate_with(&path, &facts)
                .unwrap_err()
                .contains("camera card")
        );
        std::fs::remove_dir(root.join("DCIM")).unwrap();
        std::fs::create_dir(root.join(".card-intake-marker")).unwrap();
        assert!(
            validate_with(&path, &facts)
                .unwrap_err()
                .contains("backup drive")
        );
        assert!(!path.exists());
    }

    #[test]
    fn missing_local_folder_is_creatable_and_created_only_when_needed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("offline");
        let check = check(&path);
        assert!(check.ok, "{check:?}");
        assert!(!path.exists());
        prepare(&path).unwrap();
        assert!(path.is_dir());
    }
}
