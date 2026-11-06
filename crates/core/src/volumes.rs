//! Finds mounted camera cards: any volume with a `DCIM` folder.
//!
//! Polling the mount points every second or two works the same on Windows,
//! macOS and Linux, with no drivers or system hooks.

use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CardVolume {
    /// Where the card is mounted, e.g. `E:\` or `/Volumes/Untitled`.
    pub root: PathBuf,
    /// Sony bodies keep video clips under `PRIVATE/M4ROOT`.
    pub has_sony_video: bool,
}

pub fn camera_card(root: &Path) -> Option<CardVolume> {
    if !root.join("DCIM").is_dir() {
        return None;
    }
    Some(CardVolume {
        root: root.to_path_buf(),
        has_sony_video: root.join("PRIVATE").join("M4ROOT").is_dir(),
    })
}

/// All mounted volumes that look like camera cards.
pub fn find_camera_cards() -> Vec<CardVolume> {
    mount_points()
        .iter()
        .filter_map(|p| camera_card(p))
        .collect()
}

#[cfg(windows)]
fn mount_points() -> Vec<PathBuf> {
    // Skip A: and B:, which can stall on legacy floppy drivers.
    (b'C'..=b'Z')
        .map(|letter| PathBuf::from(format!("{}:\\", letter as char)))
        .filter(|p| p.exists())
        .collect()
}

#[cfg(target_os = "macos")]
fn mount_points() -> Vec<PathBuf> {
    subdirs(Path::new("/Volumes"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn mount_points() -> Vec<PathBuf> {
    // /media/<user>/<label>, /run/media/<user>/<label> or /mnt/<label>
    let mut points = Vec::new();
    for base in ["/media", "/run/media", "/mnt"] {
        for dir in subdirs(Path::new(base)) {
            points.extend(subdirs(&dir));
            points.push(dir);
        }
    }
    points
}

#[cfg(unix)]
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_volume_with_dcim_is_a_card() {
        let card = tempfile::tempdir().unwrap();
        assert_eq!(camera_card(card.path()), None);

        std::fs::create_dir_all(card.path().join("DCIM/100MSDCF")).unwrap();
        let found = camera_card(card.path()).unwrap();
        assert!(!found.has_sony_video);

        std::fs::create_dir_all(card.path().join("PRIVATE/M4ROOT/CLIP")).unwrap();
        assert!(camera_card(card.path()).unwrap().has_sony_video);
    }
}
