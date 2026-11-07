//! Card imports. Originals are opened read-only; destination files are published
//! only after a complete copy (and, by default, an xxHash64 read-back).
//!
//! Several files are copied at once. A CFexpress card answers parallel reads faster
//! than one stream, and while one file is read back from the NAS for verification,
//! the next ones are already being copied.

use crate::scan::MediaKind;
use crate::{destinations, naming, nas, rating, scan, volume_info};
use chrono::{Local, NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};
use xxhash_rust::xxh64::Xxh64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShootSelection {
    pub date: NaiveDate,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportRequest {
    pub root: PathBuf,
    pub nas_root: PathBuf,
    pub expected_card_id: Option<String>,
    pub shoots: Vec<ShootSelection>,
    pub include_videos: bool,
    pub verify: bool,
    /// Files copied at the same time; 0 means [`DEFAULT_WORKERS`].
    #[serde(default)]
    pub workers: usize,
    /// The camera that shot the card, kept in the import's history.
    #[serde(default)]
    pub camera: Option<String>,
}

/// Four files at once keeps a CFexpress card and a 10 GbE link busy without
/// thrashing an SD card.
pub const DEFAULT_WORKERS: usize = 4;

/// Read and write in 4 MiB pieces: fewer round trips over SMB than 1 MiB.
const CHUNK: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportStatus {
    Complete,
    Cancelled,
    Failed,
}

/// Stored in the local ledger, including partial, cancelled and failed imports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportResult {
    pub started_at: NaiveDateTime,
    pub finished_at: NaiveDateTime,
    pub root: PathBuf,
    pub nas_root: PathBuf,
    /// The card's volume ID when it was imported.
    pub card_id: Option<String>,
    /// The card's record in the ledger, which survives formatting when the card's
    /// own serial number is known.
    #[serde(default)]
    pub card_key: Option<String>,
    pub status: ImportStatus,
    pub total_files: usize,
    pub total_bytes: u64,
    pub copied_files: usize,
    pub skipped_files: usize,
    pub verified_files: usize,
    pub completed_bytes: u64,
    /// Photos and clips selected (sidecars not counted), copied or already on the NAS.
    #[serde(default)]
    pub photos: usize,
    #[serde(default)]
    pub videos: usize,
    /// Files skipped because the same file is already in another folder for the
    /// same date (an earlier import under another shoot name).
    #[serde(default)]
    pub skipped_elsewhere: usize,
    /// Those folders, relative to the NAS root.
    #[serde(default)]
    pub found_in: Vec<PathBuf>,
    #[serde(default)]
    pub camera: Option<String>,
    /// Average speed reading the card while files were being copied.
    #[serde(default)]
    pub average_bytes_per_sec: Option<u64>,
    /// Relative to the NAS root, so history also makes sense on another OS.
    pub shoots: Vec<PathBuf>,
    pub error: Option<String>,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub ratings: rating::RatingSummary,
    pub ejected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportProgress {
    pub phase: &'static str,
    pub current_file: Option<PathBuf>,
    pub completed_files: usize,
    pub skipped_files: usize,
    pub total_files: usize,
    /// Copy and verification each account for half of a file when verifying.
    pub processed_bytes: u64,
    pub total_bytes: u64,
    /// How fast the card is being read, over the last few seconds.
    pub bytes_per_sec: Option<f64>,
    /// Estimated seconds until the import is done, verification included.
    pub eta_secs: Option<f64>,
}

struct Entry {
    source: PathBuf,
    relative: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
    /// Photo or video; `None` for a sidecar.
    kind: Option<MediaKind>,
    /// Same-named, same-size files in the date's other folders, which may already
    /// hold this photo or clip.
    elsewhere: Vec<PathBuf>,
}

struct Plan {
    root: PathBuf,
    destination: PathBuf,
    entries: Vec<Entry>,
    /// A photo or clip followed by its sidecars, handled by one worker in order.
    groups: Vec<Vec<usize>>,
    shoots: Vec<PathBuf>,
    card_id: Option<String>,
}

/// How a file ended up on the NAS.
enum Done {
    Copied,
    /// An identical copy was already at the destination.
    Identical,
    /// An identical copy is in another folder for the same date.
    Elsewhere(PathBuf),
    /// A sidecar next to an earlier copy differs from the card's; the NAS's is kept.
    KeptNas(PathBuf),
}

/// Stops every worker when the user cancels or any file fails.
struct Stop<'a> {
    user: &'a AtomicBool,
    failed: AtomicBool,
}

impl Stop<'_> {
    fn check(&self) -> Result<(), String> {
        if self.user.load(Ordering::Relaxed) || self.failed.load(Ordering::Relaxed) {
            Err(STOPPED.into())
        } else {
            Ok(())
        }
    }
}

const STOPPED: &str = "Import stopped. Completed files are kept; retry to check and skip them.";

/// What one worker is doing right now.
#[derive(Default, Clone)]
struct Slot {
    phase: &'static str,
    /// Progress through the current file, in the same units as `processed_bytes`.
    processed: u64,
    /// Bytes of the current file read from the card so far.
    read: u64,
}

/// Counters shared by the workers. Progress is reported while holding the lock, so a
/// callback sees a consistent picture and runs before its worker continues.
struct Shared<P> {
    progress: P,
    total_files: usize,
    total_bytes: u64,
    copied_files: usize,
    skipped_files: usize,
    verified_files: usize,
    skipped_elsewhere: usize,
    found_in: BTreeSet<PathBuf>,
    warnings: Vec<String>,
    rating_targets: Vec<(PathBuf, PathBuf)>,
    destination: PathBuf,
    completed_bytes: u64,
    completed_processed: u64,
    completed_read: u64,
    slots: Vec<Slot>,
    window: VecDeque<(Instant, u64, u64)>,
    started: Instant,
    error: Option<String>,
}

const SPEED_WINDOW: Duration = Duration::from_secs(5);

impl<P: FnMut(ImportProgress)> Shared<P> {
    fn processed(&self) -> u64 {
        self.completed_processed + self.slots.iter().map(|s| s.processed).sum::<u64>()
    }

    fn read(&self) -> u64 {
        self.completed_read + self.slots.iter().map(|s| s.read).sum::<u64>()
    }

    /// The most telling thing any worker is doing: copying, then verifying, then
    /// checking a file that's already on the NAS.
    fn phase(&self) -> &'static str {
        ["copying", "verifying", "checking_existing"]
            .into_iter()
            .find(|p| self.slots.iter().any(|s| s.phase == *p))
            .unwrap_or("copying")
    }

    /// Card speed and time left, from the last few seconds.
    fn rates(&mut self) -> (Option<f64>, Option<f64>) {
        let now = Instant::now();
        let (processed, read) = (self.processed(), self.read());
        if self
            .window
            .back()
            .is_none_or(|(t, ..)| now.duration_since(*t) >= Duration::from_millis(250))
        {
            self.window.push_back((now, processed, read));
        }
        while self.window.len() > 2
            && self
                .window
                .front()
                .is_some_and(|(t, ..)| now.duration_since(*t) > SPEED_WINDOW)
        {
            self.window.pop_front();
        }
        let Some(&(t0, p0, r0)) = self.window.front() else {
            return (None, None);
        };
        let seconds = now.duration_since(t0).as_secs_f64();
        if seconds < 1.0 {
            return (None, None);
        }
        let speed = read.saturating_sub(r0) as f64 / seconds;
        let work = processed.saturating_sub(p0) as f64 / seconds;
        let eta = (work > 0.0).then(|| self.total_bytes.saturating_sub(processed) as f64 / work);
        (Some(speed), eta)
    }

    fn emit(&mut self, phase: &'static str, file: &Path) {
        let (bytes_per_sec, eta_secs) = self.rates();
        let progress = ImportProgress {
            phase,
            current_file: Some(file.to_path_buf()),
            completed_files: self.copied_files + self.skipped_files,
            skipped_files: self.skipped_files,
            total_files: self.total_files,
            processed_bytes: self.processed(),
            total_bytes: self.total_bytes,
            bytes_per_sec,
            eta_secs,
        };
        (self.progress)(progress);
    }

    fn update(
        &mut self,
        worker: usize,
        phase: &'static str,
        processed: u64,
        read: u64,
        file: &Path,
    ) {
        self.slots[worker] = Slot {
            phase,
            processed,
            read,
        };
        let phase = self.phase();
        self.emit(phase, file);
    }

    fn complete(&mut self, worker: usize, entry: &Entry, done: Done, verify: bool) {
        self.slots[worker] = Slot::default();
        match done {
            Done::Copied => {
                self.copied_files += 1;
                if verify {
                    self.verified_files += 1;
                }
            }
            Done::Identical => {
                self.skipped_files += 1;
                self.verified_files += 1;
            }
            Done::Elsewhere(folder) => {
                self.skipped_files += 1;
                self.verified_files += 1;
                self.skipped_elsewhere += 1;
                // The shoot folder (`2026/2026-11-06_夜景`), not its `video` folder.
                if let Ok(relative) = folder.strip_prefix(&self.destination) {
                    self.found_in
                        .insert(relative.components().take(2).collect());
                }
            }
            Done::KeptNas(path) => {
                self.skipped_files += 1;
                self.verified_files += 1;
                self.warnings.push(format!(
                    "{} differs from the card's {}; the NAS's version was kept.",
                    path.display(),
                    entry.relative.display()
                ));
            }
        }
        self.completed_bytes += entry.size;
        self.completed_processed += entry.size;
        self.completed_read += entry.size;
        self.emit("file_complete", &entry.relative);
    }
}

