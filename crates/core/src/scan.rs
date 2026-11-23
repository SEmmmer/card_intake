//! Lists the photos and videos on a camera card.
//!
//! Sony cards are read from `DCIM` and `M4ROOT/CLIP`; other makers, including Nikon
//! (whose Z 9 cards keep photos and videos in a folder such as `DCIM/100NCZ_9`), are
//! read from the whole `DCIM` tree. Nikon raws are `.NEF` (`.NRW` on older bodies);
//! Hasselblad's (the X2D II, in `DCIM/118HASBL`) are `.3FR`.

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

const PHOTO_EXTENSIONS: &[&str] = &[
    "arw", "nef", "nrw", "3fr", "dng", "jpg", "jpeg", "hif", "heic",
];
// "nev" is Nikon's N-RAW video, which a Z 9 writes instead of, or beside, an MP4.
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mov", "mts", "nev"];

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

/// Proprietary raws use XMP sidecars; DNG, JPEG and HEIF store XMP in the photo.
pub fn uses_raw_sidecar(path: &Path) -> bool {
    kind_of(path) == Some(MediaKind::Photo)
        && path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
            !["dng", "jpg", "jpeg", "hif", "heic", "heif"]
                .iter()
                .any(|other| e.eq_ignore_ascii_case(other))
        })
}

/// Folders that hold the originals. Thumbnails (`M4ROOT/THMBNL`) are left out, and so
/// are proxies (`M4ROOT/SUB`): an import copies each one next to its clip instead
/// (see [`crate::video`]).
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

/// Refuses a card whose photos don't hold what their names say. Windows can show the
/// folders of the card that was in the reader before over the new card's data: every
/// name and size is the old card's, and the bytes are whatever the new card has there.
/// A copy of that would pass its read-back, so it must not start.
pub fn check_content(files: &[MediaFile]) -> Result<(), String> {
    let wrong: Vec<&MediaFile> = files
        .iter()
        .filter(|f| f.kind == MediaKind::Photo && !exif::content_matches(&f.path).unwrap_or(true))
        .collect();
    let Some(first) = wrong.first() else {
        return Ok(());
    };
    let name = first.path.file_name().unwrap_or_default().to_string_lossy();
    Err(format!(
        "{name}{} on this card {} what the name says. Windows may still be showing the \
         folders of the card that was in this reader before. Nothing was copied. Take the \
         card out, wait a few seconds, and put it back in.",
        match wrong.len() - 1 {
            0 => String::new(),
            1 => " and 1 other photo".into(),
            more => format!(" and {more} other photos"),
        },
        if wrong.len() == 1 {
            "doesn't hold"
        } else {
            "don't hold"
        },
    ))
}

/// Said when a card was only now shown with its own folders: whatever was chosen from
/// the ones before (shoots, names) was chosen from another card's.
pub const SHOWN_AFRESH: &str = "Windows was showing another card's folders for the card \
     in this reader, and now shows its own. Nothing was copied. Check its shoots, then \
     import again.";

/// Makes sure Windows shows the card in the reader at `root`, not the one that was in
/// it before.
///
/// A reader that doesn't tell Windows a card was swapped leaves the old card's file
/// system mounted: the old card's names, sizes and volume ID over the new card's
/// bytes. That happens to a card swapped for another (the second card of a camera
/// that records to both), and to a card formatted in the camera and put back. Copies
/// made then would hold the wrong bytes under the right names, and pass their
/// read-back. The card's own boot sector tells: its volume ID differs from the one
/// Windows shows. Windows is then made to read the card afresh, as on insertion.
///
/// `Ok(true)` when it was showing another card and now shows this one; an error when
/// it still doesn't. Cards whose first sector Windows doesn't let the app read are
/// left to the other checks (see [`check_content`]).
pub fn check_card_shows_itself(root: &Path) -> Result<bool, String> {
    shows_itself(
        || crate::volume_info::volume_info(root).id,
        || crate::card_details::volume_serial_on_card(root),
        || crate::eject::remount_card(root),
    )
}

