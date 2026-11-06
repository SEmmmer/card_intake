//! Lists the photos and videos on a camera card.

use chrono::{DateTime, Local, NaiveDateTime};
use serde::Serialize;
use std::io;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Photo,
    Video,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MediaFile {
    pub path: PathBuf,
    pub size: u64,
    /// When the file was captured. For now this is the file's modified time, which
    /// cameras set to the moment of capture. EXIF `DateTimeOriginal` (via exiftool)
    /// will replace it.
    pub captured: NaiveDateTime,
    pub kind: MediaKind,
}

const PHOTO_EXTENSIONS: &[&str] = &["arw", "dng", "jpg", "jpeg", "hif", "heic"];
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mov", "mts"];

/// Photo, video, or `None` for anything else (XML sidecars, thumbnails, databases).
pub fn kind_of(path: &Path) -> Option<MediaKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if PHOTO_EXTENSIONS.contains(&ext.as_str()) {
        Some(MediaKind::Photo)
    } else if VIDEO_EXTENSIONS.contains(&ext.as_str()) {
        Some(MediaKind::Video)
    } else {
        None
    }
}

/// Lists photos under `DCIM` and Sony videos under `PRIVATE/M4ROOT/CLIP`, oldest first.
///
/// Skips the `._` files macOS leaves on exFAT cards.
pub fn scan_card(root: &Path) -> io::Result<Vec<MediaFile>> {
    let mut files = Vec::new();
    let sources = [
        root.join("DCIM"),
        root.join("PRIVATE").join("M4ROOT").join("CLIP"),
    ];
    for dir in sources.iter().filter(|d| d.is_dir()) {
        for entry in WalkDir::new(dir) {
            let entry = entry?;
            if !entry.file_type().is_file() || entry.file_name().to_string_lossy().starts_with("._")
            {
                continue;
            }
            let Some(kind) = kind_of(entry.path()) else {
                continue;
            };
            let meta = entry.metadata()?;
            let captured = DateTime::<Local>::from(meta.modified()?).naive_local();
            files.push(MediaFile {
                path: entry.into_path(),
                size: meta.len(),
                captured,
                kind,
            });
        }
    }
    files.sort_by(|a, b| {
        a.captured
            .cmp(&b.captured)
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::time::{Duration, SystemTime};

    fn touch(path: &Path, bytes: usize, age_secs: u64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![0u8; bytes]).unwrap();
        let when = SystemTime::now() - Duration::from_secs(age_secs);
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    #[test]
    fn finds_photos_and_videos_and_skips_the_rest() {
        let card = tempfile::tempdir().unwrap();
        let root = card.path();
        touch(&root.join("DCIM/100MSDCF/DSC03519.ARW"), 10, 60);
        touch(&root.join("DCIM/100MSDCF/DSC03518.ARW"), 20, 120);
        touch(&root.join("DCIM/100MSDCF/._DSC03518.ARW"), 4, 120);
        touch(&root.join("DCIM/100MSDCF/notes.txt"), 4, 30);
        touch(&root.join("PRIVATE/M4ROOT/CLIP/C0001.MP4"), 30, 10);
        touch(&root.join("PRIVATE/M4ROOT/CLIP/C0001M01.XML"), 4, 10);

        let files = scan_card(root).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert_eq!(names, ["DSC03518.ARW", "DSC03519.ARW", "C0001.MP4"]);
        assert_eq!(files[0].size, 20);
        assert_eq!(files[2].kind, MediaKind::Video);
    }

    #[test]
    fn empty_card_has_no_files() {
        let card = tempfile::tempdir().unwrap();
        assert!(scan_card(card.path()).unwrap().is_empty());
    }

    #[test]
    fn kind_ignores_case() {
        assert_eq!(kind_of(Path::new("a/DSC00001.ARW")), Some(MediaKind::Photo));
        assert_eq!(kind_of(Path::new("a/c0001.mp4")), Some(MediaKind::Video));
        assert_eq!(kind_of(Path::new("a/AVIN0001.BNP")), None);
    }
}