/// The production entry point requires an existing network destination. Local
/// fixtures exercise the same copy engine in the tests below.
pub fn import_card(
    request: &ImportRequest,
    exiftool: Option<&Path>,
    cancel: &AtomicBool,
    progress: impl FnMut(ImportProgress) + Send,
) -> ImportResult {
    run_with_exiftool(request, true, exiftool, cancel, progress)
}

/// The same import engine for a local destination, without app settings or history.
pub fn import_to_folder(
    request: &ImportRequest,
    exiftool: Option<&Path>,
    cancel: &AtomicBool,
    progress: impl FnMut(ImportProgress) + Send,
) -> ImportResult {
    run_with_exiftool(request, false, exiftool, cancel, progress)
}

#[cfg(test)]
fn run(
    request: &ImportRequest,
    require_network: bool,
    cancel: &AtomicBool,
    progress: impl FnMut(ImportProgress) + Send,
) -> ImportResult {
    run_with_exiftool(request, require_network, None, cancel, progress)
}

fn run_with_exiftool(
    request: &ImportRequest,
    require_network: bool,
    exiftool: Option<&Path>,
    cancel: &AtomicBool,
    mut progress: impl FnMut(ImportProgress) + Send,
) -> ImportResult {
    let now = Local::now().naive_local();
    let mut result = ImportResult {
        started_at: now,
        finished_at: now,
        root: request.root.clone(),
        nas_root: request.nas_root.clone(),
        card_id: request.expected_card_id.clone(),
        card_key: None,
        status: ImportStatus::Failed,
        total_files: 0,
        total_bytes: 0,
        copied_files: 0,
        skipped_files: 0,
        verified_files: 0,
        completed_bytes: 0,
        photos: 0,
        videos: 0,
        skipped_elsewhere: 0,
        found_in: Vec::new(),
        camera: request.camera.clone(),
        average_bytes_per_sec: None,
        shoots: Vec::new(),
        error: None,
        warnings: Vec::new(),
        ratings: rating::RatingSummary::default(),
        ejected: false,
    };
    progress(ImportProgress {
        phase: "preparing",
        current_file: None,
        completed_files: 0,
        skipped_files: 0,
        total_files: 0,
        processed_bytes: 0,
        total_bytes: 0,
        bytes_per_sec: None,
        eta_secs: None,
    });
    let stop = Stop {
        user: cancel,
        failed: AtomicBool::new(false),
    };
    let plan = stop.check().and_then(|()| plan(request, require_network));
    let outcome = match plan {
        Err(error) => Err(error),
        Ok(plan) => {
            result.card_id = plan.card_id.clone();
            result.total_files = plan.entries.len();
            result.total_bytes = plan.entries.iter().map(|f| f.size).sum();
            result.shoots = plan.shoots.clone();
            let count = |k| plan.entries.iter().filter(|e| e.kind == Some(k)).count();
            result.photos = count(MediaKind::Photo);
            result.videos = count(MediaKind::Video);
            let workers = match request.workers {
                0 => DEFAULT_WORKERS,
                n => n,
            }
            .min(plan.groups.len())
            .max(1);
            let shared = Mutex::new(Shared {
                progress,
                total_files: result.total_files,
                total_bytes: result.total_bytes,
                copied_files: 0,
                skipped_files: 0,
                verified_files: 0,
                skipped_elsewhere: 0,
                found_in: BTreeSet::new(),
                warnings: Vec::new(),
                rating_targets: Vec::new(),
                destination: plan.destination.clone(),
                completed_bytes: 0,
                completed_processed: 0,
                completed_read: 0,
                slots: vec![Slot::default(); workers],
                window: VecDeque::new(),
                started: Instant::now(),
                error: None,
            });
            let next = AtomicUsize::new(0);
            let raw_sources: Vec<_> = plan
                .entries
                .iter()
                .filter(|e| e.kind == Some(MediaKind::Photo) && scan::uses_raw_sidecar(&e.source))
                .map(|e| e.source.clone())
                .collect();
            let readings = std::thread::scope(|scope| {
                let metadata = scope.spawn(|| rating::read_settings(exiftool, &raw_sources));
                for worker in 0..workers {
                    let (shared, next, stop, plan) = (&shared, &next, &stop, &plan);
                    scope.spawn(move || {
                        copy_files(worker, request, require_network, plan, next, stop, shared)
                    });
                }
                metadata.join().unwrap_or_else(|_| rating::Readings {
                    warnings: vec![
                        "Initial raw ratings couldn't be read; import again to retry.".into(),
                    ],
                    ..Default::default()
                })
            });
            let mut shared = shared.into_inner().unwrap_or_else(|e| e.into_inner());
            shared.warnings.extend(readings.warnings);
            apply_ratings(
                &plan,
                require_network,
                &stop,
                &readings.settings,
                &mut shared,
                &mut result.ratings,
            );
            if stop.user.load(Ordering::Relaxed) {
                shared.error.get_or_insert_with(|| STOPPED.to_string());
            }
            result.copied_files = shared.copied_files;
            result.skipped_files = shared.skipped_files;
            result.verified_files = shared.verified_files;
            result.skipped_elsewhere = shared.skipped_elsewhere;
            result.found_in = shared.found_in.into_iter().collect();
            // The folders that hold the files now: a new folder is only created when
            // something was copied into it, and earlier folders count too.
            let mut shoots: BTreeSet<PathBuf> = plan
                .shoots
                .iter()
                .filter(|s| plan.destination.join(s).is_dir())
                .cloned()
                .collect();
            shoots.extend(result.found_in.iter().cloned());
            result.shoots = shoots.into_iter().collect();
            result.warnings = shared.warnings;
            result.completed_bytes = shared.completed_bytes;
            let seconds = shared.started.elapsed().as_secs_f64();
            if shared.completed_read > 0 && seconds > 0.0 {
                result.average_bytes_per_sec =
                    Some((shared.completed_read as f64 / seconds) as u64);
            }
            match shared.error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    };
    match outcome {
        Ok(()) => result.status = ImportStatus::Complete,
        Err(error) => {
            result.status = if cancel.load(Ordering::Relaxed) {
                ImportStatus::Cancelled
            } else {
                ImportStatus::Failed
            };
            result.error = Some(error);
        }
    }
    result.finished_at = Local::now().naive_local();
    result
}

/// One worker: takes the next photo or clip (with its sidecars) until none are left
/// or the import stops.
fn copy_files<P: FnMut(ImportProgress)>(
    worker: usize,
    request: &ImportRequest,
    require_network: bool,
    plan: &Plan,
    next: &AtomicUsize,
    stop: &Stop,
    shared: &Mutex<Shared<P>>,
) {
    let lock = || shared.lock().unwrap_or_else(|e| e.into_inner());
    let mut buffer = vec![0u8; CHUNK];
    loop {
        if stop.check().is_err() {
            if stop.user.load(Ordering::Relaxed) {
                lock().error.get_or_insert_with(|| STOPPED.to_string());
            }
            return;
        }
        let Some(group) = plan.groups.get(next.fetch_add(1, Ordering::Relaxed)) else {
            return;
        };
        // Where the group's photo or clip already was, under another shoot name.
        let mut main_elsewhere: Option<PathBuf> = None;
        for &index in group {
            let entry = &plan.entries[index];
            let outcome = (|| -> Result<Done, String> {
                // A disconnected mount must never turn into a local destination.
                check_destination(&plan.destination, require_network)?;
                if volume_info::volume_info(&request.root).id != plan.card_id {
                    return Err(
                        "The card changed during import. Insert the original card and retry."
                            .into(),
                    );
                }
                let mut update = |phase, processed, read| {
                    lock().update(worker, phase, processed, read, &entry.relative);
                };
                if let Some(folder) = &main_elsewhere {
                    return sidecar_next_to(
                        entry,
                        folder,
                        &plan.root,
                        request.verify,
                        stop,
                        &mut buffer,
                        &mut update,
                    );
                }
                let destination = plan.destination.join(&entry.relative);
                if entry.kind.is_none() {
                    return sidecar_next_to(
                        entry,
                        destination.parent().unwrap(),
                        &plan.root,
                        request.verify,
                        stop,
                        &mut buffer,
                        &mut update,
                    )
                    .map(|done| match done {
                        Done::Elsewhere(_) => Done::Identical,
                        other => other,
                    });
                }
                if !entry.elsewhere.is_empty() && !destination.exists() {
                    for other in &entry.elsewhere {
                        update("checking_existing", 0, 0);
                        match compare_existing(entry, other, stop, &mut buffer, &mut |p, r| {
                            update("checking_existing", p, r)
                        }) {
                            Ok(()) => {
                                return Ok(Done::Elsewhere(other.parent().unwrap().to_path_buf()));
                            }
                            Err(e) if e == STOPPED => return Err(e),
                            // Another photo with the same name: this one is new.
                            Err(_) => {}
                        }
                    }
                }
                ensure_parent(&plan.destination, &entry.relative)?;
                transfer(
                    entry,
                    &plan.root,
                    &destination,
                    request.verify,
                    stop,
                    &mut buffer,
                    &mut update,
                )
                .map(|copied| {
                    if copied {
                        Done::Copied
                    } else {
                        Done::Identical
                    }
                })
            })()
            .map_err(|e| {
                if e == STOPPED {
                    e
                } else {
                    format!("{}: {e}", entry.relative.display())
                }
            });
            match outcome {
                Ok(done) => {
                    // The photo or clip is in another folder: its sidecars go there too.
                    if let (Some(_), Done::Elsewhere(folder)) = (entry.kind, &done) {
                        main_elsewhere = Some(folder.clone());
                    }
                    lock().complete(worker, entry, done, request.verify);
                }
                Err(error) => {
                    // The first error wins; the others are workers being told to stop.
                    lock().error.get_or_insert(error);
                    stop.failed.store(true, Ordering::Relaxed);
                    return;
                }
            }
        }
        let main = &plan.entries[group[0]];
        if main.kind == Some(MediaKind::Photo) && scan::uses_raw_sidecar(&main.source) {
            let destination = main_elsewhere
                .map(|folder| folder.join(main.relative.file_name().unwrap()))
                .unwrap_or_else(|| plan.destination.join(&main.relative));
            lock()
                .rating_targets
                .push((main.source.clone(), destination));
        }
    }
}

fn apply_ratings<P: FnMut(ImportProgress)>(
    plan: &Plan,
    require_network: bool,
    stop: &Stop,
    settings: &HashMap<PathBuf, rating::Settings>,
    shared: &mut Shared<P>,
    summary: &mut rating::RatingSummary,
) {
    // Read each folder once, including sidecars whose extension is upper-case.
    let mut folders: HashMap<PathBuf, HashMap<String, PathBuf>> = HashMap::new();
    let mut unread = 0;
    for (source, raw) in std::mem::take(&mut shared.rating_targets) {
        if stop.user.load(Ordering::Relaxed) {
            break;
        }
        let outcome = (|| {
            check_destination(&plan.destination, require_network)?;
            if volume_info::volume_info(&plan.root).id != plan.card_id {
                return Err("The card changed before its initial ratings were written.".into());
            }
            let locked = rating::locked_on_card(&source).map_err(|e| e.to_string())?;
            let fallback = rating::Settings::default();
            let Some(settings) = settings.get(&source).or(locked.then_some(&fallback)) else {
                unread += 1;
                return Ok(());
            };
            let initial = rating::initial_rating(settings, locked);
            let folder = raw.parent().unwrap();
            if !folders.contains_key(folder) {
                let mut names = HashMap::new();
                for item in fs::read_dir(folder).map_err(|e| e.to_string())? {
                    let item = item.map_err(|e| e.to_string())?;
                    let key = item.file_name().to_string_lossy().to_lowercase();
                    if names.insert(key, item.path()).is_some() {
                        return Err(format!("Ambiguous file names in {}.", folder.display()));
                    }
                }
                folders.insert(folder.to_path_buf(), names);
            }
            let names = &folders[folder];
            let raw = names
                .get(&raw.file_name().unwrap().to_string_lossy().to_lowercase())
                .unwrap_or(&raw);
            let wanted = raw.with_extension("xmp");
            let sidecar = names
                .get(&wanted.file_name().unwrap().to_string_lossy().to_lowercase())
                .unwrap_or(&wanted);
            let relative = sidecar
                .strip_prefix(&plan.destination)
                .map_err(|e| e.to_string())?;
            inspect_relative(&plan.destination, relative)?;
            shared.emit("rating", relative);
            let written = rating::write_sidecar(sidecar, &initial)?;
            summary.record(&initial, written);
            Ok(())
        })();
        if let Err(warning) = outcome {
            shared.warnings.push(warning);
        }
    }
    if unread > 0 {
        let noun = if unread == 1 { "raw" } else { "raws" };
        shared.warnings.push(format!("Initial ratings skipped for {unread} {noun} whose settings couldn't be read. Import again to retry."));
    }
}

/// A sidecar whose photo or clip is already in `folder` belongs next to it: an
/// identical copy there is skipped, a different one (e.g. edited in Lightroom) is
/// kept, and a missing one is added.
fn sidecar_next_to(
    entry: &Entry,
    folder: &Path,
    root: &Path,
    verify: bool,
    stop: &Stop,
    buffer: &mut [u8],
    update: &mut impl FnMut(&'static str, u64, u64),
) -> Result<Done, String> {
    let target = folder.join(entry.relative.file_name().unwrap());
    match fs::symlink_metadata(&target) {
        Ok(meta) if meta.is_file() && !is_link(&meta) && meta.len() == entry.size => {
            update("checking_existing", 0, 0);
            match compare_existing(entry, &target, stop, buffer, &mut |p, r| {
                update("checking_existing", p, r)
            }) {
                Ok(()) => Ok(Done::Elsewhere(folder.to_path_buf())),
                Err(e) if e == STOPPED => Err(e),
                Err(_) => Ok(Done::KeptNas(target)),
            }
        }
        Ok(_) => Ok(Done::KeptNas(target)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            transfer(entry, root, &target, verify, stop, buffer, update).map(|_| Done::Copied)
        }
        Err(e) => Err(e.to_string()),
    }
}

fn check_cancel(stop: &Stop) -> Result<(), String> {
    stop.check()
}

fn check_destination(path: &Path, require_network: bool) -> Result<(), String> {
    if !path.is_absolute() || !path.is_dir() {
        return Err(
            "The NAS folder must be an existing absolute path. Connect the share first.".into(),
        );
    }
    if require_network && !matches!(nas::location(path), Some(nas::Location::Network(_))) {
        return Err("The destination is not a confirmed network share. Set the NAS folder to a UNC path or mapped network drive.".into());
    }
    Ok(())
}

fn plan(request: &ImportRequest, require_network: bool) -> Result<Plan, String> {
    if !request.root.is_absolute() || !request.root.is_dir() {
        return Err("The card is no longer available. Insert it and scan again.".into());
    }
    check_destination(&request.nas_root, require_network)?;
    let root = fs::canonicalize(&request.root).map_err(|e| e.to_string())?;
    let destination = fs::canonicalize(&request.nas_root).map_err(|e| e.to_string())?;
    if root.starts_with(&destination) || destination.starts_with(&root) {
        return Err("The card and destination must be separate folders.".into());
    }
    let card_id = volume_info::volume_info(&request.root).id;
    if request.expected_card_id.is_some() && request.expected_card_id != card_id {
        return Err(
            "A different card is now in the reader. Scan it again before importing.".into(),
        );
    }
    let mut selections = BTreeMap::new();
    for shoot in &request.shoots {
        let folder = naming::shoot_dir(shoot.date, &shoot.name);
        if folder
            .file_name()
            .unwrap()
            .to_string_lossy()
            .encode_utf16()
            .count()
            > 255
        {
            return Err("A shoot name is too long. Shorten it before importing.".into());
        }
        if selections.insert(shoot.date, folder).is_some() {
            return Err("A capture date was selected more than once.".into());
        }
    }
    if selections.is_empty() {
        return Err("Select at least one shoot to import.".into());
    }
    let files = scan::scan_card(&root).map_err(|e| format!("Couldn't scan the card: {e}"))?;
    let dates: BTreeSet<_> = files.iter().map(|f| f.captured.date()).collect();
    if selections.keys().any(|d| !dates.contains(d)) {
        return Err("A selected shoot is no longer on the card. Scan it again.".into());
    }
    let mut entries = Vec::new();
    let mut groups = Vec::new();
    let mut names = HashMap::new();
    let mut shoots = BTreeSet::new();
    let mut companions: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    // The date's other folders, where an earlier import may already hold these files.
    let mut others: HashMap<NaiveDate, Vec<destinations::FolderIndex>> = HashMap::new();
    for (date, folder) in &selections {
        let chosen = folder.file_name().unwrap().to_string_lossy().to_lowercase();
        let found = destinations::date_folders(&destination, *date)
            .into_iter()
            .filter(|f| f.name.to_lowercase() != chosen)
            .collect();
        others.insert(*date, found);
    }
    for file in &files {
        let Some(shoot) = selections.get(&file.captured.date()) else {
            continue;
        };
        if file.kind == scan::MediaKind::Video && !request.include_videos {
            continue;
        }
        let folder = if file.kind == scan::MediaKind::Video {
            shoot.join("video")
        } else {
            shoot.clone()
        };
        let Some(main) = add_entry(
            &root,
            &file.path,
            folder.join(file.path.file_name().unwrap()),
            &mut entries,
            &mut names,
        )?
        else {
            continue;
        };
        let inside = destinations::inside_shoot(file);
        entries[main].kind = Some(file.kind);
        entries[main].elsewhere = others[&file.captured.date()]
            .iter()
            .filter_map(|f| f.find(&inside, file.size).map(Path::to_path_buf))
            .collect();
        let mut group = vec![main];
        shoots.insert(shoot.clone());
        let parent = file.path.parent().unwrap();
        if !companions.contains_key(parent) {
            let mut sidecars = Vec::new();
            for item in fs::read_dir(parent).map_err(|e| e.to_string())? {
                let item = item.map_err(|e| e.to_string())?;
                let path = item.path();
                if path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("xmp") || e.eq_ignore_ascii_case("xml"))
                {
                    sidecars.push(path);
                }
            }
            companions.insert(parent.to_path_buf(), sidecars);
        }
        let stem = file.path.file_stem().unwrap().to_string_lossy();
        for sidecar in &companions[parent] {
            let name = sidecar.file_name().unwrap().to_string_lossy();
            let matches = name.eq_ignore_ascii_case(&format!("{stem}.xmp"))
                || (file.kind == scan::MediaKind::Video
                    && (name.eq_ignore_ascii_case(&format!("{stem}M01.XML"))
                        || name.eq_ignore_ascii_case(&format!("{stem}.xml"))));
            if matches
                && let Some(index) = add_entry(
                    &root,
                    sidecar,
                    folder.join(sidecar.file_name().unwrap()),
                    &mut entries,
                    &mut names,
                )?
            {
                group.push(index);
            }
        }
        groups.push(group);
    }
    if entries.is_empty() {
        return Err("No photos or enabled video clips were selected.".into());
    }
    // Detect duplicate names and unsafe destination paths before writing anything.
    let mut needed = 0u64;
    for entry in &entries {
        inspect_relative(&destination, &entry.relative)?;
        match fs::symlink_metadata(destination.join(&entry.relative)) {
            Ok(meta) if meta.is_file() && (entry.kind.is_none() || meta.len() == entry.size) => {}
            Ok(_) => {
                return Err(format!(
                    "{} already exists with a different size or type. Nothing was overwritten. Choose another shoot name.",
                    entry.relative.display()
                ));
            }
            // A file likely already in another folder for the date needs no space.
            Err(e) if e.kind() == io::ErrorKind::NotFound && entry.elsewhere.is_empty() => {
                needed += entry.size
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    check_space(&destination, needed)?;
    Ok(Plan {
        root,
        destination,
        entries,
        groups,
        shoots: shoots.into_iter().collect(),
        card_id,
    })
}

/// Adds a file to copy and returns its index, or `None` when it's already planned
/// (a sidecar shared by a RAW and a JPEG of the same shot).
fn add_entry(
    root: &Path,
    source: &Path,
    relative: PathBuf,
    entries: &mut Vec<Entry>,
    names: &mut HashMap<String, PathBuf>,
) -> Result<Option<usize>, String> {
    let resolved = fs::canonicalize(source).map_err(|e| e.to_string())?;
    if !resolved.starts_with(root) {
        return Err(format!("{} points outside the card.", source.display()));
    }
    let meta = fs::symlink_metadata(source).map_err(|e| e.to_string())?;
    if !meta.is_file() || is_link(&meta) {
        return Err(format!(
            "{} is not a regular original or sidecar.",
            source.display()
        ));
    }
    // Windows and most NAS shares compare filenames without case sensitivity.
    let key = relative.to_string_lossy().to_lowercase();
    if let Some(previous) = names.get(&key) {
        if previous == source {
            return Ok(None);
        } // RAW+JPEG can share one XMP.
        return Err(format!(
            "Two card files would use {}. Nothing was copied. Import them into separate shoot folders.",
            relative.display()
        ));
    }
    names.insert(key, source.to_path_buf());
    entries.push(Entry {
        source: source.to_path_buf(),
        relative,
        size: meta.len(),
        modified: meta.modified().ok(),
        kind: None,
        elsewhere: Vec::new(),
    });
    Ok(Some(entries.len() - 1))
}

fn is_link(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT (includes junctions)
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

/// Reject links/junctions inside the share and paths that could escape it.
fn inspect_relative(root: &Path, relative: &Path) -> Result<(), String> {
    let mut path = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err("Invalid destination path.".into());
        };
        path.push(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) if is_link(&meta) => {
                return Err(format!(
                    "{} is a link or junction. Choose a regular folder on the share.",
                    path.display()
                ));
            }
            Ok(meta) if path != root.join(relative) && !meta.is_dir() => {
                return Err(format!("{} is not a folder.", path.display()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

fn ensure_parent(root: &Path, relative: &Path) -> Result<(), String> {
    inspect_relative(root, relative)?;
    let mut path = root.to_path_buf();
    for component in relative.parent().unwrap().components() {
        path.push(component);
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("Couldn't create {}: {e}", path.display())),
        }
        let meta = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if !meta.is_dir() || is_link(&meta) {
            return Err(format!(
                "{} is not a regular destination folder.",
                path.display()
            ));
        }
    }
    Ok(())
}

fn check_space(path: &Path, needed: u64) -> Result<(), String> {
    let (free, _) = nas::space(path)
        .ok_or("Couldn't read the NAS's free space. Reconnect the share and retry.")?;
    if free < needed {
        Err(format!(
            "Not enough space on the NAS: {needed} bytes needed, {free} available."
        ))
    } else {
        Ok(())
    }
}

fn open_source(path: &Path) -> io::Result<File> {
    let mut options = File::options();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1); // FILE_SHARE_READ: prevent writes/replacement while copying.
    }
    options.open(path)
}

fn unchanged(meta: &Metadata, entry: &Entry) -> Result<(), String> {
    if meta.len() != entry.size || meta.modified().ok() != entry.modified {
        Err("The source changed after scanning. Scan the card again.".into())
    } else {
        Ok(())
    }
}

/// Copies one file (or checks an identical existing copy). `update` gets the phase,
/// progress through this file and the bytes read from the card so far.
fn transfer(
    entry: &Entry,
    root: &Path,
    destination: &Path,
    verify: bool,
    stop: &Stop,
    buffer: &mut [u8],
    update: &mut impl FnMut(&'static str, u64, u64),
) -> Result<bool, String> {
    if !fs::canonicalize(&entry.source)
        .map_err(|e| e.to_string())?
        .starts_with(root)
    {
        return Err("The source now points outside the card.".into());
    }
    let size = entry.size;
    let mut input = open_source(&entry.source).map_err(|e| e.to_string())?;
    unchanged(&input.metadata().map_err(|e| e.to_string())?, entry)?;
    let existing = match fs::symlink_metadata(destination) {
        Ok(meta) if meta.is_file() && !is_link(&meta) && meta.len() == size => true,
        Ok(_) => return Err(
            "The destination already exists with different data or type. Nothing was overwritten."
                .into(),
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.to_string()),
    };
    if existing {
        update("checking_existing", 0, 0);
        compare_existing(entry, destination, stop, buffer, &mut |processed, read| {
            update("checking_existing", processed, read)
        })?;
        return Ok(false);
    }
    check_space(destination.parent().unwrap(), size)?;
    // A unique file in the final directory keeps publication on the same volume.
    // Dropping it cleans up on errors and cancellation; a crash may leave a .part.
    let mut temp = tempfile::Builder::new()
        .prefix(".card-intake-")
        .suffix(".part")
        .tempfile_in(destination.parent().unwrap())
        .map_err(|e| e.to_string())?;
    update("copying", 0, 0);
    let divisor = if verify { 2 } else { 1 };
    let source_hash = read_stream(
        &mut input,
        Some(temp.as_file_mut()),
        size,
        stop,
        buffer,
        |n| update("copying", n / divisor, n),
    )?;
    unchanged(&input.metadata().map_err(|e| e.to_string())?, entry)?;
    if let Some(modified) = entry.modified {
        temp.as_file()
            .set_modified(modified)
            .map_err(|e| e.to_string())?;
    }
    temp.as_file().sync_all().map_err(|e| e.to_string())?;
    if verify {
        update("verifying", size / 2, size);
        // Reopen after flushing instead of verifying the write buffer.
        let mut target = File::open(temp.path()).map_err(|e| e.to_string())?;
        let target_hash = read_stream(&mut target, None, size, stop, buffer, |n| {
            update("verifying", size / 2 + n / 2, size)
        })?;
        if source_hash != target_hash {
            return Err("Verification failed: the NAS copy differs from the card. The incomplete copy was discarded.".into());
        }
    }
    check_cancel(stop)?;
    temp.persist_noclobber(destination).map_err(|e| {
        format!("Couldn't publish the completed copy (existing files are never replaced): {e}")
    })?;
    Ok(true)
}

fn compare_existing(
    entry: &Entry,
    destination: &Path,
    stop: &Stop,
    buffer: &mut [u8],
    update: &mut impl FnMut(u64, u64),
) -> Result<(), String> {
    let meta = fs::symlink_metadata(destination).map_err(|e| e.to_string())?;
    if !meta.is_file() || is_link(&meta) || meta.len() != entry.size {
        return Err(
            "The destination has a different size or type. Nothing was overwritten.".into(),
        );
    }
    let mut input = open_source(&entry.source).map_err(|e| e.to_string())?;
    unchanged(&input.metadata().map_err(|e| e.to_string())?, entry)?;
    let size = entry.size;
    let source_hash = read_stream(&mut input, None, size, stop, buffer, |n| update(n / 2, n))?;
    let mut target = open_source(destination).map_err(|e| e.to_string())?;
    let target_hash = read_stream(&mut target, None, size, stop, buffer, |n| {
        update(size / 2 + n / 2, size)
    })?;
    unchanged(&input.metadata().map_err(|e| e.to_string())?, entry)?;
    if source_hash != target_hash {
        return Err("An existing file has different contents. Nothing was overwritten. Choose another shoot name.".into());
    }
    stop.check()
}

/// Stream in bounded chunks, checking cancellation and the exact expected length.
fn read_stream(
    input: &mut File,
    mut output: Option<&mut File>,
    expected: u64,
    stop: &Stop,
    buffer: &mut [u8],
    mut progress: impl FnMut(u64),
) -> Result<u64, String> {
    let mut hash = Xxh64::new(0);
    let mut count = 0;
    loop {
        check_cancel(stop)?;
        let n = input.read(buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        count += n as u64;
        if count > expected {
            return Err("File grew while it was being read.".into());
        }
        hash.update(&buffer[..n]);
        if let Some(out) = output.as_deref_mut() {
            out.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
        }
        progress(count);
    }
    if count != expected {
        return Err("File was truncated or disconnected while being read.".into());
    }
    Ok(hash.digest())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        request: ImportRequest,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("card");
            let nas_root = dir.path().join("photos");
            fs::create_dir_all(root.join("DCIM/100SONY")).unwrap();
            fs::create_dir_all(&nas_root).unwrap();
            fs::write(
                root.join("DCIM/100SONY/DSC00001.ARW"),
                crate::exif::tests::sample(true),
            )
            .unwrap();
            Self {
                _dir: dir,
                request: ImportRequest {
                    root,
                    nas_root,
                    expected_card_id: None,
                    shoots: vec![ShootSelection {
                        date: NaiveDate::from_ymd_opt(2026, 11, 6).unwrap(),
                        name: "Tokyo 秋".into(),
                    }],
                    include_videos: true,
                    verify: true,
                    // One at a time, so the progress hooks below see a fixed order.
                    workers: 1,
                    camera: Some("ILCE-7RM6".into()),
                },
            }
        }

        fn folder(&self) -> PathBuf {
            self.request.nas_root.join("2026/2026-11-06_Tokyo-秋")
        }
        fn run(&self) -> ImportResult {
            run(&self.request, false, &AtomicBool::new(false), |_| {})
        }
        fn add(&self, path: &str, bytes: &[u8]) {
            let path = self.request.root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        fn parts(&self) -> Vec<PathBuf> {
            walkdir::WalkDir::new(&self.request.nas_root)
                .into_iter()
                .map(Result::unwrap)
                .filter(|e| e.path().extension().is_some_and(|e| e == "part"))
                .map(|e| e.into_path())
                .collect()
        }
    }

    const SIDECAR: &[u8] =
        br#"<NonRealTimeMeta><CreationDate value="2026-11-06T12:00:00+09:00"/></NonRealTimeMeta>"#;

    #[test]
    fn copies_and_verifies_photos_videos_and_companions_without_touching_the_card() {
        let f = Fixture::new();
        f.add("DCIM/100SONY/DSC00001.xmp", b"photo edits");
        f.add("M4ROOT/CLIP/C0001.MP4", b"video bytes");
        f.add("M4ROOT/CLIP/C0001M01.XML", SIDECAR);
        f.add("M4ROOT/SUB/C0001S03.MP4", b"proxy, not an original");
        let source = f.request.root.join("DCIM/100SONY/DSC00001.ARW");
        let original = fs::read(&source).unwrap();
        let modified = fs::metadata(&source).unwrap().modified().unwrap();
        let result = f.run();
        assert_eq!(result.status, ImportStatus::Complete, "{result:?}");
        assert_eq!(
            (
                result.copied_files,
                result.verified_files,
                result.total_files
            ),
            (4, 4, 4)
        );
        assert_eq!(result.completed_bytes, result.total_bytes);
        assert_eq!(fs::read(f.folder().join("DSC00001.ARW")).unwrap(), original);
        assert_eq!(
            fs::read(f.folder().join("DSC00001.xmp")).unwrap(),
            b"photo edits"
        );
        assert_eq!(
            fs::read(f.folder().join("video/C0001.MP4")).unwrap(),
            b"video bytes"
        );
        assert_eq!(
            fs::read(f.folder().join("video/C0001M01.XML")).unwrap(),
            SIDECAR
        );
        assert_eq!(fs::read(&source).unwrap(), original);
        assert_eq!(fs::metadata(&source).unwrap().modified().unwrap(), modified);
        assert_eq!(
            fs::metadata(f.folder().join("DSC00001.ARW"))
                .unwrap()
                .modified()
                .unwrap(),
            modified
        );
        assert!(f.parts().is_empty());
    }

    #[test]
    fn imports_a_nikon_z_9_cards_raws_and_sidecars() {
        let mut f = Fixture::new();
        // A Z 9 card: NEF raws in Nikon's own DCIM folder name, Lightroom's sidecar
        // beside them.
        fs::remove_file(f.request.root.join("DCIM/100SONY/DSC00001.ARW")).unwrap();
        let nef = crate::exif::tests::nikon_sample();
        f.add("DCIM/100NCZ_9/DSC_0001.NEF", &nef);
        f.add("DCIM/100NCZ_9/DSC_0001.xmp", b"nikon edits");
        f.request.shoots = vec![ShootSelection {
            date: NaiveDate::from_ymd_opt(2026, 11, 7).unwrap(),
            name: "Kyoto".into(),
        }];

        let result = f.run();
        assert_eq!(result.status, ImportStatus::Complete, "{result:?}");
        assert_eq!((result.photos, result.copied_files), (1, 2));
        assert_eq!(result.verified_files, 2);
        let folder = f.request.nas_root.join("2026/2026-11-07_Kyoto");
        assert_eq!(fs::read(folder.join("DSC_0001.NEF")).unwrap(), nef);
        assert_eq!(
            fs::read(folder.join("DSC_0001.xmp")).unwrap(),
            b"nikon edits"
        );
        assert!(f.parts().is_empty());
    }

    #[test]
    fn retries_hash_and_skip_identical_files_even_when_verification_is_disabled() {
        let mut f = Fixture::new();
        assert_eq!(f.run().status, ImportStatus::Complete);
        f.request.verify = false;
        let second = f.run();
        assert_eq!(second.status, ImportStatus::Complete, "{second:?}");
        assert_eq!(
            (
                second.copied_files,
                second.skipped_files,
                second.verified_files
            ),
            (0, 1, 1)
        );
        let dest = f.folder().join("DSC00001.ARW");
        let mut corrupt = fs::read(&dest).unwrap();
        corrupt[0] ^= 1;
        fs::write(&dest, &corrupt).unwrap();
        let third = f.run();
        assert_eq!(third.status, ImportStatus::Failed);
        assert!(third.error.unwrap().contains("different contents"));
        assert_eq!(fs::read(&dest).unwrap(), corrupt);
    }

    #[test]
    fn video_exclusion_and_unverified_copies_are_honored() {
        let mut f = Fixture::new();
        f.add("PRIVATE/M4ROOT/CLIP/C0001.MP4", b"video");
        f.add("PRIVATE/M4ROOT/CLIP/C0001M01.XML", SIDECAR);
        f.request.include_videos = false;
        f.request.verify = false;
        let result = f.run();
        assert_eq!(result.status, ImportStatus::Complete, "{result:?}");
        assert_eq!((result.copied_files, result.verified_files), (1, 0));
        assert!(!f.folder().join("video").exists());
    }

    #[test]
    fn date_selection_excludes_other_shoots() {
        let f = Fixture::new();
        f.add("M4ROOT/CLIP/C0001.MP4", b"yesterday's video");
        f.add(
            "M4ROOT/CLIP/C0001M01.XML",
            br#"<CreationDate value="2026-11-05T12:00:00+09:00"/>"#,
        );
        let result = f.run();
        assert_eq!(result.status, ImportStatus::Complete);
        assert_eq!(result.total_files, 1);
        assert!(!f.request.nas_root.join("2026/2026-11-05").exists());
    }

    #[test]
    fn duplicate_names_on_the_card_fail_before_any_copy() {
        let f = Fixture::new();
        f.add(
            "DCIM/101SONY/dsc00001.arw",
            &crate::exif::tests::sample(true),
        );
        let result = f.run();
        assert_eq!(result.status, ImportStatus::Failed);
        assert!(result.error.unwrap().contains("Two card files"));
        assert_eq!(fs::read_dir(&f.request.nas_root).unwrap().count(), 0);
    }

    #[test]
    fn shared_xmp_is_copied_once() {
        let f = Fixture::new();
        f.add(
            "DCIM/100SONY/DSC00001.JPG",
            &crate::exif::tests::sample(true),
        );
        f.add("DCIM/100SONY/DSC00001.XMP", b"shared edits");
        let result = f.run();
        assert_eq!(result.status, ImportStatus::Complete, "{result:?}");
        assert_eq!(result.total_files, 3);
    }

    #[test]
    fn cancellation_discards_the_active_file_and_retry_keeps_completed_files() {
        let f = Fixture::new();
        f.add(
            "DCIM/100SONY/DSC00002.ARW",
            &crate::exif::tests::sample(true),
        );
        let cancel = AtomicBool::new(false);
        let result = run(&f.request, false, &cancel, |p| {
            if p.phase == "copying" && p.completed_files == 1 && p.processed_bytes > 0 {
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert_eq!(result.status, ImportStatus::Cancelled, "{result:?}");
        assert_eq!(result.copied_files, 1);
        assert!(!f.folder().join("DSC00002.ARW").exists());
        assert!(f.parts().is_empty());
        let retry = f.run();
        assert_eq!(retry.status, ImportStatus::Complete, "{retry:?}");
        assert_eq!((retry.copied_files, retry.skipped_files), (1, 1));
    }

    #[test]
    fn readback_corruption_is_not_published() {
        let f = Fixture::new();
        let result = run(&f.request, false, &AtomicBool::new(false), |p| {
            if p.phase == "verifying" && p.processed_bytes == p.total_bytes / 2 {
                let part = f.parts().pop().unwrap();
                let mut bytes = fs::read(&part).unwrap();
                bytes[0] ^= 1;
                fs::write(part, bytes).unwrap();
            }
        });
        assert_eq!(result.status, ImportStatus::Failed, "{result:?}");
        assert!(result.error.unwrap().contains("Verification failed"));
        assert!(!f.folder().join("DSC00001.ARW").exists());
        assert!(f.parts().is_empty());
    }

    #[test]
    fn another_writer_cannot_be_overwritten_at_publication() {
        let f = Fixture::new();
        let dest = f.folder().join("DSC00001.ARW");
        let result = run(&f.request, false, &AtomicBool::new(false), |p| {
            if p.phase == "verifying" {
                fs::write(&dest, b"another import won").unwrap();
            }
        });
        assert_eq!(result.status, ImportStatus::Failed, "{result:?}");
        assert_eq!(fs::read(dest).unwrap(), b"another import won");
        assert!(f.parts().is_empty());
    }

    #[test]
    fn a_source_changed_since_planning_is_rejected() {
        let f = Fixture::new();
        let plan = plan(&f.request, false).unwrap();
        let entry = &plan.entries[0];
        fs::write(&entry.source, b"changed").unwrap();
        let cancel = AtomicBool::new(false);
        let stop = Stop {
            user: &cancel,
            failed: AtomicBool::new(false),
        };
        let error = transfer(
            entry,
            &plan.root,
            &f.folder().join("DSC00001.ARW"),
            true,
            &stop,
            &mut [0; 64],
            &mut |_, _, _| {},
        )
        .unwrap_err();
        assert!(error.contains("source changed"));
    }

    #[test]
    fn rejects_invalid_requests_and_local_production_destinations() {
        let mut f = Fixture::new();
        let result = import_card(&f.request, None, &AtomicBool::new(false), |_| {});
        assert_eq!(result.status, ImportStatus::Failed);
        assert!(result.error.unwrap().contains("network share"));
        f.request.expected_card_id = Some("not-this-card".into());
        assert!(f.run().error.unwrap().contains("different card"));
        f.request.expected_card_id = None;
        f.request.shoots.push(f.request.shoots[0].clone());
        assert!(f.run().error.unwrap().contains("more than once"));
        f.request.shoots.clear();
        assert!(f.run().error.unwrap().contains("Select at least one"));
        assert!(
            check_space(&f.request.nas_root, u64::MAX)
                .unwrap_err()
                .contains("Not enough space")
        );
        assert!(inspect_relative(&f.request.nas_root, Path::new("../outside")).is_err());
        f.request.nas_root = f.request.root.clone();
        assert!(f.run().error.unwrap().contains("separate folders"));
    }

    #[test]
    fn progress_is_monotonic_and_history_survives_reload() {
        let f = Fixture::new();
        let mut previous = 0;
        let result = run(&f.request, false, &AtomicBool::new(false), |p| {
            assert!(p.processed_bytes >= previous);
            assert!(p.processed_bytes <= p.total_bytes);
            previous = p.processed_bytes;
        });
        assert_eq!(previous, result.total_bytes);
        let path = f._dir.path().join("ledger.json");
        let mut ledger = crate::ledger::Ledger::default();
        ledger.record_import(result.clone());
        ledger.save(&path).unwrap();
        ledger.record_import(result);
        ledger.save(&path).unwrap();
        assert_eq!(crate::ledger::Ledger::load(&path).unwrap(), ledger);
        let old: crate::ledger::Ledger =
            serde_json::from_str(r#"{"cards":[],"cameras":[]}"#).unwrap();
        assert!(old.imports.is_empty());
    }

    fn add_photos(f: &Fixture, count: usize) {
        for i in 2..count + 2 {
            let mut bytes = crate::exif::tests::sample(true);
            // Different contents and sizes, so a mix-up would show.
            bytes.extend(std::iter::repeat_n(i as u8, i * 3001));
            f.add(&format!("DCIM/100SONY/DSC{i:05}.ARW"), &bytes);
        }
    }

    #[test]
    fn parallel_workers_copy_and_verify_every_file() {
        let mut f = Fixture::new();
        add_photos(&f, 15);
        f.request.workers = 4;
        let mut previous = 0;
        let mut events = 0;
        let result = run(&f.request, false, &AtomicBool::new(false), |p| {
            assert!(p.processed_bytes >= previous, "progress went backwards");
            assert!(p.processed_bytes <= p.total_bytes);
            previous = p.processed_bytes;
            events += 1;
        });
        assert_eq!(result.status, ImportStatus::Complete, "{result:?}");
        assert_eq!((result.copied_files, result.verified_files), (16, 16));
        assert_eq!(previous, result.total_bytes);
        assert!(events > 16);
        for entry in walkdir::WalkDir::new(f.request.root.join("DCIM")) {
            let entry = entry.unwrap();
            if entry.file_type().is_file() {
                let copy = f.folder().join(entry.file_name());
                assert_eq!(fs::read(copy).unwrap(), fs::read(entry.path()).unwrap());
            }
        }
        assert!(f.parts().is_empty());
        assert!(result.average_bytes_per_sec.is_some());
    }

    #[test]
    fn one_failing_file_stops_the_other_workers_and_reports_its_own_error() {
        let mut f = Fixture::new();
        add_photos(&f, 15);
        f.request.workers = 4;
        // An existing NAS file with the same size but different contents.
        let source = f.request.root.join("DCIM/100SONY/DSC00009.ARW");
        let mut clash = fs::read(&source).unwrap();
        clash[0] ^= 1;
        fs::create_dir_all(f.folder()).unwrap();
        fs::write(f.folder().join("DSC00009.ARW"), &clash).unwrap();
        let result = f.run();
        assert_eq!(result.status, ImportStatus::Failed, "{result:?}");
        let error = result.error.unwrap();
        assert!(
            error.contains("DSC00009.ARW") && error.contains("different contents"),
            "{error}"
        );
        assert_eq!(fs::read(f.folder().join("DSC00009.ARW")).unwrap(), clash);
        assert!(f.parts().is_empty());
    }

    #[test]
    fn cancelling_a_parallel_import_reports_stopped() {
        let mut f = Fixture::new();
        add_photos(&f, 15);
        f.request.workers = 4;
        let cancel = AtomicBool::new(false);
        let result = run(&f.request, false, &cancel, |p| {
            if p.completed_files >= 3 {
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert_eq!(result.status, ImportStatus::Cancelled, "{result:?}");
        assert!(result.error.unwrap().contains("Import stopped"));
        assert!(result.copied_files >= 3 && result.copied_files < 16);
        assert!(f.parts().is_empty());
    }

    fn photo(extra: usize) -> Vec<u8> {
        let mut bytes = crate::exif::tests::sample(true);
        bytes.extend(std::iter::repeat_n(7u8, extra));
        bytes
    }

    #[test]
    fn files_already_under_another_title_are_checked_and_skipped() {
        let mut f = Fixture::new();
        f.add("M4ROOT/CLIP/C0001.MP4", b"video bytes");
        f.add("M4ROOT/CLIP/C0001M01.XML", SIDECAR);
        let first = f.run();
        assert_eq!(first.status, ImportStatus::Complete, "{first:?}");
        assert_eq!(
            (first.photos, first.videos, first.camera.as_deref()),
            (1, 1, Some("ILCE-7RM6"))
        );

        // The same card again with one new photo, under a different shoot name.
        f.add("DCIM/100SONY/DSC00002.ARW", &photo(10));
        f.request.shoots[0].name = "Kyoto".into();
        let second = f.run();
        assert_eq!(second.status, ImportStatus::Complete, "{second:?}");
        assert_eq!(
            (
                second.copied_files,
                second.skipped_files,
                second.skipped_elsewhere
            ),
            (1, 3, 3)
        );
        assert_eq!(second.verified_files, second.total_files);
        assert_eq!(second.found_in, [PathBuf::from("2026/2026-11-06_Tokyo-秋")]);
        assert_eq!(
            second.shoots,
            [
                PathBuf::from("2026/2026-11-06_Kyoto"),
                PathBuf::from("2026/2026-11-06_Tokyo-秋")
            ]
        );
        let kyoto = f.request.nas_root.join("2026/2026-11-06_Kyoto");
        let names: Vec<_> = fs::read_dir(&kyoto)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["DSC00002.ARW"]);
        assert!(!f.folder().join("DSC00002.ARW").exists());
        assert!(f.parts().is_empty());
    }

    #[test]
    fn a_different_photo_with_the_same_name_goes_into_the_new_folder() {
        let mut f = Fixture::new();
        // Another camera's DSC00001.ARW, same size, already in the date's other folder.
        let card = fs::read(f.request.root.join("DCIM/100SONY/DSC00001.ARW")).unwrap();
        let mut other = card.clone();
        *other.last_mut().unwrap() ^= 1;
        fs::create_dir_all(f.folder()).unwrap();
        fs::write(f.folder().join("DSC00001.ARW"), &other).unwrap();
        f.request.shoots[0].name = "Kyoto".into();
        let result = f.run();
        assert_eq!(result.status, ImportStatus::Complete, "{result:?}");
        assert_eq!((result.copied_files, result.skipped_elsewhere), (1, 0));
        let kyoto = f
            .request
            .nas_root
            .join("2026/2026-11-06_Kyoto/DSC00001.ARW");
        assert_eq!(fs::read(kyoto).unwrap(), card);
        assert_eq!(fs::read(f.folder().join("DSC00001.ARW")).unwrap(), other);
    }

    #[test]
    fn a_clips_sidecar_follows_it_into_the_earlier_folder() {
        let mut f = Fixture::new();
        f.add("M4ROOT/CLIP/C0001.MP4", b"video bytes");
        f.add("M4ROOT/CLIP/C0001M01.XML", SIDECAR);
        f.add("M4ROOT/CLIP/C0002.MP4", b"second clip");
        f.add("M4ROOT/CLIP/C0002M01.XML", SIDECAR);
        assert_eq!(f.run().status, ImportStatus::Complete);
        let video = f.folder().join("video");
        // One sidecar went missing on the NAS, the other was changed there.
        fs::remove_file(video.join("C0001M01.XML")).unwrap();
        fs::write(video.join("C0002M01.XML"), b"edited on the NAS").unwrap();
        f.request.shoots[0].name = "Kyoto".into();
        let result = f.run();
        assert_eq!(result.status, ImportStatus::Complete, "{result:?}");
        assert_eq!(fs::read(video.join("C0001M01.XML")).unwrap(), SIDECAR);
        assert_eq!(
            fs::read(video.join("C0002M01.XML")).unwrap(),
            b"edited on the NAS"
        );
        assert_eq!(
            result
                .warnings
                .iter()
                .filter(|w| w.contains("differs from the card"))
                .count(),
            1,
            "{:?}",
            result.warnings
        );
        assert!(!f.request.nas_root.join("2026/2026-11-06_Kyoto").exists());
        assert_eq!(result.shoots, [PathBuf::from("2026/2026-11-06_Tokyo-秋")]);
    }

    #[test]
    fn generated_sidecars_do_not_conflict_on_same_or_different_title_reimports() {
        let mut f = Fixture::new();
        let source = f.request.root.join("DCIM/100SONY/DSC00001.ARW");
        let original = fs::read(&source).unwrap();
        let permissions = fs::metadata(&source).unwrap().permissions();
        let mut locked = permissions.clone();
        locked.set_readonly(true);
        fs::set_permissions(&source, locked).unwrap();
        let first = f.run();
        assert_eq!(first.status, ImportStatus::Complete, "{first:?}");
        assert_eq!(
            (
                first.ratings.rated,
                first.ratings.locked,
                first.ratings.stars[5]
            ),
            (1, 1, 1)
        );
        assert!(!f.request.root.join("DCIM/100SONY/DSC00001.xmp").exists());
        let sidecar = f.folder().join("DSC00001.xmp");
        assert!(
            fs::read_to_string(&sidecar)
                .unwrap()
                .contains("ci:Pick=\"True\"")
        );
        // The NAS file is writable: protection came from the card, not the copy.
        assert!(
            !fs::metadata(f.folder().join("DSC00001.ARW"))
                .unwrap()
                .permissions()
                .readonly()
        );
        let second = f.run();
        assert_eq!(
            (
                second.status,
                second.copied_files,
                second.skipped_files,
                second.ratings.kept
            ),
            (ImportStatus::Complete, 0, 1, 1)
        );
        f.request.shoots[0].name = "Another name".into();
        let third = f.run();
        assert_eq!(
            (
                third.status,
                third.copied_files,
                third.skipped_elsewhere,
                third.ratings.kept
            ),
            (ImportStatus::Complete, 0, 1, 1)
        );
        assert!(
            !f.request
                .nas_root
                .join("2026/2026-11-06_Another-name")
                .exists()
        );
        assert_eq!(fs::read(&source).unwrap(), original);
        assert!(f.parts().is_empty());
        fs::set_permissions(&source, permissions).unwrap();
    }

    #[test]
    fn changed_card_sidecars_are_kept_and_then_rated_after_copying() {
        let f = Fixture::new();
        let source = f.request.root.join("DCIM/100SONY/DSC00001.ARW");
        let permissions = fs::metadata(&source).unwrap().permissions();
        let mut locked = permissions.clone();
        locked.set_readonly(true);
        fs::set_permissions(&source, locked).unwrap();
        let card_xmp = b"<rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description xmlns:xmp='http://ns.adobe.com/xap/1.0/' xmp:Rating='3' xmlns:crs='http://ns.adobe.com/camera-raw-settings/1.0/' crs:Exposure2012='0.25'/></rdf:RDF>";
        f.add("DCIM/100SONY/DSC00001.XMP", card_xmp);
        let first = f.run();
        assert_eq!(first.status, ImportStatus::Complete, "{first:?}");
        assert_eq!((first.ratings.kept, first.ratings.stars[3]), (1, 1));
        let sidecar = f.folder().join("DSC00001.XMP");
        let text = fs::read_to_string(&sidecar).unwrap();
        assert!(text.contains("xmp:Rating='3'"));
        assert!(text.contains("crs:Exposure2012='0.25'"));
        assert!(text.contains("ci:Pick=\"True\""));
        assert_ne!(fs::metadata(&sidecar).unwrap().len(), card_xmp.len() as u64);
        let retry = f.run();
        assert_eq!(
            (
                retry.status,
                retry.copied_files,
                retry.skipped_files,
                retry.ratings.kept
            ),
            (ImportStatus::Complete, 0, 2, 1)
        );
        assert_eq!(
            fs::read(f.request.root.join("DCIM/100SONY/DSC00001.XMP")).unwrap(),
            card_xmp
        );
        let text = fs::read_to_string(&sidecar).unwrap();
        assert_eq!(text.matches("Card Intake|Locked in camera").count(), 1);
        assert!(f.parts().is_empty());
        fs::set_permissions(&source, permissions).unwrap();
    }

    #[test]
    fn missing_or_broken_exiftool_never_fails_a_copy() {
        for tool in [None, Some(Path::new("missing-exiftool"))] {
            let f = Fixture::new();
            let result =
                run_with_exiftool(&f.request, false, tool, &AtomicBool::new(false), |_| {});
            assert_eq!(result.status, ImportStatus::Complete, "{result:?}");
            assert_eq!((result.copied_files, result.ratings.rated), (1, 0));
            assert!(!result.warnings.is_empty());
            assert!(!f.folder().join("DSC00001.xmp").exists());
        }
    }

    #[test]
    fn cancellation_during_ratings_keeps_completed_files_and_allows_retry() {
        let f = Fixture::new();
        let source = f.request.root.join("DCIM/100SONY/DSC00001.ARW");
        let permissions = fs::metadata(&source).unwrap().permissions();
        let mut locked = permissions.clone();
        locked.set_readonly(true);
        fs::set_permissions(&source, locked).unwrap();
        let cancel = AtomicBool::new(false);
        let result = run(&f.request, false, &cancel, |p| {
            if p.phase == "rating" {
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert_eq!(result.status, ImportStatus::Cancelled);
        assert_eq!(result.copied_files, 1);
        let retry = f.run();
        assert_eq!(
            (retry.status, retry.skipped_files, retry.ratings.rated),
            (ImportStatus::Complete, 1, 1)
        );
        fs::set_permissions(source, permissions).unwrap();
    }
}
