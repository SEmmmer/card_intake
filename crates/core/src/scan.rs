//! Lists the photos and videos on a camera card.

use crate::exif;
use chrono::{DateTime, Local, NaiveDateTime};
use serde::Serialize;
use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Photo,
    Video,
}

/// Where a file's capture time came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeSource {
    /// The photo's EXIF `DateTimeOriginal`.
    Exif,
    /// The `CreationDate` in a Sony clip's XML sidecar.
    Sidecar,
    /// The file's modified time. Least reliable: copying can change it.
    FileTime,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MediaFile {
    pub path: PathBuf,
    pub size: u64,
    /// Camera clock time at capture.
    pub captured: NaiveDateTime,
    pub time_source: TimeSource,
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

/// Folders that hold the originals. Proxies (`M4ROOT/SUB`) and thumbnails
/// (`M4ROOT/THMBNL`) are left out.
fn source_dirs(root: &Path) -> [PathBuf; 3] {
    [
        root.join("DCIM"),
        // Newer Sony bodies (e.g. ILCE-7RM6) put clips at the card's top level...
        root.join("M4ROOT").join("CLIP"),
        // ...older ones under PRIVATE.
        root.join("PRIVATE").join("M4ROOT").join("CLIP"),
    ]
}

/// Lists photos and video clips, oldest first.
///
/// Skips the `._` files macOS leaves on exFAT cards.
pub fn scan_card(root: &Path) -> io::Result<Vec<MediaFile>> {
    if !std::fs::metadata(root)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "The card path is not a directory",
        ));
    }
    let mut files = Vec::new();
    for dir in source_dirs(root).iter().filter(|d| d.is_dir()) {
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
            let (captured, time_source) = capture_time(entry.path(), kind, &meta)?;
            files.push(MediaFile {
                path: entry.into_path(),
                size: meta.len(),
                captured,
                time_source,
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

fn capture_time(
    path: &Path,
    kind: MediaKind,
    meta: &Metadata,
) -> io::Result<(NaiveDateTime, TimeSource)> {
    let found = match kind {
        MediaKind::Photo => exif::read_basic(path)
            .ok()
            .and_then(|e| e.captured)
            .map(|t| (t, TimeSource::Exif)),
        MediaKind::Video => sidecar_creation_date(path).map(|t| (t, TimeSource::Sidecar)),
    };
    match found {
        Some(found) => Ok(found),
        None => {
            let modified = DateTime::<Local>::from(meta.modified()?).naive_local();
            Ok((modified, TimeSource::FileTime))
        }
    }
}

/// Sony writes `<clip>M01.XML` next to each clip, e.g.
/// `<CreationDate value="2026-11-03T16:39:07+08:00"/>`. Returns the camera's local time.
pub fn sidecar_creation_date(clip: &Path) -> Option<NaiveDateTime> {
    let text = std::fs::read_to_string(sidecar_path(clip)?).ok()?;
    let value = xml_attr(&text, "<CreationDate", "value")?;
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|t| t.naive_local())
}

/// The camera serial number from a Sony clip sidecar (`<Device ... serialNo="..."/>`).
pub fn sidecar_serial(clip: &Path) -> Option<String> {
    sidecar_device_attr(clip, "serialNo")
}

/// The camera model from a Sony clip sidecar (`<Device ... modelName="..."/>`).
pub fn sidecar_model(clip: &Path) -> Option<String> {
    sidecar_device_attr(clip, "modelName")
}

fn sidecar_device_attr(clip: &Path, attr: &str) -> Option<String> {
    let text = std::fs::read_to_string(sidecar_path(clip)?).ok()?;
    xml_attr(&text, "<Device ", attr).map(str::to_string)
}

fn sidecar_path(clip: &Path) -> Option<PathBuf> {
    let stem = clip.file_stem()?.to_str()?;
    Some(clip.with_file_name(format!("{stem}M01.XML")))
}

/// Value of `attr` in the first element that starts with `element`. Enough for
/// Sony's flat, machine-written sidecars; not a general XML parser.
fn xml_attr<'a>(text: &'a str, element: &str, attr: &str) -> Option<&'a str> {
    let start = text.find(element)?;
    let end = text[start..].find('>')? + start;
    let tag = &text[start..end];
    let key = format!(" {attr}=\"");
    let value_start = tag.find(&key)? + key.len();
    let value_end = tag[value_start..].find('"')? + value_start;
    Some(&tag[value_start..value_end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::time::{Duration, SystemTime};

    fn write(path: &Path, bytes: &[u8], age_secs: u64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
        let when = SystemTime::now() - Duration::from_secs(age_secs);
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    const SIDECAR: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<NonRealTimeMeta>
  <CreationDate value="2026-11-03T16:39:07+08:00"/>
  <Device manufacturer="Sony" modelName="ILCE-7RM6" serialNo="03506694"/>
</NonRealTimeMeta>"#;

    #[test]
    fn finds_photos_and_videos_and_skips_the_rest() {
        let card = tempfile::tempdir().unwrap();
        let root = card.path();
        write(&root.join("DCIM/100MSDCF/DSC03519.ARW"), &[0; 10], 60);
        write(&root.join("DCIM/100MSDCF/DSC03518.ARW"), &[0; 20], 120);
        write(&root.join("DCIM/100MSDCF/._DSC03518.ARW"), &[0; 4], 120);
        write(&root.join("DCIM/100MSDCF/notes.txt"), &[0; 4], 30);
        write(&root.join("PRIVATE/M4ROOT/CLIP/C0001.MP4"), &[0; 30], 10);
        write(&root.join("PRIVATE/M4ROOT/CLIP/C0001M01.XML"), &[0; 4], 10);

        let files = scan_card(root).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert_eq!(names, ["DSC03518.ARW", "DSC03519.ARW", "C0001.MP4"]);
        assert_eq!(files[0].size, 20);
        assert_eq!(files[0].time_source, TimeSource::FileTime);
        assert_eq!(files[2].kind, MediaKind::Video);
    }

    #[test]
    fn uses_exif_and_sidecar_times_and_top_level_m4root() {
        let card = tempfile::tempdir().unwrap();
        let root = card.path();
        write(
            &root.join("DCIM/10061106/EM_01212.ARW"),
            &crate::exif::tests::sample(true),
            5,
        );
        write(&root.join("M4ROOT/CLIP/20261103_EM_0001.MP4"), &[0; 8], 5);
        write(
            &root.join("M4ROOT/CLIP/20261103_EM_0001M01.XML"),
            SIDECAR.as_bytes(),
            5,
        );
        write(&root.join("M4ROOT/SUB/20261103_EM_0001S03.MP4"), &[0; 8], 5);

        let files = scan_card(root).unwrap();
        assert_eq!(files.len(), 2, "proxy in M4ROOT/SUB is skipped");

        assert_eq!(files[0].kind, MediaKind::Video);
        assert_eq!(files[0].time_source, TimeSource::Sidecar);
        assert_eq!(files[0].captured.to_string(), "2026-11-03 16:39:07");

        assert_eq!(files[1].kind, MediaKind::Photo);
        assert_eq!(files[1].time_source, TimeSource::Exif);
        assert_eq!(files[1].captured.to_string(), "2026-11-06 17:21:04");

        assert_eq!(
            sidecar_serial(&root.join("M4ROOT/CLIP/20261103_EM_0001.MP4")).as_deref(),
            Some("03506694")
        );
    }

    #[test]
    fn empty_card_has_no_files() {
        let card = tempfile::tempdir().unwrap();
        assert!(scan_card(card.path()).unwrap().is_empty());
    }

    #[test]
    fn missing_cards_are_errors_instead_of_empty_scans() {
        let card = tempfile::tempdir().unwrap();
        assert!(scan_card(&card.path().join("removed")).is_err());
    }

    #[test]
    fn kind_ignores_case() {
        assert_eq!(kind_of(Path::new("a/DSC00001.ARW")), Some(MediaKind::Photo));
        assert_eq!(kind_of(Path::new("a/c0001.mp4")), Some(MediaKind::Video));
        assert_eq!(kind_of(Path::new("a/AVIN0001.BNP")), None);
    }

    #[test]
    fn reads_xml_attributes() {
        assert_eq!(
            xml_attr(SIDECAR, "<CreationDate", "value"),
            Some("2026-11-03T16:39:07+08:00")
        );
        assert_eq!(
            xml_attr(SIDECAR, "<Device ", "modelName"),
            Some("ILCE-7RM6")
        );
        assert_eq!(xml_attr(SIDECAR, "<Device ", "missing"), None);
    }
}
