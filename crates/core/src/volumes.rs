//! Finds mounted camera cards with DCIM or Sony M4ROOT/CLIP folders.
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
    let has_sony_video =
        root.join("M4ROOT/CLIP").is_dir() || root.join("PRIVATE/M4ROOT/CLIP").is_dir();
    if !root.join("DCIM").is_dir() && !has_sony_video {
        return None;
    }
    Some(CardVolume {
        root: root.to_path_buf(),
        has_sony_video,
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
pub fn mount_points() -> Vec<PathBuf> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    // SAFETY: GetLogicalDrives has no arguments.
    let drives = unsafe { GetLogicalDrives() };
    // Skip A: and B:, which can stall on legacy floppy drivers.
    (b'C'..=b'Z')
        .filter(|letter| drives & (1 << (letter - b'A')) != 0)
        .map(|letter| PathBuf::from(format!("{}:\\", letter as char)))
        .filter(|p| {
            let root: Vec<u16> = p.to_string_lossy().encode_utf16().chain(Some(0)).collect();
            // SAFETY: root is a NUL-terminated drive root.
            // CFexpress readers may report fixed drives (3), SD readers removable (2).
            matches!(unsafe { GetDriveTypeW(root.as_ptr()) }, 2 | 3)
        })
        .collect()
}

#[cfg(target_os = "macos")]
pub fn mount_points() -> Vec<PathBuf> {
    subdirs(Path::new("/Volumes"))
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn mount_points() -> Vec<PathBuf> {
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

/// The existing volume holding a path. Callers resolve missing folders through
/// their nearest existing parent before asking, so a disconnected mount fails.
#[cfg(windows)]
pub fn volume_root(path: &Path) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;
    let resolved = std::fs::canonicalize(path).ok()?;
    let wide: Vec<u16> = resolved.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut root = vec![0u16; 32768];
    // SAFETY: wide is terminated, and root is a live output buffer.
    if unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return None;
    }
    let end = root.iter().position(|&c| c == 0)?;
    let text = String::from_utf16_lossy(&root[..end]);
    Some(PathBuf::from(
        text.strip_prefix(r"\\?\")
            .filter(|s| !s.starts_with("UNC\\"))
            .unwrap_or(&text),
    ))
}

#[cfg(unix)]
pub fn volume_root(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let mut root = std::fs::canonicalize(path).ok()?;
    let dev = std::fs::metadata(&root).ok()?.dev();
    while let Some(parent) = root.parent() {
        if std::fs::metadata(parent).ok()?.dev() != dev {
            break;
        }
        root = parent.to_path_buf();
    }
    Some(root)
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

    #[test]
    fn detects_video_only_sony_cards() {
        let card = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(card.path().join("M4ROOT/CLIP")).unwrap();
        assert!(camera_card(card.path()).unwrap().has_sony_video);
    }
}
