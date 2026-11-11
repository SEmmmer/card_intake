//! Shutter count history, kept per camera as one reading range per shooting day.
//!
//! The history is stored in SQLite (see [`store`]): a copy in this computer's app data,
//! and the shared one in the photo share (`.card-intake/shutter.sqlite`), which every
//! computer running Card Intake reads and adds to. Merging is idempotent (per day it
//! keeps the lowest and highest count), so two computers can add the same photos
//! without counting them twice. [`ShutterDb`] holds readings in memory before they're
//! stored, and the rows a sync moves between databases.

mod store;
pub use store::{Generation, SHARED_DB, Store, shared_path, sync_shared};

use crate::cameras;
use crate::exiftool::{self, ShutterSample};
use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;

/// Folder in the photo share for Card Intake's shared files.
pub const SHARED_DIR: &str = ".card-intake";
/// The JSON file older Card Intake builds kept the history in.
pub const SHARED_FILE: &str = "shutter-counts.json";
/// The JSON format version older builds wrote.
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
    /// Carried with pending reads so removal also discards an old in-flight batch.
    /// Legacy JSON has no registration generations.
    #[serde(skip)]
    pub generation: Option<Generation>,
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
    fn camera(
        &mut self,
        model: &str,
        serial: Option<&str>,
        internal: Option<&str>,
    ) -> &mut CameraHistory {
        let key = |c: &CameraHistory| {
            (
                c.model.clone(),
                c.serial.clone(),
                c.serial
                    .is_none()
                    .then(|| c.internal_serial.clone())
                    .flatten(),
            )
        };
        let wanted = (
            model.to_string(),
            serial.map(str::to_string),
            serial
                .is_none()
                .then(|| internal.map(str::to_string))
                .flatten(),
        );
        match self.cameras.binary_search_by(|c| key(c).cmp(&wanted)) {
            Ok(i) => &mut self.cameras[i],
            Err(i) => {
                self.cameras.insert(
                    i,
                    CameraHistory {
                        model: wanted.0,
                        serial: wanted.1,
                        internal_serial: internal.map(str::to_string),
                        generation: None,
                        days: Vec::new(),
                    },
                );
                &mut self.cameras[i]
            }
        }
    }

    /// Adds the photos of one read (a card, a folder), leaving out counts the camera
    /// can't have made; see [`implausible`]. Returns how many were left out.
    pub fn add_read(&mut self, samples: &[ShutterSample]) -> usize {
        let (read, left_out) = self.read(samples);
        self.merge(&read);
        left_out
    }

    /// Checks raw readings before reducing them to day ranges. The returned batch
    /// contains only this read, checked against this history as well as itself.
    pub fn read(&self, samples: &[ShutterSample]) -> (ShutterDb, usize) {
        let skip = implausible_with_history(samples, self);
        let mut read = ShutterDb::default();
        for (i, sample) in samples.iter().enumerate() {
            // Even a count we can't trust still identifies a newly seen camera.
            let serial = sample.serial.as_deref().or_else(|| {
                let internal = sample.internal_serial.as_deref()?;
                self.cameras
                    .iter()
                    .find(|c| {
                        c.model == sample.model && c.internal_serial.as_deref() == Some(internal)
                    })
                    .and_then(|c| c.serial.as_deref())
                    .or_else(|| {
                        samples
                            .iter()
                            .find(|s| {
                                s.model == sample.model
                                    && s.internal_serial.as_deref() == Some(internal)
                                    && s.serial.is_some()
                            })
                            .and_then(|s| s.serial.as_deref())
                    })
            });
            let camera = read.camera(&sample.model, serial, sample.internal_serial.as_deref());
            camera.generation = self
                .cameras
                .iter()
                .find(|c| {
                    c.model == sample.model
                        && (serial.is_some() && c.serial.as_deref() == serial
                            || sample.internal_serial.is_some()
                                && c.internal_serial == sample.internal_serial
                            || serial.is_none()
                                && sample.internal_serial.is_none()
                                && c.serial.is_none()
                                && c.internal_serial.is_none())
                })
                .and_then(|c| c.generation.clone());
            if !skip.contains(&i) {
                read.add(sample);
            }
        }
        (read, skip.len())
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
        let camera = self.camera(&s.model, serial.as_deref(), s.internal_serial.as_deref());
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
                    let target = &mut self.cameras[t];
                    if orphan.generation.is_some()
                        && target.generation.is_some()
                        && orphan.generation != target.generation
                    {
                        if orphan.generation > target.generation {
                            target.generation = orphan.generation;
                            target.days = orphan.days;
                        }
                    } else {
                        target.generation = target.generation.take().or(orphan.generation);
                        for day in &orphan.days {
                            target.add_day(day);
                        }
                    }
                }
                None => i += 1,
            }
        }
    }

    pub fn merge(&mut self, other: &ShutterDb) {
        self.version = self.version.max(other.version);
        for camera in &other.cameras {
            let mine = self.camera(
                &camera.model,
                camera.serial.as_deref(),
                camera.internal_serial.as_deref(),
            );
            if let Some(incoming) = &camera.generation {
                if mine.generation.as_ref().is_some_and(|g| g > incoming) {
                    continue;
                }
                if mine.generation.as_ref().is_some_and(|g| g < incoming) {
                    mine.days.clear();
                }
                mine.generation = Some(incoming.clone());
            }
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

    pub fn is_empty(&self) -> bool {
        self.cameras.is_empty() && self.folders.is_empty()
    }

    /// Reads a JSON database written by an older Card Intake, or an empty one when the
    /// file doesn't exist.
    pub fn load_json(path: &Path) -> io::Result<Self> {
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

    /// Cameras for display, the most recently used first; cameras without a reading
    /// yet come last.
    pub fn views(&self) -> Vec<CameraView> {
        // Keep stored ranges visible, including real counter resets. Day extrema
        // alone cannot distinguish a reset from a bad high; repair needs the photos.
        let mut views: Vec<_> = self
            .cameras
            .iter()
            .map(|c| {
                let last = c.days.last();
                let info = cameras::model_info(&c.model);
                CameraView {
                    model: c.model.clone(),
                    name: info.map(|i| i.name),
                    serial: c.serial.clone(),
                    internal_serial: c.internal_serial.clone(),
                    firmware: c.days.iter().rev().find_map(|d| d.firmware.clone()),
                    rated_shutter: info.and_then(|i| i.rated_shutter),
                    rating_source: info.and_then(|i| i.rating_source),
                    // A body without a mechanical shutter (the Nikon Z 9) counts its
                    // electronic shutter only, so its count is that one.
                    has_mechanical_shutter: cameras::has_mechanical_shutter(&c.model),
                    shutter_reference: info.and_then(|i| i.shutter_reference),
                    count: last.map(|d| d.max),
                    electronic_count: c.days.iter().filter_map(|d| d.electronic_max).max(),
                    last_shot: last.map(|d| d.last_shot),
                    days: c.days.clone(),
                }
            })
            .collect();
        views.sort_by_key(|v| std::cmp::Reverse(v.last_shot));
        views
    }

    /// Takes the cameras that aren't in `mine` out of this database, and returns them.
    pub fn split_off_new(&mut self, mine: &MyCameras) -> ShutterDb {
        let (kept, new) = std::mem::take(&mut self.cameras)
            .into_iter()
            .partition(|c| mine.owns_camera(c));
        self.cameras = kept;
        ShutterDb {
            version: self.version,
            cameras: new,
            folders: BTreeMap::new(),
        }
    }
}

/// The cameras registered as the user's own (My Cameras). Only their readings are
/// stored; another camera's photos show it as a new camera until it's registered.
/// A camera is known by its serial number, or by Sony's internal serial number when its
/// photos don't carry the serial.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MyCameras {
    /// Model and serial number; an empty serial for a body known by neither number.
    serials: BTreeSet<(String, String)>,
    /// Model and internal serial number.
    internals: BTreeSet<(String, String)>,
}

impl MyCameras {
    pub fn add(&mut self, model: &str, serial: Option<&str>, internal: Option<&str>) {
        let serial = serial.filter(|s| !s.is_empty());
        if let Some(internal) = internal {
            self.internals.insert((model.into(), internal.into()));
        }
        if serial.is_some() || internal.is_none() {
            self.serials
                .insert((model.into(), serial.unwrap_or_default().into()));
        }
    }

    pub fn owns(&self, model: &str, serial: Option<&str>, internal: Option<&str>) -> bool {
        let has = |set: &BTreeSet<(String, String)>, value: &str| {
            set.contains(&(model.to_string(), value.to_string()))
        };
        let serial = serial.filter(|s| !s.is_empty());
        serial.is_some_and(|s| has(&self.serials, s))
            || internal.is_some_and(|i| has(&self.internals, i))
            || (serial.is_none() && internal.is_none() && has(&self.serials, ""))
    }

    /// Whether any of the cameras is this model, whatever its serial number.
    pub fn has_model(&self, model: &str) -> bool {
        self.serials
            .iter()
            .chain(&self.internals)
            .any(|(m, _)| m.eq_ignore_ascii_case(model))
    }

    pub fn owns_camera(&self, camera: &CameraHistory) -> bool {
        self.owns(
            &camera.model,
            camera.serial.as_deref(),
            camera.internal_serial.as_deref(),
        )
    }
}

/// EXIF times have whole seconds. One extra second covers a burst across a second
/// boundary; there is no count allowance for clock changes.
fn reachable(a: &ShutterSample, b: &ShutterSample) -> bool {
    let seconds = (b.taken - a.taken).num_seconds().max(0) as u64;
    let fastest = cameras::count_rate_limit(&a.model).saturating_mul(seconds + 1);
    b.count >= a.count && b.count - a.count <= fastest
}

/// The photos whose count doesn't fit the other photos of the same camera, by index.
///
/// ExifTool finds the α7R VI's count by guessing the maker-note layout, a guess it says
/// holds for the mechanical shutter. Where it doesn't, a photo can read 4,194,304 on a
/// day the camera's other photos read 2 to 30, and since merging keeps each day's
/// highest count, that one photo would stay in the history for good.
///
/// A count never goes down, so keep the longest consistent run. Sony's guessed
/// layout also needs another, nearby count: an isolated last photo can otherwise
/// extend that run by any plausible number of frames over a long gap.
pub fn implausible(samples: &[ShutterSample]) -> BTreeSet<usize> {
    implausible_with_history(samples, &ShutterDb::default())
}

fn implausible_with_history(samples: &[ShutterSample], history: &ShutterDb) -> BTreeSet<usize> {
    // Photos without the serial number belong to the camera with the same internal one.
    let serials: BTreeMap<(&str, &str), &str> = history
        .cameras
        .iter()
        .filter_map(|c| {
            Some((
                (c.model.as_str(), c.internal_serial.as_deref()?),
                c.serial.as_deref()?,
            ))
        })
        .chain(samples.iter().filter_map(|s| {
            Some((
                (s.model.as_str(), s.internal_serial.as_deref()?),
                s.serial.as_deref()?,
            ))
        }))
        .collect();
    // Model, serial, and the internal serial when there's no serial.
    type Body<'a> = (&'a str, Option<&'a str>, Option<&'a str>);
    let mut cameras: BTreeMap<Body, Vec<usize>> = BTreeMap::new();
    for (i, s) in samples.iter().enumerate() {
        let internal = s.internal_serial.as_deref();
        let serial = s
            .serial
            .as_deref()
            .or_else(|| serials.get(&(s.model.as_str(), internal?)).copied());
        let key = (
            s.model.as_str(),
            serial,
            serial.is_none().then_some(internal).flatten(),
        );
        cameras.entry(key).or_default().push(i);
    }
    let mut skip = BTreeSet::new();
    for ((model, serial, internal), mut photos) in cameras {
        photos.sort_by_key(|&i| (samples[i].taken, samples[i].count));
        let stored = history.cameras.iter().find(|c| {
            c.model == model
                && (serial.is_some() && c.serial.as_deref() == serial
                    || internal.is_some() && c.internal_serial.as_deref() == internal
                    || serial.is_none()
                        && internal.is_none()
                        && c.serial.is_none()
                        && c.internal_serial.is_none())
        });
        photos.retain(|&i| {
            let fits = stored.is_none_or(|c| fits_history(&samples[i], &c.days));
            if !fits {
                skip.insert(i);
            }
            fits
        });
        let order: Vec<&ShutterSample> = photos.iter().map(|&i| &samples[i]).collect();
        let kept = longest_consistent_run(&order);
        let counts: BTreeSet<u64> = kept.iter().map(|&i| order[i].count).collect();
        let mut accepted: BTreeSet<usize> = kept.into_iter().collect();
        if matches!(model, "ILCE-1M2" | "ILCE-7M5" | "ILCE-7RM6") {
            let near = cameras::count_rate_limit(model);
            accepted.retain(|&i| {
                let s = order[i];
                // A different count within one second's frame budget corroborates
                // the guess. Repeated copies of a bogus constant don't.
                counts
                    .range(..s.count)
                    .next_back()
                    .is_some_and(|&n| s.count - n <= near)
                    || counts
                        .range((
                            std::ops::Bound::Excluded(s.count),
                            std::ops::Bound::Unbounded,
                        ))
                        .next()
                        .is_some_and(|&n| n - s.count <= near)
                    || stored.is_some_and(|c| supported_by_history(s, &c.days, near))
            });
        }
        for (at, &i) in photos.iter().enumerate() {
            if !accepted.contains(&at) {
                skip.insert(i);
            }
        }
    }
    skip
}