/// [`check_card_shows_itself`] with what Windows shows, what the card holds and the
/// remount passed in.
fn shows_itself(
    shown: impl Fn() -> Option<String>,
    own: impl Fn() -> Option<String>,
    remount: impl FnOnce() -> Result<(), String>,
) -> Result<bool, String> {
    let differs = || match (shown(), own()) {
        (Some(shown), Some(own)) if shown != own => Some((shown, own)),
        _ => None,
    };
    let Some((shown, own)) = differs() else {
        return Ok(false);
    };
    let why = match remount() {
        Ok(()) => match differs() {
            None => return Ok(true),
            Some((still, _)) => format!("it still shows volume {still}"),
        },
        Err(why) => why,
    };
    Err(format!(
        "Windows is showing the folders of another card (volume {shown}) for the card in \
         this reader (volume {own}), and couldn't be made to read this card again: {why}. \
         Nothing was read or copied. Close any window or app that has the card open and \
         try again, or take the card out, wait a few seconds, and put it back in."
    ))
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

pub(crate) fn sidecar_path(clip: &Path) -> Option<PathBuf> {
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

    #[test]
    fn only_proprietary_raw_photos_use_sidecar_ratings() {
        for ext in PHOTO_EXTENSIONS {
            let path = PathBuf::from(format!("photo.{}", ext.to_uppercase()));
            assert_eq!(
                uses_raw_sidecar(&path),
                ["arw", "nef", "nrw", "3fr"].contains(ext)
            );
        }
        for name in ["clip.MP4", "clip.NEV", "photo.xmp", "photo.xml"] {
            assert!(!uses_raw_sidecar(Path::new(name)));
        }
    }

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
    fn reads_a_nikon_z_9_card() {
        let card = tempfile::tempdir().unwrap();
        let root = card.path();
        let nef = crate::exif::tests::nikon_sample();
        // Nikon keeps photos and clips in its own folder name inside DCIM ("NCZ_9"
        // by default on a Z 9), not in Sony's DSC- or clip folders.
        write(&root.join("DCIM/100NCZ_9/DSC_0001.NEF"), &nef, 5);
        write(&root.join("DCIM/100NCZ_9/DSC_0001.JPG"), &[0; 7], 5);
        write(&root.join("DCIM/100NCZ_9/DSC_0001.xmp"), b"edits", 5);
        write(&root.join("DCIM/100NCZ_9/DSC_0100.MOV"), &[0; 11], 3);

        let files = scan_card(root).unwrap();
        let mut found: Vec<_> = files
            .iter()
            .map(|f| {
                (
                    f.path.file_name().unwrap().to_string_lossy().into_owned(),
                    f.kind,
                    f.time_source,
                    f.size,
                )
            })
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            found,
            [
                (
                    "DSC_0001.JPG".to_string(),
                    MediaKind::Photo,
                    TimeSource::FileTime,
                    7
                ),
                (
                    "DSC_0001.NEF".to_string(),
                    MediaKind::Photo,
                    TimeSource::Exif,
                    nef.len() as u64
                ),
                (
                    "DSC_0100.MOV".to_string(),
                    MediaKind::Video,
                    TimeSource::FileTime,
                    11
                ),
            ]
        );
        // The NEF's own capture time, so the shoot lands on the right day.
        let raw = files
            .iter()
            .find(|f| f.path.extension().unwrap().eq_ignore_ascii_case("NEF"))
            .unwrap();
        assert_eq!(raw.captured.to_string(), "2026-11-07 09:15:22");
    }

    #[test]
    fn a_card_shown_with_another_cards_file_system_is_read_again() {
        use std::cell::{Cell, RefCell};
        let id = |text: &str| Some(text.to_string());
        let never = || -> Result<(), String> { panic!("a card showing itself was remounted") };
        // Windows and the card agree, or one of them can't say: nothing to do.
        assert_eq!(
            shows_itself(|| id("72A5-5856"), || id("72A5-5856"), never),
            Ok(false)
        );
        assert_eq!(shows_itself(|| id("02BE-8994"), || None, never), Ok(false));
        assert_eq!(shows_itself(|| None, || id("72A5-5856"), never), Ok(false));

        // 2026-11-23: the second card in, Windows still on the first one's volume. Read
        // afresh, Windows shows the card's own.
        let shown = RefCell::new("02BE-8994");
        let remounts = Cell::new(0);
        let remount = || {
            remounts.set(remounts.get() + 1);
            *shown.borrow_mut() = "72A5-5856";
            Ok(())
        };
        let fixed = shows_itself(|| id(*shown.borrow()), || id("72A5-5856"), remount);
        assert_eq!((fixed, remounts.get()), (Ok(true), 1));

        // Another app has the card open, so Windows can't let go of the old volume.
        let busy = shows_itself(
            || id("02BE-8994"),
            || id("72A5-5856"),
            || Err("another app may have the card open: Access is denied.".into()),
        )
        .unwrap_err();
        assert!(
            busy.starts_with(
                "Windows is showing the folders of another card (volume 02BE-8994) for the \
                 card in this reader (volume 72A5-5856)"
            ),
            "{busy}"
        );
        assert!(
            busy.contains("another app may have the card open"),
            "{busy}"
        );
        assert!(busy.contains("Nothing was read or copied"), "{busy}");

        // Read afresh and still the other volume: refused all the same.
        let stuck = shows_itself(|| id("02BE-8994"), || id("72A5-5856"), || Ok(())).unwrap_err();
        assert!(stuck.contains("it still shows volume 02BE-8994"), "{stuck}");
    }

    #[test]
    fn a_folder_that_isnt_a_drive_root_is_never_remounted() {
        let card = tempfile::tempdir().unwrap();
        assert_eq!(check_card_shows_itself(card.path()), Ok(false));
    }

    #[test]
    fn a_card_showing_another_cards_folders_is_refused() {
        let card = tempfile::tempdir().unwrap();
        let root = card.path();
        let raw = crate::exif::tests::sample(true);
        write(&root.join("DCIM/100MSDCF/DSC00001.ARW"), &raw, 60);
        write(
            &root.join("DCIM/100MSDCF/DSC00001.HIF"),
            &crate::exif::tests::heif(&raw),
            60,
        );
        write(&root.join("M4ROOT/CLIP/C0001.MP4"), b"clip", 60);
        assert_eq!(check_content(&scan_card(root).unwrap()), Ok(()));
        // The same names over another card's bytes.
        write(&root.join("DCIM/100MSDCF/DSC00002.ARW"), &[0x7D; 64], 50);
        let one = check_content(&scan_card(root).unwrap()).unwrap_err();
        assert!(
            one.starts_with("DSC00002.ARW on this card doesn't hold"),
            "{one}"
        );
        write(&root.join("DCIM/100MSDCF/DSC00002.HIF"), &raw, 50);
        write(&root.join("DCIM/100MSDCF/DSC00003.JPG"), &raw, 40);
        let more = check_content(&scan_card(root).unwrap()).unwrap_err();
        assert!(
            more.contains("and 2 other photos on this card don't hold"),
            "{more}"
        );
        assert!(more.contains("Nothing was copied"), "{more}");
    }

    #[test]
    fn a_heif_photo_is_dated_by_its_exif() {
        let card = tempfile::tempdir().unwrap();
        let photo = card.path().join("DCIM/100MSDCF/DSC00001.HIF");
        write(
            &photo,
            &crate::exif::tests::heif(&crate::exif::tests::sample(true)),
            60,
        );
        let files = scan_card(card.path()).unwrap();
        assert_eq!(files[0].time_source, TimeSource::Exif);
        assert_eq!(files[0].captured.to_string(), "2026-11-06 17:21:04");
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
        assert_eq!(kind_of(Path::new("a/DSC_0001.NEF")), Some(MediaKind::Photo));
        assert_eq!(kind_of(Path::new("a/SLOW.NRW")), Some(MediaKind::Photo));
        assert_eq!(kind_of(Path::new("a/B0030873.3FR")), Some(MediaKind::Photo));
        assert_eq!(kind_of(Path::new("a/c0001.mp4")), Some(MediaKind::Video));
        assert_eq!(kind_of(Path::new("a/DSC_0001.NEV")), Some(MediaKind::Video));
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
