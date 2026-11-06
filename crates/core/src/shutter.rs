//! Shutter count history, kept per camera as one reading range per shooting day.
//!
//! The same database lives in two places: a copy in this computer's app data, and the
//! shared one in the photo share (`.card-intake/shutter-counts.json`), which every
//! computer running Card Intake reads and adds to. Merging is idempotent (per day it
//! keeps the lowest and highest count), so two computers can add the same photos
//! without counting them twice, and any copy can be merged into any other.

use crate::cameras;
use crate::exiftool::{self, ShutterSample};
use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Folder in the photo share for Card Intake's shared files.
pub const SHARED_DIR: &str = ".card-intake";
pub const SHARED_FILE: &str = "shutter-counts.json";
/// Format version this build writes. A newer file on the NAS is never overwritten.
pub const VERSION: u32 = 2;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ShutterDb {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub cameras: Vec<CameraHistory>,
    /// Photo-share folders already read, by path relative to the share with `/`
    /// separators. A folder is read again only when its photos change.
    #[serde(default)]
    pub folders: BTreeMap<String, FolderStamp>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraHistory {
    pub model: String,
    pub serial: Option<String>,
    /// Sony's internal serial number, which ties photos without the regular serial
    /// number to the right camera.
    #[serde(default)]
    pub internal_serial: Option<String>,
    /// One entry per shooting day, oldest first.
    pub days: Vec<DayCounts>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayCounts {
    pub date: NaiveDate,
    /// Lowest and highest shutter count among the day's photos.
    pub min: u64,
    pub max: u64,
    #[serde(default)]
    pub electronic_max: Option<u64>,
    pub first_shot: NaiveDateTime,
    pub last_shot: NaiveDateTime,
    /// Firmware of the day's last photo.
    #[serde(default)]
    pub firmware: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderStamp {
    pub photos: u64,
    pub bytes: u64,
    /// Newest modification time among the photos, in seconds since 1970.
    pub modified: i64,
}

impl DayCounts {
    fn merge(&mut self, other: &DayCounts) {
        if other.last_shot > self.last_shot {
            self.firmware = other.firmware.clone().or(self.firmware.take());
        }
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.electronic_max = self.electronic_max.max(other.electronic_max);
        self.first_shot = self.first_shot.min(other.first_shot);
        self.last_shot = self.last_shot.max(other.last_shot);
    }
}

impl CameraHistory {
    fn add_day(&mut self, day: &DayCounts) {
        match self.days.binary_search_by_key(&day.date, |d| d.date) {
            Ok(i) => self.days[i].merge(day),
            Err(i) => self.days.insert(i, day.clone()),
        }
    }
}

impl ShutterDb {
    fn camera(&mut self, model: &str, serial: Option<&str>) -> &mut CameraHistory {
        let key = |c: &CameraHistory| (c.model.clone(), c.serial.clone());
        let wanted = (model.to_string(), serial.map(str::to_string));
        match self.cameras.binary_search_by(|c| key(c).cmp(&wanted)) {
            Ok(i) => &mut self.cameras[i],
            Err(i) => {
                self.cameras.insert(
                    i,
                    CameraHistory {
                        model: wanted.0,
                        serial: wanted.1,
                        internal_serial: None,
                        days: Vec::new(),
                    },
                );
                &mut self.cameras[i]
            }
        }
    }

    pub fn add(&mut self, s: &ShutterSample) {
        let day = DayCounts {
            date: s.taken.date(),
            min: s.count,
            max: s.count,
            electronic_max: s.electronic_count,
            first_shot: s.taken,
            last_shot: s.taken,
            firmware: s.firmware.clone(),
        };
        // A photo without the regular serial number belongs to the camera that has
        // the same internal one.
        let serial = s.serial.clone().or_else(|| {
            let internal = s.internal_serial.as_ref()?;
            self.cameras
                .iter()
                .find(|c| c.model == s.model && c.internal_serial.as_ref() == Some(internal))
                .and_then(|c| c.serial.clone())
        });
        let camera = self.camera(&s.model, serial.as_deref());
        if camera.internal_serial.is_none() {
            camera.internal_serial = s.internal_serial.clone();
        }
        camera.add_day(&day);
        self.fold_cameras_without_serial();
    }

    /// Moves the days of a camera recorded without its serial number into the camera
    /// with the same model and internal serial, once that's known.
    fn fold_cameras_without_serial(&mut self) {
        let mut i = 0;
        while i < self.cameras.len() {
            let c = &self.cameras[i];
            let target = match (&c.serial, &c.internal_serial) {
                (None, Some(internal)) => self.cameras.iter().position(|o| {
                    o.serial.is_some()
                        && o.model == c.model
                        && o.internal_serial.as_ref() == Some(internal)
                }),
                _ => None,
            };
            match target {
                Some(t) => {
                    let orphan = self.cameras.remove(i);
                    let t = if t > i { t - 1 } else { t };
                    for day in &orphan.days {
                        self.cameras[t].add_day(day);
                    }
                }
                None => i += 1,
            }
        }
    }

    pub fn merge(&mut self, other: &ShutterDb) {
        self.version = self.version.max(other.version);
        for camera in &other.cameras {
            let mine = self.camera(&camera.model, camera.serial.as_deref());
            if mine.internal_serial.is_none() {
                mine.internal_serial = camera.internal_serial.clone();
            }
            for day in &camera.days {
                mine.add_day(day);
            }
        }
        self.fold_cameras_without_serial();
        for (folder, stamp) in &other.folders {
            let newer = self
                .folders
                .get(folder)
                .is_none_or(|mine| (stamp.modified, stamp.photos) > (mine.modified, mine.photos));
            if newer {
                self.folders.insert(folder.clone(), *stamp);
            }
        }
    }

    /// Loads a database, or an empty one when the file doesn't exist.
    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let loaded: ShutterDb = serde_json::from_slice(&bytes).map_err(|e| {
                    io::Error::other(format!(
                        "{} is damaged ({e}). Move it aside to start a new one.",
                        path.display()
                    ))
                })?;
                // Rebuild through merge so the lists are sorted even if the file was edited.
                let mut db = ShutterDb {
                    version: loaded.version,
                    ..ShutterDb::default()
                };
                db.merge(&loaded);
                // Re-read old folder snapshots once to collect electronic counters.
                if loaded.version < 2 {
                    db.folders.clear();
                }
                Ok(db)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Writes a temporary file next to the database and renames it into place, so
    /// readers on other computers never see half a file.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut copy = self.clone();
        copy.version = copy.version.max(VERSION);
        let text = serde_json::to_vec_pretty(&copy).map_err(io::Error::other)?;
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }

    /// Cameras for display, the most recently used first.
    pub fn views(&self) -> Vec<CameraView> {
        let mut views: Vec<_> = self
            .cameras
            .iter()
            .filter_map(|c| {
                let last = c.days.last()?;
                let info = cameras::model_info(&c.model);
                Some(CameraView {
                    model: c.model.clone(),
                    name: info.map(|i| i.name),
                    serial: c.serial.clone(),
                    firmware: c.days.iter().rev().find_map(|d| d.firmware.clone()),
                    rated_shutter: info.and_then(|i| i.rated_shutter),
                    rating_source: info.and_then(|i| i.rating_source),
                    shutter_reference: info.and_then(|i| i.shutter_reference),
                    count: last.max,
                    electronic_count: c.days.iter().filter_map(|d| d.electronic_max).max(),
                    last_shot: last.last_shot,
                    days: c.days.clone(),
                })
            })
            .collect();
        views.sort_by_key(|v| std::cmp::Reverse(v.last_shot));
        views
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CameraView {
    pub model: String,
    /// The name on the body, e.g. "α7R VI".
    pub name: Option<&'static str>,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    /// The maker's official shutter rating, when it published one.
    pub rated_shutter: Option<u64>,
    pub rating_source: Option<&'static str>,
    /// The highest shutter count seen.
    pub count: u64,
    pub electronic_count: Option<u64>,
    /// A user-suggested comparison value, not a manufacturer durability rating.
    pub shutter_reference: Option<u64>,
    pub last_shot: NaiveDateTime,
    pub days: Vec<DayCounts>,
}

/// The shared database's path in the photo share.
pub fn shared_path(nas_root: &Path) -> PathBuf {
    nas_root.join(SHARED_DIR).join(SHARED_FILE)
}

/// Merges `local` into the shared database and returns the merged result. Writes only
/// when `local` adds something, under a lock file so two computers don't overwrite
/// each other's additions.
pub fn sync_shared(nas_root: &Path, local: &ShutterDb) -> io::Result<ShutterDb> {
    let path = shared_path(nas_root);
    let current = ShutterDb::load(&path)?;
    let mut merged = current.clone();
    merged.merge(local);
    if merged == current {
        return Ok(merged);
    }
    if current.version > VERSION {
        return Err(io::Error::other(
            "The shutter count database on the NAS was written by a newer Card Intake. Update this app to add to it.",
        ));
    }
    let _lock = Lock::acquire(&path.with_extension("lock"))?;
    // Read again under the lock: another computer may have just added to it.
    let mut merged = ShutterDb::load(&path)?;
    merged.merge(local);
    merged.save(&path)?;
    Ok(merged)
}

/// A lock file; removed when dropped. A lock older than a minute is from a computer
/// that crashed or lost the share, and is taken over.
struct Lock(PathBuf);

impl Lock {
    fn acquire(path: &Path) -> io::Result<Lock> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match std::fs::File::create_new(path) {
                Ok(_) => return Ok(Lock(path.to_path_buf())),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok())
                        .is_some_and(|age| age > Duration::from_secs(60));
                    if stale {
                        let _ = std::fs::remove_file(path);
                        continue;
                    }
                    if std::time::Instant::now() > deadline {
                        return Err(io::Error::other(
                            "Another computer is updating the shutter count database on the NAS. Try again in a minute.",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A folder of photos in the share, and what's in it.
#[derive(Debug, Clone)]
pub struct LibraryFolder {
    /// Relative to the share, with `/` separators.
    pub relative: String,
    pub photos: Vec<PathBuf>,
    pub stamp: FolderStamp,
}

/// Lists the share's photo folders. Skips Card Intake's own and hidden folders,
/// working folders (`_work`, `_final`, `_inbox`, `_catalog`), Synology's `@eaDir` and
/// `#recycle`, and `video` folders.
pub fn list_library(nas_root: &Path, cancel: &AtomicBool) -> io::Result<Vec<LibraryFolder>> {
    let mut folders: BTreeMap<PathBuf, LibraryFolder> = BTreeMap::new();
    let walk = walkdir::WalkDir::new(nas_root)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0
                || !e.file_type().is_dir()
                || !{
                    let name = e.file_name().to_string_lossy();
                    name.starts_with(['.', '_', '@', '#']) || name.eq_ignore_ascii_case("video")
                }
        });
    for entry in walk {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other("Stopped."));
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() || entry.file_name().to_string_lossy().starts_with("._") {
            continue;
        }
        if crate::scan::kind_of(entry.path()) != Some(crate::scan::MediaKind::Photo) {
            continue;
        }
        let Some(parent) = entry.path().parent() else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64);
        let folder = folders
            .entry(parent.to_path_buf())
            .or_insert_with(|| LibraryFolder {
                relative: relative_key(nas_root, parent),
                photos: Vec::new(),
                stamp: FolderStamp {
                    photos: 0,
                    bytes: 0,
                    modified: 0,
                },
            });
        folder.photos.push(entry.into_path());
        folder.stamp.photos += 1;
        folder.stamp.bytes += meta.len();
        folder.stamp.modified = folder.stamp.modified.max(modified);
    }
    Ok(folders.into_values().collect())
}

fn relative_key(root: &Path, folder: &Path) -> String {
    let relative = folder.strip_prefix(root).unwrap_or(folder);
    relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanProgress {
    pub folders_done: usize,
    pub folders_total: usize,
    pub photos_done: usize,
    pub photos_total: usize,
    pub current: Option<String>,
}

/// What one folder produced.
pub enum FolderResult {
    Read {
        folder: LibraryFolder,
        samples: Vec<ShutterSample>,
    },
    Failed {
        folder: LibraryFolder,
        error: String,
    },
}

/// Photos per ExifTool run: enough to spread its start-up time, few enough to show progress.
const BATCH: usize = 250;

/// Reads the shutter counts of the given folders with several ExifTool runs at once.
/// `on_result` and `progress` are called on this thread, in completion order.
pub fn read_folders(
    exiftool: &Path,
    folders: Vec<LibraryFolder>,
    workers: usize,
    cancel: &AtomicBool,
    mut progress: impl FnMut(ScanProgress),
    mut on_result: impl FnMut(FolderResult),
) {
    let folders_total = folders.len();
    let photos_total = folders.iter().map(|f| f.photos.len()).sum();
    let next = AtomicUsize::new(0);
    let (tx, rx) = std::sync::mpsc::channel::<(usize, FolderResult)>();
    let mut done = ScanProgress {
        folders_done: 0,
        folders_total,
        photos_done: 0,
        photos_total,
        current: None,
    };
    progress(done.clone());
    std::thread::scope(|scope| {
        for _ in 0..workers.max(1).min(folders_total.max(1)) {
            let (tx, next, folders) = (tx.clone(), &next, &folders);
            scope.spawn(move || {
                while let Some(folder) = folders.get(next.fetch_add(1, Ordering::Relaxed)) {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let photos = exiftool::one_per_shot(folder.photos.iter().cloned());
                    let mut samples = Vec::new();
                    let mut error = None;
                    for chunk in photos.chunks(BATCH) {
                        if cancel.load(Ordering::Relaxed) {
                            return;
                        }
                        match exiftool::shutter_samples(exiftool, chunk) {
                            Ok(s) => samples.extend(s),
                            Err(e) => {
                                error = Some(e.to_string());
                                break;
                            }
                        }
                    }
                    let photos = folder.photos.len();
                    let result = match error {
                        None => FolderResult::Read {
                            folder: folder.clone(),
                            samples,
                        },
                        Some(error) => FolderResult::Failed {
                            folder: folder.clone(),
                            error,
                        },
                    };
                    if tx.send((photos, result)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(tx);
        for (photos, result) in rx {
            done.folders_done += 1;
            done.photos_done += photos;
            done.current = Some(match &result {
                FolderResult::Read { folder, .. } | FolderResult::Failed { folder, .. } => {
                    folder.relative.clone()
                }
            });
            on_result(result);
            progress(done.clone());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    fn sample(count: u64, taken: &str) -> ShutterSample {
        ShutterSample {
            model: "ILCE-7RM6".into(),
            serial: Some("03506694".into()),
            internal_serial: Some("73 255 0 0 25 9".into()),
            firmware: Some("1.01".into()),
            count,
            electronic_count: None,
            taken: at(taken),
        }
    }

    #[test]
    fn electronic_counts_merge_and_old_history_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("counts.json");
        let mut db = ShutterDb::default();
        db.add(&ShutterSample {
            electronic_count: Some(1234),
            ..sample(0, "2026-11-07 10:00")
        });
        db.add(&sample(1, "2026-11-07 11:00"));
        db.add(&ShutterSample {
            electronic_count: Some(1200),
            ..sample(0, "2026-11-07 09:00")
        });
        db.save(&path).unwrap();
        let loaded = ShutterDb::load(&path).unwrap();
        assert_eq!(loaded.views()[0].electronic_count, Some(1234));
        assert_eq!(loaded.views()[0].count, 1);
        let mut old = serde_json::to_value(&loaded).unwrap();
        old["version"] = 1.into();
        old["cameras"][0]["days"][0]
            .as_object_mut()
            .unwrap()
            .remove("electronic_max");
        old["folders"] =
            serde_json::json!({ "2026/shoot": { "photos": 1, "bytes": 100, "modified": 200 } });
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        let migrated = ShutterDb::load(&path).unwrap();
        assert_eq!(migrated.views()[0].count, 1);
        assert_eq!(migrated.views()[0].electronic_count, None);
        assert!(migrated.folders.is_empty());
    }

    #[test]
    fn keeps_one_range_per_day_and_merging_twice_changes_nothing() {
        let mut db = ShutterDb::default();
        for (count, taken) in [
            (700, "2026-11-06 19:00"),
            (650, "2026-11-06 17:00"),
            (894, "2026-11-07 10:00"),
        ] {
            db.add(&sample(count, taken));
        }
        assert_eq!(db.cameras.len(), 1);
        let days = &db.cameras[0].days;
        assert_eq!(days.len(), 2);
        assert_eq!((days[0].min, days[0].max), (650, 700));
        assert_eq!(days[0].first_shot, at("2026-11-06 17:00"));

        let mut other = ShutterDb::default();
        other.add(&sample(600, "2026-11-06 12:00"));
        other.add(&ShutterSample {
            model: "ILCE-1".into(),
            ..sample(5, "2026-10-01 09:00")
        });
        db.merge(&other);
        let once = db.clone();
        db.merge(&other);
        db.merge(&once);
        assert_eq!(db, once);
        assert_eq!(db.cameras.len(), 2);
        let a7r6 = db.cameras.iter().find(|c| c.model == "ILCE-7RM6").unwrap();
        assert_eq!(a7r6.days[0].min, 600);

        let views = db.views();
        assert_eq!(views[0].model, "ILCE-7RM6");
        assert_eq!(views[0].count, 894);
        assert_eq!(views[0].name, Some("α7R VI"));
        assert_eq!(views[0].rated_shutter, None);
        assert_eq!(views[1].rated_shutter, Some(500_000));
    }

    #[test]
    fn photos_without_a_serial_join_their_camera_by_internal_serial() {
        let no_serial = |count, taken| ShutterSample {
            serial: None,
            ..sample(count, taken)
        };
        // Serial-less photos first: kept apart until the camera's serial shows up.
        let mut db = ShutterDb::default();
        db.add(&no_serial(2, "2026-10-30 17:54"));
        assert_eq!(db.cameras.len(), 1);
        db.add(&sample(30, "2026-10-30 23:03"));
        db.add(&no_serial(18, "2026-10-30 20:47"));
        assert_eq!(db.cameras.len(), 1, "{:?}", db.cameras);
        assert_eq!(db.cameras[0].serial.as_deref(), Some("03506694"));
        assert_eq!(
            (db.cameras[0].days[0].min, db.cameras[0].days[0].max),
            (2, 30)
        );

        // The same when two databases meet.
        let mut a = ShutterDb::default();
        a.add(&no_serial(2, "2026-10-30 17:54"));
        let mut b = ShutterDb::default();
        b.add(&sample(30, "2026-10-30 23:03"));
        a.merge(&b);
        assert_eq!(a.cameras.len(), 1);
        assert_eq!(a.cameras[0].days[0].min, 2);
        // Another body of the same model stays separate.
        a.add(&ShutterSample {
            serial: None,
            internal_serial: Some("1 2 3".into()),
            ..sample(5, "2026-10-30 10:00")
        });
        assert_eq!(a.cameras.len(), 2);
    }

    #[test]
    fn syncs_with_the_shared_file_and_writes_only_when_something_is_new() {
        let dir = tempfile::tempdir().unwrap();
        let mut pc = ShutterDb::default();
        pc.add(&sample(700, "2026-11-06 19:00"));
        let shared = sync_shared(dir.path(), &pc).unwrap();
        assert_eq!(shared.cameras[0].days[0].max, 700);
        let path = shared_path(dir.path());
        assert_eq!(ShutterDb::load(&path).unwrap().version, VERSION);

        // A Mac adds another day; the PC's next sync brings it in.
        let mut mac = ShutterDb::default();
        mac.add(&sample(894, "2026-11-07 10:00"));
        sync_shared(dir.path(), &mac).unwrap();
        let both = sync_shared(dir.path(), &pc).unwrap();
        assert_eq!(both.cameras[0].days.len(), 2);

        // Nothing new: the file isn't rewritten.
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        sync_shared(dir.path(), &both).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
        assert!(!path.with_extension("lock").exists());
    }

    #[test]
    fn refuses_a_damaged_or_newer_shared_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = shared_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        let mut pc = ShutterDb::default();
        pc.add(&sample(700, "2026-11-06 19:00"));
        assert!(
            sync_shared(dir.path(), &pc)
                .unwrap_err()
                .to_string()
                .contains("damaged")
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"{not json");

        std::fs::write(&path, br#"{"version": 99, "cameras": []}"#).unwrap();
        assert!(
            sync_shared(dir.path(), &pc)
                .unwrap_err()
                .to_string()
                .contains("newer")
        );
    }

    #[test]
    fn a_stale_lock_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("x.lock");
        std::fs::write(&lock, b"").unwrap();
        let old = SystemTime::now() - Duration::from_secs(120);
        std::fs::File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let held = Lock::acquire(&lock).unwrap();
        drop(held);
        assert!(!lock.exists());
    }

    #[test]
    fn lists_photo_folders_and_skips_working_folders() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for file in [
            "2026/2026-11-06_夜景/EM_01212.ARW",
            "2026/2026-11-06_夜景/EM_01212.JPG",
            "2026/2026-11-06_夜景/video/C0001.MP4",
            "2026/2026-11-06_夜景/_final/EM_01212.jpg",
            "2026/2026-11-06_夜景/EM_01212.xmp",
            ".card-intake/x.jpg",
            "@eaDir/thumb.jpg",
            "2026/2026-10-29/DSC00001.ARW",
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"photo").unwrap();
        }
        let folders = list_library(root, &AtomicBool::new(false)).unwrap();
        let names: Vec<_> = folders.iter().map(|f| f.relative.as_str()).collect();
        assert_eq!(names, ["2026/2026-10-29", "2026/2026-11-06_夜景"]);
        assert_eq!(folders[1].stamp.photos, 2);
        assert_eq!(folders[1].stamp.bytes, 10);
    }
}