/// A lasting drop starts a new counter history, as after a shutter replacement.
/// Don't compare readings across it. A high that overlaps the next day's low is
/// unusable as a lower bound: the old max() merge may already have poisoned it.
fn history_days(
    days: &[DayCounts],
    taken: NaiveDateTime,
) -> impl Iterator<Item = (&DayCounts, Option<u64>)> {
    let mut start = 0;
    let mut end = days.len();
    for i in 1..days.len() {
        if days[i].max < days[i - 1].min {
            if days[i].date <= taken.date() {
                start = i;
            } else {
                end = i;
                break;
            }
        }
    }
    (start..end).map(move |i| {
        let d = &days[i];
        let high = (i + 1 == end || d.max <= days[i + 1].min).then_some(d.max);
        (d, high)
    })
}

fn fits_history(s: &ShutterSample, days: &[DayCounts]) -> bool {
    let rate = cameras::count_rate_limit(&s.model);
    history_days(days, s.taken).all(|(d, high)| {
        let before = (s.taken - d.first_shot).num_seconds();
        let after = (s.taken - d.last_shot).num_seconds();
        if before < 0 {
            s.count <= d.min && d.min - s.count <= rate.saturating_mul(before.unsigned_abs() + 1)
        } else if after > 0 {
            let (count, seconds) = high.map_or((d.min, before), |h| (h, after));
            s.count >= count && s.count - count <= rate.saturating_mul(seconds as u64 + 1)
        } else {
            // The extrema aren't tied to individual photos in the store. At an
            // endpoint's whole second, allow the rest of that second's burst.
            s.count.saturating_add(if before == 0 { rate } else { 0 }) >= d.min
                && s.count.saturating_sub(d.min) <= rate.saturating_mul(before as u64 + 1)
                && high.is_none_or(|h| {
                    s.count <= h.saturating_add(if after == 0 { rate } else { 0 })
                        && h.saturating_sub(s.count)
                            <= rate.saturating_mul(after.unsigned_abs() + 1)
                })
        }
    })
}

