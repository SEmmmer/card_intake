//! Shoot folders that already exist on the NAS for a capture date, and which of a
//! card's files are already in them.
//!
//! A date's folders are `<year>/<date>` and `<year>/<date>_<title>`. When one exists,
//! the app offers its title for the shoot, so another card from the same day goes
//! into the same folder. Under a different title, files already in the date's other
//! folders are checked and skipped instead of being copied a second time.

use crate::scan::{MediaFile, MediaKind};
use chrono::NaiveDate;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One existing shoot folder and what's in it, by file name.
#[derive(Debug, Clone)]
pub struct FolderIndex {
    /// The folder's name, e.g. `2026-11-06_夜景`.
    pub name: String,
    pub path: PathBuf,
    modified: Option<SystemTime>,
    /// Lower-case path inside the folder (`dsc00001.arw`, `video/c0001.mp4`) to the
    /// file's size and real path.
    files: HashMap<String, (u64, PathBuf)>,
}

impl FolderIndex {
    fn read(name: String, path: PathBuf) -> Self {
        let mut files = HashMap::new();
        for (prefix, dir) in [("", path.clone()), ("video/", path.join("video"))] {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_file() {
                    let key =
                        format!("{prefix}{}", entry.file_name().to_string_lossy()).to_lowercase();
                    files.insert(key, (meta.len(), entry.path()));
                }
            }
        }
        let modified = fs::metadata(&path).and_then(|m| m.modified()).ok();
        FolderIndex {
            name,
            path,
            modified,
            files,
        }
    }

    /// The file at `inside` (e.g. `video/C0001.MP4`) if it has this size.
    pub fn find(&self, inside: &Path, size: u64) -> Option<&Path> {
        let key = inside.to_string_lossy().replace('\\', "/").to_lowercase();
        self.files
            .get(&key)
            .filter(|(s, _)| *s == size)
            .map(|(_, p)| p.as_path())
    }

    /// The part after `<date>_`, or "" for a folder named just by its date.
    pub fn title(&self) -> &str {
        self.name.get(11..).unwrap_or("")
    }
}

/// The date's existing shoot folders, read once. Links and junctions are left out.
pub fn date_folders(nas_root: &Path, date: NaiveDate) -> Vec<FolderIndex> {
    let year = nas_root.join(date.format("%Y").to_string());
    let prefix = date.format("%Y-%m-%d").to_string();
    let Ok(entries) = fs::read_dir(&year) else {
        return Vec::new();
    };
    let mut folders: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let ours = name == prefix
                || name
                    .strip_prefix(&prefix)
                    .is_some_and(|rest| rest.starts_with('_'));
            let meta = fs::symlink_metadata(entry.path()).ok()?;
            (ours && meta.is_dir() && !is_link(&meta))
                .then(|| FolderIndex::read(name, entry.path()))
        })
        .collect();
    folders.sort_by(|a, b| a.name.cmp(&b.name));
    folders
}

fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