fn supported_by_history(s: &ShutterSample, days: &[DayCounts], near: u64) -> bool {
    history_days(days, s.taken).any(|(d, high)| {
        s.count.abs_diff(d.min) <= near
            || high.is_some_and(|h| {
                s.count.abs_diff(h) <= near
                    || (s.taken >= d.first_shot
                        && s.taken <= d.last_shot
                        && (d.min..=h).contains(&s.count))
            })
    })
}

/// Positions in `photos` (in time order) of the longest run whose counts never go down,
/// cut where a count rises faster than a camera fires; ties keep the earliest run.
fn longest_consistent_run(photos: &[&ShutterSample]) -> Vec<usize> {
    // Longest non-decreasing subsequence: `ends[n]` is where the best run of n + 1
    // photos found so far ends, and `back` links each photo to the one before it.
    let mut ends: Vec<usize> = Vec::new();
    let mut back: Vec<Option<usize>> = vec![None; photos.len()];
    for (i, photo) in photos.iter().enumerate() {
        let n = ends.partition_point(|&e| photos[e].count <= photo.count);
        back[i] = n.checked_sub(1).map(|n| ends[n]);
        if n == ends.len() {
            ends.push(i);
        } else {
            ends[n] = i;
        }
    }
    let mut run = Vec::with_capacity(ends.len());
    let mut at = ends.last().copied();
    while let Some(i) = at {
        run.push(i);
        at = back[i];
    }
    run.reverse();
    run.chunk_by(|&a, &b| reachable(photos[a], photos[b]))
        .fold(
            &[][..],
            |best, part| if part.len() > best.len() { part } else { best },
        )
        .to_vec()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CameraView {
    pub model: String,
    /// The name on the body, e.g. "α7R VI".
    pub name: Option<&'static str>,
    pub serial: Option<String>,
    /// Sony's internal serial number, which identifies a body whose photos don't carry
    /// the serial number.
    pub internal_serial: Option<String>,
    pub firmware: Option<String>,
    /// The maker's official shutter rating, when it published one.
    pub rated_shutter: Option<u64>,
    pub rating_source: Option<&'static str>,
    /// Whether the count above is a mechanical one. False for a body with no mechanical
    /// shutter (the Nikon Z 9), whose count is its electronic shutter's.
    pub has_mechanical_shutter: bool,
    /// The highest shutter count seen; none for a camera registered before any of its
    /// photos with a count were read.
    pub count: Option<u64>,
    pub electronic_count: Option<u64>,
    /// A user-suggested comparison value, not a manufacturer durability rating.
    pub shutter_reference: Option<u64>,
    pub last_shot: Option<NaiveDateTime>,
    pub days: Vec<DayCounts>,
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
        std::fs::write(&path, serde_json::to_vec(&db).unwrap()).unwrap();
        let loaded = ShutterDb::load_json(&path).unwrap();
        assert_eq!(loaded.views()[0].electronic_count, Some(1234));
        assert_eq!(loaded.views()[0].count, Some(1));
        let mut old = serde_json::to_value(&loaded).unwrap();
        old["version"] = 1.into();
        old["cameras"][0]["days"][0]
            .as_object_mut()
            .unwrap()
            .remove("electronic_max");
        old["folders"] =
            serde_json::json!({ "2026/shoot": { "photos": 1, "bytes": 100, "modified": 200 } });
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        let migrated = ShutterDb::load_json(&path).unwrap();
        assert_eq!(migrated.views()[0].count, Some(1));
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
        assert_eq!(views[0].count, Some(894));
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

    fn at_secs(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    /// The α7R VI's first shoots (counts from the photos in the share), with the photo
    /// that read 4,194,304 among them.
    fn first_shoots(bad: u64, bad_at: &str) -> Vec<ShutterSample> {
        let mut photos: Vec<ShutterSample> = [
            (2, "2026-10-30 17:54:04"),
            (4, "2026-10-30 17:58:54"),
            (5, "2026-10-30 17:59:36"),
            (18, "2026-10-30 20:47:21"),
            (30, "2026-10-30 23:03:22"),
            (31, "2026-10-31 11:44:16"),
            (604, "2026-10-31 18:16:13"),
            (614, "2026-10-31 18:17:49"),
        ]
        .into_iter()
        .map(|(count, taken)| ShutterSample {
            taken: at_secs(taken),
            ..sample(count, "2026-01-01 00:00")
        })
        .collect();
        photos.push(ShutterSample {
            taken: at_secs(bad_at),
            ..sample(bad, "2026-01-01 00:00")
        });
        photos
    }

    #[test]
    fn a_count_that_does_not_fit_the_camera_is_left_out() {
        let photos = first_shoots(4_194_304, "2026-10-30 19:00:00");
        assert_eq!(implausible(&photos), BTreeSet::from([8]));
        let mut db = ShutterDb::default();
        assert_eq!(db.add_read(&photos), 1);
        let days = &db.cameras[0].days;
        assert_eq!((days[0].min, days[0].max), (2, 30));
        assert_eq!(db.views()[0].count, Some(614));

        // The camera's newest photo, with nothing after it to compare: a count can't
        // rise by millions in a minute.
        let last = first_shoots(4_194_304, "2026-10-31 18:18:49");
        assert_eq!(implausible(&last), BTreeSet::from([8]));
        // Nor read far below the photos around it.
        let low = first_shoots(1, "2026-10-31 12:00:00");
        assert_eq!(
            implausible(&low),
            BTreeSet::from([8]),
            "a clock change is not a count allowance"
        );
        let mut later = first_shoots(0, "2026-10-31 12:00:00");
        for s in &mut later {
            s.count += 50_000;
        }
        later[8].count = 7;
        assert_eq!(implausible(&later), BTreeSet::from([8]));
    }

    #[test]
    fn clock_changes_are_not_slack_and_consistent_counts_are_kept() {
        // Setting the clock back an hour puts photos 31..=40 before photos 25..=30.
        let mut photos: Vec<ShutterSample> = (0..30u32)
            .map(|i| sample(1 + u64::from(i), &format!("2026-11-01 18:{:02}", i * 2)))
            .collect();
        photos.extend(
            (0..10u32).map(|i| sample(31 + u64::from(i), &format!("2026-11-01 18:{:02}", 40 + i))),
        );
        assert_eq!(implausible(&photos), (21..30).collect());

        // Two bodies of one model are compared apart, even without serial numbers; a
        // photo without the serial goes with the camera of the same internal one.
        let other = |count, taken| ShutterSample {
            serial: None,
            internal_serial: Some("63 255 0 0 42 9".into()),
            ..sample(count, taken)
        };
        let unnamed = |count, taken| ShutterSample {
            serial: None,
            ..sample(count, taken)
        };
        let mixed = [
            unnamed(2, "2026-10-30 17:54"),
            other(3480, "2026-10-30 18:00"),
            other(3481, "2026-10-30 18:01"),
            sample(30, "2026-10-30 23:03"),
            other(3560, "2026-10-30 23:09"),
            other(3561, "2026-10-30 23:10"),
        ];
        assert_eq!(implausible(&mixed), BTreeSet::new());

        // A body whose mechanical count stays put through electronic-shutter photos.
        let zf: Vec<_> = [10_045, 10_045, 10_045, 10_046]
            .into_iter()
            .enumerate()
            .map(|(i, count)| ShutterSample {
                model: "NIKON Z f".into(),
                ..sample(count, &format!("2026-06-02 12:0{i}"))
            })
            .collect();
        assert_eq!(implausible(&zf), BTreeSet::new());
        // An isolated guessed Sony count has no support, even if its rate fits.
        assert_eq!(
            implausible(&[sample(4_194_304, "2026-10-30 19:00")]),
            BTreeSet::from([0])
        );
    }

    /// A ten-minute burst at `fps`, by `model`.
    fn burst(model: &str, fps: u64) -> Vec<ShutterSample> {
        let start = at("2026-11-07 10:00");
        [(0, 5), (0, 6), (600, 5 + fps * 600), (600, 6 + fps * 600)]
            .into_iter()
            .map(|(second, count)| ShutterSample {
                model: model.into(),
                taken: start + chrono::Duration::seconds(second),
                ..sample(count, "2026-11-07 10:00")
            })
            .collect()
    }

    #[test]
    fn each_body_rises_no_faster_than_its_own_top_frame_rate() {
        // 120 frames a second is a Z 9's top speed, and far past an α7R VI's 30.
        assert_eq!(implausible(&burst("NIKON Z 9", 120)), BTreeSet::new());
        assert_eq!(implausible(&burst("ILCE-7RM6", 120)).len(), 2);
        // Within 10% of the top speed passes; beyond doesn't.
        assert_eq!(implausible(&burst("ILCE-7RM6", 32)), BTreeSet::new());
        assert_eq!(implausible(&burst("ILCE-7RM6", 36)).len(), 2);
        assert_eq!(implausible(&burst("Canon EOS R3", 195)), BTreeSet::new());
    }

    #[test]
    fn new_cameras_are_split_off_from_registered_ones() {
        let demo = |count, taken| ShutterSample {
            serial: None,
            internal_serial: Some("63 255 0 0 42 9".into()),
            ..sample(count, taken)
        };
        let mut read = ShutterDb::default();
        read.add_read(&[
            sample(30, "2026-10-30 23:03"),
            demo(3480, "2026-06-10 23:04"),
            ShutterSample {
                model: "NIKON Z 9".into(),
                serial: Some("8016296".into()),
                internal_serial: None,
                ..sample(165_601, "2026-10-05 13:57")
            },
        ]);
        let mut mine = MyCameras::default();
        mine.add("ILCE-7RM6", Some("03506694"), Some("73 255 0 0 25 9"));
        let new = read.split_off_new(&mine);
        let kept: Vec<_> = read.cameras.iter().map(|c| c.model.as_str()).collect();
        assert_eq!(kept, ["ILCE-7RM6"]);
        assert_eq!(new.cameras.len(), 2);
        assert!(new.folders.is_empty());

        // Known by the internal serial number when the photos carry no serial.
        assert!(mine.owns("ILCE-7RM6", None, Some("73 255 0 0 25 9")));
        assert!(!mine.owns("ILCE-7RM6", None, Some("63 255 0 0 42 9")));
        assert!(!mine.owns("ILCE-7RM6", None, None));
        // A body registered by its internal serial alone owns only photos with it.
        mine.add("ILCE-1", None, Some("1 2 3"));
        assert!(mine.owns("ILCE-1", Some("5021734"), Some("1 2 3")));
        assert!(!mine.owns("ILCE-1", None, None));
        mine.add("PP-101", None, None);
        assert!(mine.owns("PP-101", None, None));
        assert!(!mine.owns("PP-101", Some("5WCQ"), None));
    }

    fn real_history() -> ShutterDb {
        let mut db = ShutterDb::default();
        for (count, taken) in [
            (2, "2026-10-30 17:54:04"),
            (30, "2026-10-30 23:03:22"),
            (31, "2026-10-31 11:44:16"),
            (614, "2026-10-31 18:17:49"),
            (699, "2026-11-06 17:21:17"),
            (894, "2026-11-06 20:51:07"),
        ] {
            db.add(&ShutterSample {
                taken: at_secs(taken),
                ..sample(count, "2026-01-01 00:00")
            });
        }
        db
    }

    #[test]
    fn stored_history_rejects_small_bad_counts_even_in_a_one_photo_read() {
        let db = real_history();
        for count in [64, 256, 1024, 4_194_304] {
            let (read, left_out) = db.read(&[sample(count, "2026-10-30 20:00")]);
            assert_eq!(left_out, 1, "{count}");
            assert!(read.cameras[0].days.is_empty());
        }
        // The bad reading is newest on this card, but not in the stored history.
        let (_, left_out) = db.read(&[
            sample(2, "2026-10-30 17:54"),
            sample(5, "2026-10-30 17:59"),
            sample(1024, "2026-10-30 20:00"),
        ]);
        assert_eq!(left_out, 1);
        // There are stored days on both sides of this photo as well.
        assert_eq!(db.read(&[sample(1024, "2026-10-31 16:00")]).1, 1);
        let unnamed = ShutterSample {
            serial: None,
            ..sample(1024, "2026-10-30 20:00")
        };
        assert_eq!(db.read(&[unnamed]).1, 1);
        let mut surrounding = db.clone();
        surrounding.cameras[0].days.remove(0);
        assert_eq!(surrounding.read(&[sample(1024, "2026-10-30 20:00")]).1, 1);
        let mut surrounding = db.clone();
        surrounding.cameras[0].days.remove(1);
        assert_eq!(surrounding.read(&[sample(1024, "2026-10-31 16:00")]).1, 1);
    }

    #[test]
    fn stored_endpoint_times_limit_rates_even_when_bad_guesses_support_each_other() {
        let db = real_history();
        let photos = [
            ShutterSample {
                taken: at_secs("2026-11-06 20:51:08"),
                ..sample(1024, "2026-01-01 00:00")
            },
            ShutterSample {
                taken: at_secs("2026-11-06 20:51:09"),
                ..sample(1025, "2026-01-01 00:00")
            },
        ];
        assert_eq!(db.read(&photos).1, 2);
        let photos = [
            ShutterSample {
                taken: at_secs("2026-10-31 11:44:17"),
                ..sample(256, "2026-01-01 00:00")
            },
            ShutterSample {
                taken: at_secs("2026-10-31 11:44:18"),
                ..sample(257, "2026-01-01 00:00")
            },
        ];
        assert_eq!(db.read(&photos).1, 2);
    }

    #[test]
    fn an_isolated_newest_guess_needs_support_even_when_the_rate_is_possible() {
        let db = real_history();
        assert_eq!(db.read(&[sample(1024, "2026-11-07 12:00")]).1, 1);
        assert_eq!(db.read(&[sample(895, "2026-11-07 12:00")]).1, 0);
        let photos = [
            sample(18, "2026-10-30 20:47"),
            sample(30, "2026-10-30 23:03"),
            sample(256, "2026-10-31 18:00"),
        ];
        assert_eq!(implausible(&photos), BTreeSet::from([2]));
        // Two copies of the same bogus constant don't corroborate the layout.
        assert_eq!(
            implausible(&[
                sample(1024, "2026-11-07 12:00"),
                sample(1024, "2026-11-07 12:01")
            ]),
            BTreeSet::from([0, 1])
        );
    }

    #[test]
    fn a_poisoned_stored_high_does_not_discredit_good_readings() {
        let mut db = real_history();
        db.add(&sample(1024, "2026-10-30 20:00"));
        let mut photos = first_shoots(1024, "2026-10-30 20:00:00");
        photos.pop();
        photos.extend([
            sample(699, "2026-11-06 17:22"),
            sample(894, "2026-11-06 20:51"),
        ]);
        let (read, left_out) = db.read(&photos);
        assert_eq!(left_out, 0);
        assert_eq!(
            (read.cameras[0].days[0].min, read.cameras[0].days[0].max),
            (2, 30)
        );
        assert_eq!(db.read(&[sample(1024, "2026-10-30 20:00")]).1, 1);
        // Filtering doesn't pretend to repair a range already merged in the store.
        assert_eq!(db.cameras[0].days[0].max, 1024);
    }

    #[test]
    fn stored_counter_resets_remain_visible_and_bound_their_own_days() {
        let mut db = real_history();
        db.add(&sample(165_000, "2026-10-29 12:00"));
        let views = db.views();
        assert_eq!(views[0].days.len(), 4);
        assert_eq!(views[0].days[0].max, 165_000);
        assert_eq!(views[0].days[1].min, 2);
        assert_eq!(views[0].count, Some(894));
        assert_eq!(db.read(&[sample(18, "2026-10-30 20:47")]).1, 0);
    }

    #[test]
    fn two_bodies_without_serials_stay_separate_before_either_serial_is_known() {
        let mut db = ShutterDb::default();
        for (internal, count) in [("63 255 0 0 42 9", 3480), ("73 255 0 0 25 9", 2)] {
            db.add(&ShutterSample {
                serial: None,
                internal_serial: Some(internal.into()),
                ..sample(count, "2026-10-30 18:00")
            });
        }
        assert_eq!(db.cameras.len(), 2);
        db.add(&sample(30, "2026-10-30 23:03"));
        assert_eq!(db.cameras.len(), 2);
        let mine = db.cameras.iter().find(|c| c.serial.is_some()).unwrap();
        assert_eq!((mine.days[0].min, mine.days[0].max), (2, 30));
        assert_eq!(db.cameras[0].days[0].max, 3480);
    }

    #[test]
    fn a_camera_registered_before_any_count_is_listed_last_without_one() {
        let mut db = ShutterDb::default();
        db.add(&sample(894, "2026-11-06 20:51"));
        db.cameras.push(CameraHistory {
            model: "PP-101".into(),
            serial: Some("5WCQNBL6BB1A22".into()),
            internal_serial: None,
            generation: None,
            days: Vec::new(),
        });
        let views = db.views();
        assert_eq!(views[0].count, Some(894));
        assert_eq!(views[1].name, Some("Osmo Pocket 3"));
        assert_eq!((views[1].count, views[1].last_shot), (None, None));
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