/// Where a card file goes inside its shoot folder.
pub fn inside_shoot(file: &MediaFile) -> PathBuf {
    let name = PathBuf::from(file.path.file_name().unwrap_or_default());
    match file.kind {
        MediaKind::Video => Path::new("video").join(name),
        MediaKind::Photo => name,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ExistingFolder {
    pub name: String,
    pub title: String,
    /// This date's photos and clips from the card already in the folder (same name
    /// and size; the import compares contents before skipping).
    pub photos: usize,
    pub videos: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DateDestination {
    pub date: NaiveDate,
    pub folders: Vec<ExistingFolder>,
    /// The title to offer for this date: the folder holding most of the card's
    /// files, otherwise the newest one. `None` when the date has no folder yet.
    pub suggested_title: Option<String>,
    /// Photos and clips already in any of the date's folders.
    pub photos_on_nas: usize,
    pub videos_on_nas: usize,
    pub photo_bytes_on_nas: u64,
    pub video_bytes_on_nas: u64,
}

/// For each capture date on the card: its existing folders and the files already there.
pub fn find(nas_root: &Path, files: &[MediaFile]) -> Vec<DateDestination> {
    let mut by_date: BTreeMap<NaiveDate, Vec<&MediaFile>> = BTreeMap::new();
    for file in files {
        by_date.entry(file.captured.date()).or_default().push(file);
    }
    by_date
        .into_iter()
        .map(|(date, files)| {
            let indexes = date_folders(nas_root, date);
            let mut result = DateDestination {
                date,
                folders: indexes
                    .iter()
                    .map(|f| ExistingFolder {
                        name: f.name.clone(),
                        title: f.title().to_string(),
                        ..ExistingFolder::default()
                    })
                    .collect(),
                ..DateDestination::default()
            };
            for file in files {
                let inside = inside_shoot(file);
                let mut found = false;
                for (index, folder) in indexes.iter().zip(&mut result.folders) {
                    if index.find(&inside, file.size).is_some() {
                        found = true;
                        match file.kind {
                            MediaKind::Photo => folder.photos += 1,
                            MediaKind::Video => folder.videos += 1,
                        }
                        folder.bytes += file.size;
                    }
                }
                if found {
                    match file.kind {
                        MediaKind::Photo => {
                            result.photos_on_nas += 1;
                            result.photo_bytes_on_nas += file.size;
                        }
                        MediaKind::Video => {
                            result.videos_on_nas += 1;
                            result.video_bytes_on_nas += file.size;
                        }
                    }
                }
            }
            result.suggested_title = indexes
                .iter()
                .zip(&result.folders)
                .max_by_key(|(index, f)| (f.photos + f.videos, index.modified))
                .map(|(_, f)| f.title.clone());
            result
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::TimeSource;

    fn media(path: &str, size: u64, when: &str, kind: MediaKind) -> MediaFile {
        MediaFile {
            path: PathBuf::from(path),
            size,
            captured: chrono::NaiveDateTime::parse_from_str(when, "%Y-%m-%d %H:%M").unwrap(),
            time_source: TimeSource::Exif,
            kind,
        }
    }

    fn put(root: &Path, file: &str, size: usize) {
        let path = root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![0u8; size]).unwrap();
    }

    #[test]
    fn finds_the_dates_folders_and_suggests_the_one_holding_the_card() {
        let nas = tempfile::tempdir().unwrap();
        put(nas.path(), "2026/2026-11-06_夜景/EM_01212.ARW", 10);
        put(nas.path(), "2026/2026-11-06_夜景/video/C0001.MP4", 30);
        put(nas.path(), "2026/2026-11-06_other/EM_01212.ARW", 99); // same name, other size
        put(nas.path(), "2026/2026-11-060/x.ARW", 1); // not this date's folder
        put(nas.path(), "2026/2026-11-03/C0002.MP4", 1);
        let files = [
            media(
                "E:/DCIM/100MSDCF/EM_01212.ARW",
                10,
                "2026-11-06 17:21",
                MediaKind::Photo,
            ),
            media(
                "E:/DCIM/100MSDCF/EM_01213.ARW",
                11,
                "2026-11-06 17:22",
                MediaKind::Photo,
            ),
            media(
                "E:/M4ROOT/CLIP/C0001.MP4",
                30,
                "2026-11-06 18:00",
                MediaKind::Video,
            ),
            media(
                "E:/M4ROOT/CLIP/C0002.MP4",
                5,
                "2026-11-03 16:39",
                MediaKind::Video,
            ),
            media(
                "E:/DCIM/100MSDCF/EM_00001.ARW",
                5,
                "2026-11-01 10:00",
                MediaKind::Photo,
            ),
        ];
        let found = find(nas.path(), &files);
        assert_eq!(found.len(), 3);
        let nov1 = &found[0];
        assert_eq!(
            (nov1.folders.len(), nov1.suggested_title.clone()),
            (0, None)
        );
        let nov3 = &found[1];
        // A bare date folder offers an empty title; the clip there has another size.
        assert_eq!(nov3.suggested_title.as_deref(), Some(""));
        assert_eq!(nov3.videos_on_nas, 0);
        let nov6 = &found[2];
        let names: Vec<_> = nov6.folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["2026-11-06_other", "2026-11-06_夜景"]);
        assert_eq!(nov6.suggested_title.as_deref(), Some("夜景"));
        assert_eq!((nov6.photos_on_nas, nov6.videos_on_nas), (1, 1));
        assert_eq!((nov6.photo_bytes_on_nas, nov6.video_bytes_on_nas), (10, 30));
        assert_eq!((nov6.folders[1].photos, nov6.folders[1].videos), (1, 1));
        assert_eq!(nov6.folders[0].photos, 0);
    }

    #[test]
    fn a_missing_share_or_year_has_no_folders() {
        let files = [media(
            "E:/DCIM/a.ARW",
            1,
            "2026-11-06 10:00",
            MediaKind::Photo,
        )];
        let found = find(Path::new("Z:/does/not/exist"), &files);
        assert_eq!(found[0].folders.len(), 0);
        assert_eq!(found[0].suggested_title, None);
    }
}
