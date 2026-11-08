//! Card imports. Originals are opened read-only; destination files are published
//! only after a complete copy (and, by default, an xxHash64 read-back).
//!
//! Several files are copied at once. A CFexpress card answers parallel reads faster
//! than one stream, and while one file is read back from the NAS for verification,
//! the next ones are already being copied.
//!
//! An import to the NAS can use this computer as its buffer ([`ImportRequest::relay_to`]):
//! the card is read into the laptop at the card's full speed, and every verified photo
//! or clip is sent on to the NAS by separate workers at the same time, so the network
//! never holds the card back. The card counts as imported once the NAS has everything.

use crate::scan::MediaKind;
use crate::{backup, copies, destinations, naming, nas, offline, rating, scan, send, volume_info};
use chrono::{Local, NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
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
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub backup_drives: Vec<backup::Target>,
    /// The NAS root, when this computer is the import's buffer: `nas_root` is then the
    /// offline folder, the card is read into it at full speed, and each verified photo
    /// or clip goes on to the NAS at the same time. The laptop copy is deleted once the
    /// NAS has verified every file; if the NAS fails, it stays as an offline import.
    #[serde(default)]
    pub relay_to: Option<PathBuf>,
}

/// Photos and clips sent to the NAS at once while the card is read into the laptop.
const RELAY_WORKERS: usize = 4;

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
    /// Older history remains a NAS import. Unsent offline records are never trimmed.
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub offline_root: Option<PathBuf>,
    #[serde(default)]
    pub local_complete: bool,
    #[serde(default)]
    pub backups: Vec<backup::BackupResult>,
    #[serde(default)]
    pub sent: Option<crate::send::Sent>,
    #[serde(default)]
    pub laptop_deleted: bool,
    /// Handed to the desktop for its local file list, never stored in the ledger.
    #[serde(skip)]
    pub local_files: Vec<copies::CopyFile>,
    /// For an import through this computer whose laptop copy couldn't all be deleted:
    /// the files verified on the NAS, which Delete laptop copy may remove later.
    #[serde(skip)]
    pub sent_manifest: Option<send::Manifest>,
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
    /// The NAS side of an import that uses this computer as its buffer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nas: Option<NasProgress>,
}

/// How far the NAS is behind the card, when the card is read into this computer first.
#[derive(Debug, Clone, Serialize)]
pub struct NasProgress {
    pub completed_files: usize,
    pub total_files: usize,
    pub processed_bytes: u64,
    pub total_bytes: u64,
    pub bytes_per_sec: Option<f64>,
    pub eta_secs: Option<f64>,
}

/// The NAS side of an import through this computer: each photo or clip whose laptop
/// copy is verified is queued, and [`RELAY_WORKERS`] senders take it to the NAS.
struct Relay {
    nas: PathBuf,
    require_network: bool,
    cache: send::DateFolders,
    total_files: usize,
    total_bytes: u64,
    done_files: AtomicUsize,
    done_bytes: AtomicU64,
    /// Bytes of the file each sender is copying right now.
    in_flight: Vec<AtomicU64>,
    started: Instant,
    /// Every photo is queued; what's left is the NAS catching up.
    card_done: AtomicBool,
    failed: AtomicBool,
    error: Mutex<Option<String>>,
    sent: Mutex<HashMap<String, send::Relayed>>,
}

impl Relay {
    fn new(nas: &Path, require_network: bool, entries: &[Entry]) -> Self {
        let originals = entries.iter().filter(|e| e.kind.is_some());
        Self {
            nas: nas.to_path_buf(),
            require_network,
            cache: Default::default(),
            total_files: originals.clone().count(),
            total_bytes: originals.map(|e| e.size).sum(),
            done_files: AtomicUsize::new(0),
            done_bytes: AtomicU64::new(0),
            in_flight: (0..RELAY_WORKERS).map(|_| AtomicU64::new(0)).collect(),
            started: Instant::now(),
            card_done: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            error: Mutex::new(None),
            sent: Mutex::new(HashMap::new()),
        }
    }

    fn progress(&self) -> NasProgress {
        let processed = self.done_bytes.load(Ordering::Relaxed)
            + self
                .in_flight
                .iter()
                .map(|b| b.load(Ordering::Relaxed))
                .sum::<u64>();
        let seconds = self.started.elapsed().as_secs_f64();
        let speed = (seconds >= 1.0 && processed > 0).then(|| processed as f64 / seconds);
        NasProgress {
            completed_files: self.done_files.load(Ordering::Relaxed),
            total_files: self.total_files,
            processed_bytes: processed,
            total_bytes: self.total_bytes,
            bytes_per_sec: speed,
            eta_secs: speed.map(|s| self.total_bytes.saturating_sub(processed) as f64 / s),
        }
    }

    fn fail(&self, error: String) {
        self.error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(error);
        self.failed.store(true, Ordering::Relaxed);
    }
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
    Copied(u64),
    SidecarCopied(PathBuf, u64),
    /// An identical copy was already at the destination.
    Identical(u64),
    /// An identical copy is in another folder for the same date.
    Elsewhere(PathBuf, u64),
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
    verified: Vec<copies::CopyFile>,
    record_verified: bool,
    destination: PathBuf,
    completed_bytes: u64,
    completed_processed: u64,
    completed_read: u64,
    slots: Vec<Slot>,
    window: VecDeque<(Instant, u64, u64)>,
    started: Instant,
    error: Option<String>,
    relay: Option<Arc<Relay>>,
    /// Verified photos and clips for the NAS senders; dropped once the card is read.
    relay_queue: Option<mpsc::Sender<copies::CopyFile>>,
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
            nas: self.relay.as_ref().map(|r| r.progress()),
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
        let (path, hash) = match &done {
            Done::Copied(hash) | Done::Identical(hash) => {
                (self.destination.join(&entry.relative), Some(*hash))
            }
            Done::SidecarCopied(path, hash) => (path.clone(), Some(*hash)),
            Done::Elsewhere(folder, hash) => (
                folder.join(entry.relative.file_name().unwrap()),
                Some(*hash),
            ),
            Done::KeptNas(path) => (path.clone(), None),
        };
        if self.record_verified
            && let Ok(relative) = path.strip_prefix(&self.destination)
        {
            match copies::file(&self.destination, relative.to_path_buf()) {
                Ok(mut file) => {
                    file.hash = hash;
                    // Photos and clips go on to the NAS right away; sidecars follow
                    // once the card is read.
                    if entry.kind.is_some()
                        && let Some(queue) = &self.relay_queue
                    {
                        let _ = queue.send(file.clone());
                    }
                    self.verified.push(file);
                }
                Err(error) => {
                    self.error.get_or_insert(error);
                }
            }
        }
        match done {
            Done::Copied(_) | Done::SidecarCopied(_, _) => {
                self.copied_files += 1;
                if verify {
                    self.verified_files += 1;
                }
            }
            Done::Identical(_) => {
                self.skipped_files += 1;
                self.verified_files += 1;
            }
            Done::Elsewhere(folder, _) => {
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
    run_with_exiftool(request, !request.offline, true, exiftool, cancel, progress)
}

/// The same import engine for a local destination, without app settings or history.
pub fn import_to_folder(
    request: &ImportRequest,
    exiftool: Option<&Path>,
    cancel: &AtomicBool,
    progress: impl FnMut(ImportProgress) + Send,
) -> ImportResult {
    run_with_exiftool(request, false, false, exiftool, cancel, progress)
}

#[cfg(test)]
fn run(
    request: &ImportRequest,
    require_network: bool,
    cancel: &AtomicBool,
    progress: impl FnMut(ImportProgress) + Send,
) -> ImportResult {
    run_with_exiftool(
        request,
        require_network,
        require_network,
        None,
        cancel,
        progress,
    )
}

/// `require_network` is for the destination in `nas_root`; `nas_network` for the NAS
/// an import through this computer sends to.
fn run_with_exiftool(
    request: &ImportRequest,
    require_network: bool,
    nas_network: bool,
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
        offline: request.offline,
        offline_root: request.offline.then(|| request.nas_root.clone()),
        local_complete: false,
        backups: Vec::new(),
        sent: None,
        laptop_deleted: false,
        local_files: Vec::new(),
        sent_manifest: None,
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
        nas: None,
    });
    let stop = Stop {
        user: cancel,
        failed: AtomicBool::new(false),
    };
    // A buffer folder this computer can't use, or can't fit the card into.
    let no_buffer = request
        .relay_to
        .as_ref()
        .and_then(|_| offline::prepare(&request.nas_root).err());
    let plan = stop.check().and_then(|()| {
        if !request.backup_drives.is_empty() && (!request.offline || !request.verify) {
            return Err(
                "Backup drives require Copy to this computer and Check every file after copying."
                    .into(),
            );
        }
        if let Some(nas) = &request.relay_to {
            if !request.offline || !request.verify || !request.backup_drives.is_empty() {
                return Err("Importing through this computer needs its offline folder and file checks, and no backup drives.".into());
            }
            if let Some(error) = &no_buffer {
                return Err(error.clone());
            }
            check_destination(nas, nas_network)?;
        }
        if request.offline {
            offline::prepare(&request.nas_root)?;
        }
        let plan = plan(request, require_network)?;
        preflight_backups(request, &plan)?;
        if let Some(nas) = &request.relay_to {
            preflight_nas(&plan, nas)?;
        }
        Ok(plan)
    });
    // Without room on this computer, the card is copied straight to the NAS as before.
    if let Some(nas) = &request.relay_to
        && let Err(error) = &plan
        && (no_buffer.is_some() || error.starts_with(LOCAL_FULL))
    {
        let direct = ImportRequest {
            nas_root: nas.clone(),
            offline: false,
            relay_to: None,
            ..request.clone()
        };
        let mut result = run_with_exiftool(
            &direct,
            nas_network,
            nas_network,
            exiftool,
            cancel,
            progress,
        );
        result.warnings.insert(
            0,
            format!(
                "This computer couldn't hold the card ({}), so it was copied straight to the NAS.",
                no_buffer.as_deref().unwrap_or(error).trim_end_matches('.')
            ),
        );
        return result;
    }
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
            let relay = request
                .relay_to
                .as_ref()
                .map(|nas| Arc::new(Relay::new(nas, nas_network, &plan.entries)));
            let (queue, queued) = match relay {
                Some(_) => {
                    let (queue, queued) = mpsc::channel();
                    (Some(queue), Some(Mutex::new(queued)))
                }
                None => (None, None),
            };
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
                verified: Vec::new(),
                record_verified: request.offline,
                destination: plan.destination.clone(),
                completed_bytes: 0,
                completed_processed: 0,
                completed_read: 0,
                slots: vec![Slot::default(); workers],
                window: VecDeque::new(),
                started: Instant::now(),
                error: None,
                relay: relay.clone(),
                relay_queue: queue,
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
                let senders: Vec<_> = match (&relay, &queued) {
                    (Some(relay), Some(queued)) => (0..RELAY_WORKERS)
                        .map(|slot| {
                            let (relay, shared) = (&**relay, &shared);
                            scope.spawn(move || relay_files(slot, relay, queued, cancel, shared))
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                let copiers: Vec<_> = (0..workers)
                    .map(|worker| {
                        let (shared, next, stop, plan) = (&shared, &next, &stop, &plan);
                        scope.spawn(move || {
                            copy_files(worker, request, require_network, plan, next, stop, shared)
                        })
                    })
                    .collect();
                for copier in copiers {
                    let _ = copier.join();
                }
                // The card is read: the senders finish the queue, then stop.
                shared.lock().unwrap_or_else(|e| e.into_inner()).relay_queue = None;
                if let Some(relay) = &relay {
                    relay.card_done.store(true, Ordering::Relaxed);
                }
                let readings = metadata.join().unwrap_or_else(|_| rating::Readings {
                    warnings: vec![
                        "Initial raw ratings couldn't be read; import again to retry.".into(),
                    ],
                    ..Default::default()
                });
                for sender in senders {
                    let _ = sender.join();
                }
                readings
            });
            let mut shared = shared.into_inner().unwrap_or_else(|e| e.into_inner());
            shared.warnings.extend(readings.warnings);
            let copy_seconds = shared.started.elapsed().as_secs_f64();
            let targets = std::mem::take(&mut shared.rating_targets);
            // Through this computer: once the NAS has every file, ratings go into the
            // NAS sidecars as in a direct import, and the laptop copy is deleted.
            let mut sent = None;
            let mut nas_failed = None;
            if let Some(relay) = &relay
                && shared.error.is_none()
                && !cancel.load(Ordering::Relaxed)
            {
                let relayed = relay.error.lock().unwrap_or_else(|e| e.into_inner()).take();
                let outcome = match relayed {
                    Some(error) => Err(error),
                    None => finish_relay(
                        &plan,
                        relay,
                        &stop,
                        &readings.settings,
                        &targets,
                        &mut shared,
                        &mut result,
                    ),
                };
                match outcome {
                    Ok(done) => sent = Some(done),
                    Err(error) if cancel.load(Ordering::Relaxed) => {
                        shared.error.get_or_insert(error);
                    }
                    Err(error) => nas_failed = Some(error),
                }
            }
            if sent.is_none() {
                apply_ratings(
                    &plan,
                    &plan.destination,
                    require_network,
                    request.offline,
                    &stop,
                    &readings.settings,
                    targets,
                    &mut shared,
                    &mut result.ratings,
                );
            }
            if stop.user.load(Ordering::Relaxed) {
                shared.error.get_or_insert_with(|| STOPPED.to_string());
            }
            if sent.is_none()
                && request.offline
                && shared.error.is_none()
                && !cancel.load(Ordering::Relaxed)
            {
                match collect_local_files(&plan, std::mem::take(&mut shared.verified)) {
                    Ok(files) => {
                        result.local_files = files;
                        result.local_complete = true;
                    }
                    Err(error) => {
                        shared.error.get_or_insert(error);
                    }
                }
                if shared.error.is_none() && !request.backup_drives.is_empty() {
                    finish_backups(request, cancel, &mut shared, &mut result);
                }
                // The NAS failed: the laptop copy is an offline import, sent later.
                if shared.error.is_none()
                    && let Some(error) = nas_failed
                {
                    shared.error = Some(format!(
                        "This computer has a verified copy, but the NAS copy failed: {}. The card was not ejected. Press Send to NAS in Recent imports once the NAS is fixed, or Resume.",
                        error.trim_end_matches('.')
                    ));
                }
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
            if let (Some(relay), Some(sent)) = (&relay, &sent) {
                // The import now lives on the NAS, in the folders that hold its files.
                result.nas_root = relay.nas.clone();
                result.skipped_elsewhere = relay
                    .sent
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .values()
                    .filter(|r| r.found_in.is_some())
                    .count();
                result.found_in = sent.found_in.clone();
                let mut shoots: BTreeSet<PathBuf> = plan.shoots.iter().cloned().collect();
                shoots.extend(sent.found_in.iter().cloned());
                result.shoots = shoots
                    .into_iter()
                    .filter(|s| relay.nas.join(s).is_dir())
                    .collect();
                if result.laptop_deleted {
                    result.offline = false;
                    result.offline_root = None;
                } else {
                    // Some laptop files remain: Delete laptop copy offers them later.
                    result.sent = Some(sent.clone());
                }
            }
            result.warnings = shared.warnings;
            result.completed_bytes = shared.completed_bytes;
            if shared.completed_read > 0 && copy_seconds > 0.0 {
                result.average_bytes_per_sec =
                    Some((shared.completed_read as f64 / copy_seconds) as u64);
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
    for drive in &request.backup_drives {
        if !result.backups.iter().any(|b| b.uuid == drive.uuid) {
            result.backups.push(backup::BackupResult {
                label: drive.label.clone(),
                uuid: drive.uuid.clone(),
                status: result.status,
                counts: Default::default(),
                error: result.error.clone().map(|e| format!("Not copied: {e}")),
            });
        }
    }
    result.finished_at = Local::now().naive_local();
    result
}

/// One NAS sender: takes each verified laptop copy off the queue and sends it on,
/// until the card is read and the queue is empty. After a NAS error the rest of the
/// queue is left for Send to NAS, and the card copy carries on.
fn relay_files<P: FnMut(ImportProgress)>(
    slot: usize,
    relay: &Relay,
    queued: &Mutex<mpsc::Receiver<copies::CopyFile>>,
    cancel: &AtomicBool,
    shared: &Mutex<Shared<P>>,
) {
    let report = |file: &Path| {
        // The card workers report often; a busy lock only skips one update.
        if let Ok(mut shared) = shared.try_lock() {
            let phase = if relay.card_done.load(Ordering::Relaxed) {
                "sending"
            } else {
                shared.phase()
            };
            shared.emit(phase, file);
        }
    };
    let mut last = Instant::now();
    loop {
        let next = queued.lock().unwrap_or_else(|e| e.into_inner()).recv();
        let Ok(file) = next else {
            return;
        };
        if relay.failed.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
            continue;
        }
        let outcome = send::relay_file(
            &relay.nas,
            &file,
            relay.require_network,
            &relay.cache,
            cancel,
            |n| {
                relay.in_flight[slot].store(n, Ordering::Relaxed);
                if last.elapsed() >= Duration::from_millis(200) {
                    last = Instant::now();
                    report(&file.relative);
                }
            },
        );
        relay.in_flight[slot].store(0, Ordering::Relaxed);
        match outcome {
            Ok(done) => {
                relay.done_files.fetch_add(1, Ordering::Relaxed);
                relay.done_bytes.fetch_add(file.size, Ordering::Relaxed);
                relay
                    .sent
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(send::key(&file.relative), done);
            }
            Err(_) if cancel.load(Ordering::Relaxed) => {}
            Err(error) => relay.fail(format!("{}: {error}", file.relative.display())),
        }
        if last.elapsed() >= Duration::from_millis(200) || relay.card_done.load(Ordering::Relaxed) {
            last = Instant::now();
            report(&file.relative);
        }
    }
}

/// After the card is read and its photos and clips are on the NAS: sends the rest
/// (the card's sidecars), writes the initial ratings into the NAS sidecars as a direct
/// import does, then deletes the laptop copy of every file the NAS verified.
fn finish_relay<P: FnMut(ImportProgress)>(
    plan: &Plan,
    relay: &Relay,
    stop: &Stop,
    settings: &HashMap<PathBuf, rating::Settings>,
    targets: &[(PathBuf, PathBuf)],
    shared: &mut Shared<P>,
    result: &mut ImportResult,
) -> Result<send::Sent, String> {
    let local = collect_local_files(plan, shared.verified.clone())?;
    let manifest = send::Manifest {
        offline_root: plan.destination.clone(),
        shoots: plan
            .shoots
            .iter()
            .filter(|s| plan.destination.join(s).is_dir())
            .cloned()
            .collect(),
        files: local
            .iter()
            .map(|f| send::VerifiedFile {
                relative: f.relative.clone(),
                size: f.size,
                modified: f.modified,
            })
            .collect(),
    };
    let relayed = relay.sent.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let (card_files, card_bytes) = (shared.total_files, shared.total_bytes);
    let sent = send::send_import_with(
        &manifest,
        &[],
        &relay.nas,
        relay.require_network,
        stop.user,
        |p| {
            (shared.progress)(ImportProgress {
                phase: "sending",
                current_file: p.current_file,
                completed_files: card_files,
                skipped_files: 0,
                total_files: card_files,
                processed_bytes: card_bytes,
                total_bytes: card_bytes,
                bytes_per_sec: None,
                eta_secs: None,
                nas: Some(NasProgress {
                    completed_files: p.completed_files,
                    total_files: p.total_files,
                    processed_bytes: p.processed_bytes,
                    total_bytes: p.total_bytes,
                    bytes_per_sec: None,
                    eta_secs: None,
                }),
            })
        },
        &relayed,
        true,
    );
    let done = match (sent.status, sent.sent) {
        (ImportStatus::Complete, Some(done)) => done,
        _ => {
            return Err(sent
                .error
                .unwrap_or_else(|| "The NAS copy didn't finish.".into()));
        }
    };
    for path in &done.kept_sidecars {
        shared.warnings.push(format!(
            "{} differs from this card's; the NAS's version was kept.",
            path.display()
        ));
    }
    // Ratings go next to each raw where it is on the NAS, which may be the date's
    // other folder.
    let nas_targets = targets
        .iter()
        .filter_map(|(source, raw)| {
            let relative = raw.strip_prefix(&plan.destination).ok()?;
            let target = relayed
                .get(&send::key(relative))
                .map(|r| r.target.clone())
                .unwrap_or_else(|| relative.to_path_buf());
            let raw = relay.nas.join(target);
            raw.is_file().then_some((source.clone(), raw))
        })
        .collect();
    apply_ratings(
        plan,
        &relay.nas,
        relay.require_network,
        false,
        stop,
        settings,
        nas_targets,
        shared,
        &mut result.ratings,
    );
    // Only this import's files, and only those unchanged since the NAS verified them.
    let ours: BTreeSet<_> = local.iter().map(|f| send::key(&f.relative)).collect();
    match send::delete_laptop(&sent.manifest) {
        Ok(deleted) => {
            let kept: Vec<_> = deleted
                .kept
                .iter()
                .filter(|k| ours.contains(&send::key(k)))
                .collect();
            for error in &deleted.errors {
                shared
                    .warnings
                    .push(format!("Couldn't delete the laptop copy: {error}"));
            }
            if !kept.is_empty() {
                shared.warnings.push(format!(
                    "{} changed on this computer after it was sent, so its laptop copy was kept.",
                    kept.iter()
                        .map(|k| k.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            result.laptop_deleted = deleted.errors.is_empty() && kept.is_empty();
        }
        Err(error) => shared
            .warnings
            .push(format!("The laptop copy wasn't deleted: {error}")),
    }
    result.local_files = local;
    result.local_complete = true;
    result.offline_root = Some(plan.destination.clone());
    result.sent_manifest = Some(sent.manifest);
    Ok(done)
}

/// The NAS must have room for every photo and clip it doesn't hold yet (by name and
/// size, in the shoot folder or the date's other folders), before the card is read.
fn preflight_nas(plan: &Plan, nas: &Path) -> Result<(), String> {
    let mut folders: HashMap<NaiveDate, Vec<destinations::FolderIndex>> = HashMap::new();
    let mut needed = 0u64;
    for entry in &plan.entries {
        let there = fs::metadata(nas.join(&entry.relative))
            .is_ok_and(|m| m.is_file() && (entry.kind.is_none() || m.len() == entry.size));
        if there {
            continue;
        }
        let shoot: PathBuf = entry.relative.components().take(2).collect();
        if entry.kind.is_some()
            && let Ok(inside) = entry.relative.strip_prefix(&shoot)
            && let Ok(date) = send::shoot_date(&shoot)
        {
            let found = folders
                .entry(date)
                .or_insert_with(|| destinations::date_folders(nas, date));
            if found.iter().any(|f| f.find(inside, entry.size).is_some()) {
                continue;
            }
        }
        needed += entry.size;
    }
    copies::check_space(needed, nas::space(nas).map(|s| s.0), "the NAS")
}

/// The start of the error when this computer has no room for the import.
const LOCAL_FULL: &str = "Not enough space on this computer";

/// The local sidecars may have been edited or created by an earlier import.
/// They travel with their original even if ExifTool couldn't rate it this time.
fn local_companions(
    path: &Path,
    folders: &mut HashMap<PathBuf, HashMap<String, PathBuf>>,
) -> Result<Vec<PathBuf>, String> {
    let Some(kind) = scan::kind_of(path) else {
        return Ok(Vec::new());
    };
    let parent = path.parent().ok_or("Missing photo folder")?;
    if !folders.contains_key(parent) {
        let mut names = HashMap::new();
        if parent.is_dir() {
            for item in fs::read_dir(parent).map_err(|e| e.to_string())? {
                let item = item.map_err(|e| e.to_string())?;
                let key = item.file_name().to_string_lossy().to_lowercase();
                if names.insert(key, item.path()).is_some() {
                    return Err(format!("Ambiguous file names in {}.", parent.display()));
                }
            }
        }
        folders.insert(parent.to_path_buf(), names);
    }
    let stem = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    let mut wanted = vec![format!("{stem}.xmp")];
    if kind == MediaKind::Video {
        wanted.extend([format!("{stem}.xml"), format!("{stem}m01.xml")]);
    }
    Ok(wanted
        .into_iter()
        .filter_map(|name| folders[parent].get(&name).cloned())
        .collect())
}

fn collect_local_files(
    plan: &Plan,
    mut verified: Vec<copies::CopyFile>,
) -> Result<Vec<copies::CopyFile>, String> {
    let mut companions = Vec::new();
    let mut folders = HashMap::new();
    for file in &verified {
        for path in local_companions(&file.source, &mut folders)? {
            companions.push(copies::file(
                &plan.destination,
                path.strip_prefix(&plan.destination)
                    .map_err(|e| e.to_string())?
                    .to_path_buf(),
            )?);
        }
    }
    verified.extend(companions);
    let mut files = BTreeMap::new();
    for file in verified {
        let mut final_file = copies::file(&plan.destination, file.relative.clone())?;
        if file
            .relative
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("xmp") || e.eq_ignore_ascii_case("xml"))
        {
            final_file.hash = None;
        } else {
            copies::unchanged(&file)?;
            final_file.hash = file.hash;
        }
        files.insert(final_file.relative.clone(), final_file);
    }
    Ok(files.into_values().collect())
}

fn finish_backups<P: FnMut(ImportProgress)>(
    request: &ImportRequest,
    cancel: &AtomicBool,
    shared: &mut Shared<P>,
    result: &mut ImportResult,
) {
    let outcome = (|| {
        let mut files = result.local_files.clone();
        for file in &mut files {
            // Only final sidecars are rehashed. Original hashes still come from
            // the import's card read, without reading the card for the drives.
            if file.hash.is_none() {
                file.hash = Some(copies::hash(&file.source, file.size, cancel)?);
            }
        }
        for drive in &request.backup_drives {
            let copied = backup::back_up(&files, drive, cancel, &mut shared.progress);
            if copied.status != ImportStatus::Complete {
                if copied.error.as_deref() == Some(copies::SOURCE_MISMATCH)
                    || files.iter().any(|f| copies::unchanged(f).is_err())
                {
                    result.local_complete = false;
                    shared.error.get_or_insert_with(|| "The laptop copy changed after verification. Keep the card and check the laptop files before resuming. The card was not ejected.".into());
                } else {
                    shared.error.get_or_insert_with(|| format!("The laptop copy is complete, but {} failed: {}. The card was not ejected. Resume to retry the drive.", drive.label, copied.error.as_deref().unwrap_or("Backup stopped")));
                }
            }
            result.backups.push(copied);
        }
        Ok::<_, String>(())
    })();
    if let Err(error) = outcome {
        shared.error.get_or_insert(error);
    }
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
                if request.offline {
                    offline::validate(&plan.destination)?;
                }
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
                        Done::Elsewhere(_, hash) => Done::Identical(hash),
                        other => other,
                    });
                }
                if !entry.elsewhere.is_empty() && !destination.exists() {
                    for other in &entry.elsewhere {
                        update("checking_existing", 0, 0);
                        match compare_existing(entry, other, stop, &mut buffer, &mut |p, r| {
                            update("checking_existing", p, r)
                        }) {
                            Ok(hash) => {
                                return Ok(Done::Elsewhere(
                                    other.parent().unwrap().to_path_buf(),
                                    hash,
                                ));
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
                .map(|(copied, hash)| {
                    if copied {
                        Done::Copied(hash)
                    } else {
                        Done::Identical(hash)
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
                    if let (Some(_), Done::Elsewhere(folder, _)) = (entry.kind, &done) {
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

/// Writes the initial ratings into the sidecars beside `targets` (card raw, raw at
/// `destination`): the import's own destination, or the NAS for an import through
/// this computer. `local` is a destination on this computer.
#[allow(clippy::too_many_arguments)]
fn apply_ratings<P: FnMut(ImportProgress)>(
    plan: &Plan,
    destination: &Path,
    require_network: bool,
    local: bool,
    stop: &Stop,
    settings: &HashMap<PathBuf, rating::Settings>,
    targets: Vec<(PathBuf, PathBuf)>,
    shared: &mut Shared<P>,
    summary: &mut rating::RatingSummary,
) {
    // Read each folder once, including sidecars whose extension is upper-case.
    let mut folders: HashMap<PathBuf, HashMap<String, PathBuf>> = HashMap::new();
    let mut unread = 0;
    for (source, raw) in targets {
        if stop.user.load(Ordering::Relaxed) {
            break;
        }
        let outcome = (|| {
            check_destination(destination, require_network)?;
            if local {
                offline::validate(destination)?;
            }
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
                .strip_prefix(destination)
                .map_err(|e| e.to_string())?;
            inspect_relative(destination, relative)?;
            shared.emit("rating", relative);
            let written = rating::write_sidecar(sidecar, &initial)?;
            summary.record(&initial, written);
            if local && shared.record_verified {
                shared
                    .verified
                    .push(copies::file(destination, relative.to_path_buf())?);
            }
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
                Ok(hash) => Ok(Done::Elsewhere(folder.to_path_buf(), hash)),
                Err(e) if e == STOPPED => Err(e),
                Err(_) => Ok(Done::KeptNas(target)),
            }
        }
        Ok(_) => Ok(Done::KeptNas(target)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            transfer(entry, root, &target, verify, stop, buffer, update)
                .map(|(_, hash)| Done::SidecarCopied(target, hash))
        }
        Err(e) => Err(e.to_string()),
    }
}

fn check_cancel(stop: &Stop) -> Result<(), String> {
    stop.check()
}

pub(crate) fn check_destination(path: &Path, require_network: bool) -> Result<(), String> {
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
    if request.offline {
        offline::validate(&request.nas_root)?;
    }
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
        // Through this computer, the NAS's folders count, not the buffer's: every
        // file goes into the chosen local folder and the senders look on the NAS.
        let found = match request.relay_to {
            Some(_) => Vec::new(),
            None => destinations::date_folders(&destination, *date)
                .into_iter()
                .filter(|f| f.name.to_lowercase() != chosen)
                .collect(),
        };
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
    copies::check_space(
        needed,
        nas::space(&destination).map(|s| s.0),
        if request.offline {
            "this computer"
        } else {
            "the NAS"
        },
    )?;
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
    let name = if matches!(nas::location(path), Some(nas::Location::Network(_))) {
        "the NAS"
    } else {
        "this computer"
    };
    copies::check_space(needed, nas::space(path).map(|s| s.0), name)
}

/// Reserve room on every requested drive before reading any originals. Actual
/// checksums are collected during the local copy, rather than rereading the card
/// for this check. A little extra space covers newly written rating packets.
fn preflight_backups(request: &ImportRequest, plan: &Plan) -> Result<(), String> {
    let rating_room = plan
        .entries
        .iter()
        .filter(|e| e.kind == Some(MediaKind::Photo) && scan::uses_raw_sidecar(&e.source))
        .count() as u64
        * 65536;
    if request.offline {
        check_space(
            &plan.destination,
            rating_room
                + plan
                    .entries
                    .iter()
                    .filter(|e| {
                        !plan.destination.join(&e.relative).exists() && e.elsewhere.is_empty()
                    })
                    .map(|e| e.size)
                    .sum::<u64>(),
        )?;
    }
    if request.backup_drives.is_empty() {
        return Ok(());
    }
    let mut projected = BTreeMap::new();
    let mut folders = HashMap::new();
    for entry in &plan.entries {
        projected
            .entry(entry.relative.clone())
            .and_modify(|size: &mut u64| *size = (*size).max(entry.size))
            .or_insert(entry.size);
        let mut candidates = entry.elsewhere.clone();
        candidates.push(plan.destination.join(&entry.relative));
        for path in candidates {
            if let Ok(relative) = path.strip_prefix(&plan.destination) {
                projected
                    .entry(relative.to_path_buf())
                    .and_modify(|size| *size = (*size).max(entry.size))
                    .or_insert(entry.size);
            }
            for sidecar in local_companions(&path, &mut folders)? {
                let file = copies::file(
                    &plan.destination,
                    sidecar
                        .strip_prefix(&plan.destination)
                        .map_err(|e| e.to_string())?
                        .to_path_buf(),
                )?;
                projected
                    .entry(file.relative)
                    .and_modify(|size| *size = (*size).max(file.size))
                    .or_insert(file.size);
            }
        }
    }
    for drive in &request.backup_drives {
        drive.check()?;
        let facts = offline::disk(&drive.root)?;
        let mut needed = rating_room;
        for (relative, size) in &projected {
            copies::inspect(&drive.folder(), relative)?;
            let present = fs::metadata(drive.folder().join(relative))
                .is_ok_and(|m| m.is_file() && m.len() == *size);
            if !present {
                if facts
                    .filesystem
                    .as_deref()
                    .is_some_and(|s| s.eq_ignore_ascii_case("FAT32"))
                    && *size > copies::FAT32_MAX
                {
                    return Err(format!(
                        "{} is over 4 GB and can't go on {} (FAT32).",
                        relative.display(),
                        drive.label
                    ));
                }
                needed += size;
            }
        }
        copies::check_space(needed, facts.free, &drive.label)?;
    }
    Ok(())
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
) -> Result<(bool, u64), String> {
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
        let hash = compare_existing(entry, destination, stop, buffer, &mut |processed, read| {
            update("checking_existing", processed, read)
        })?;
        return Ok((false, hash));
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
    Ok((true, source_hash))
}

fn compare_existing(
    entry: &Entry,
    destination: &Path,
    stop: &Stop,
    buffer: &mut [u8],
    update: &mut impl FnMut(u64, u64),
) -> Result<u64, String> {
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
    stop.check()?;
    Ok(source_hash)
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

    fn backup_target(f: &Fixture) -> backup::Target {
        let root = f._dir.path().join("backup-drive");
        fs::create_dir(&root).unwrap();
        let facts = offline::DiskFacts {
            root: root.clone(),
            local: true,
            system: false,
            free: Some(u64::MAX),
            total: Some(u64::MAX),
            filesystem: Some("exFAT".into()),
            label: Some("Trip backup".into()),
        };
        let drive = backup::check_init_with(
            &f.request.nas_root,
            &facts,
            Some(&f.request.nas_root),
            |_| Ok(1.0),
        )
        .drive
        .unwrap();
        backup::Target {
            root,
            uuid: drive.uuid,
            label: drive.label,
        }
    }

    #[test]
    fn offline_import_keeps_layout_ratings_sidecars_and_history_flag() {
        let mut f = Fixture::new();
        f.request.offline = true;
        let source = f.request.root.join("DCIM/100SONY/DSC00001.ARW");
        let permissions = fs::metadata(&source).unwrap().permissions();
        let mut locked = permissions.clone();
        locked.set_readonly(true);
        fs::set_permissions(&source, locked).unwrap();
        f.add("M4ROOT/CLIP/C0001.MP4", b"clip");
        f.add("M4ROOT/CLIP/C0001M01.XML", SIDECAR);
        let r = import_card(&f.request, None, &AtomicBool::new(false), |_| {});
        fs::set_permissions(&source, permissions).unwrap();
        assert_eq!(r.status, ImportStatus::Complete, "{r:?}");
        assert!(r.offline && r.local_complete);
        assert_eq!(r.ratings.locked, 1);
        assert!(f.folder().join("DSC00001.xmp").is_file());
        assert_eq!(
            fs::read(f.folder().join("video/C0001M01.XML")).unwrap(),
            SIDECAR
        );
        let mut ledger = crate::ledger::Ledger::default();
        ledger.record_import(r);
        let path = f._dir.path().join("ledger.json");
        ledger.save(&path).unwrap();
        assert!(crate::ledger::Ledger::load(&path).unwrap().imports[0].offline);
        f.request.shoots[0].name = "Another".into();
        let lookup = destinations::find(
            &f.request.nas_root,
            &scan::scan_card(&f.request.root).unwrap(),
        );
        assert_eq!(lookup[0].suggested_title.as_deref(), Some("Tokyo-秋"));
        let second = import_card(&f.request, None, &AtomicBool::new(false), |_| {});
        assert_eq!(second.skipped_elsewhere, 3);
        assert!(!f.request.nas_root.join("2026/2026-11-06_Another").exists());
    }

    /// An import through this computer: the card goes into `laptop`, then to `nas`.
    fn through_laptop(f: &mut Fixture) -> (PathBuf, PathBuf) {
        let nas = f.request.nas_root.clone();
        let laptop = f._dir.path().join("laptop");
        f.request.offline = true;
        f.request.nas_root = laptop.clone();
        f.request.relay_to = Some(nas.clone());
        (laptop, nas)
    }

    /// Locked in camera: rated without ExifTool.
    fn lock(f: &Fixture) -> fs::Permissions {
        let source = f.request.root.join("DCIM/100SONY/DSC00001.ARW");
        let permissions = fs::metadata(&source).unwrap().permissions();
        let mut locked = permissions.clone();
        locked.set_readonly(true);
        fs::set_permissions(&source, locked).unwrap();
        permissions
    }

    fn unlock(f: &Fixture, permissions: fs::Permissions) {
        fs::set_permissions(
            f.request.root.join("DCIM/100SONY/DSC00001.ARW"),
            permissions,
        )
        .unwrap();
    }

    #[test]
    fn through_this_computer_the_nas_gets_everything_rated_and_the_laptop_copy_goes() {
        let mut f = Fixture::new();
        let (laptop, nas) = through_laptop(&mut f);
        f.add("M4ROOT/CLIP/C0001.MP4", b"clip");
        f.add("M4ROOT/CLIP/C0001M01.XML", SIDECAR);
        let permissions = lock(&f);
        let mut nas_progress = Vec::new();
        let r = run(&f.request, false, &AtomicBool::new(false), |p| {
            if let Some(n) = p.nas {
                nas_progress.push((n.completed_files, n.total_files));
            }
        });
        unlock(&f, permissions);
        assert_eq!(r.status, ImportStatus::Complete, "{r:?}");
        let shoot = nas.join("2026/2026-11-06_Tokyo-秋");
        assert_eq!(
            fs::read(shoot.join("DSC00001.ARW")).unwrap(),
            crate::exif::tests::sample(true)
        );
        assert_eq!(fs::read(shoot.join("video/C0001.MP4")).unwrap(), b"clip");
        assert_eq!(fs::read(shoot.join("video/C0001M01.XML")).unwrap(), SIDECAR);
        // The rating went into the NAS sidecar, as in a direct import.
        assert_eq!(r.ratings.locked, 1);
        let xmp = fs::read_to_string(shoot.join("DSC00001.xmp")).unwrap();
        assert!(xmp.contains("Locked in camera"), "{xmp}");
        // The laptop was only a buffer: nothing is left of the shoot there.
        assert!(r.laptop_deleted && !r.offline && r.sent.is_none(), "{r:?}");
        assert!(!laptop.join("2026/2026-11-06_Tokyo-秋").exists());
        assert_eq!(r.nas_root, nas);
        assert_eq!(r.shoots, vec![PathBuf::from("2026/2026-11-06_Tokyo-秋")]);
        assert_eq!(r.verified_files, r.total_files);
        assert!(
            nas_progress.iter().any(|(_, total)| *total == 2),
            "{nas_progress:?}"
        );
        assert!(f.parts().is_empty());
    }

    #[test]
    fn through_this_computer_four_readers_and_four_senders_deliver_every_file() {
        let mut f = Fixture::new();
        let (laptop, nas) = through_laptop(&mut f);
        f.request.workers = 4;
        add_photos(&f, 60);
        let r = run(&f.request, false, &AtomicBool::new(false), |_| {});
        assert_eq!(r.status, ImportStatus::Complete, "{r:?}");
        let shoot = nas.join("2026/2026-11-06_Tokyo-秋");
        for i in 2..62 {
            let mut bytes = crate::exif::tests::sample(true);
            bytes.extend(std::iter::repeat_n(i as u8, i * 3001));
            assert_eq!(
                fs::read(shoot.join(format!("DSC{i:05}.ARW"))).unwrap(),
                bytes
            );
        }
        assert_eq!(r.verified_files, r.total_files);
        assert!(r.laptop_deleted && !laptop.join("2026/2026-11-06_Tokyo-秋").exists());
        assert!(f.parts().is_empty());
    }

    #[test]
    fn through_this_computer_a_photo_already_in_another_nas_folder_stays_there() {
        let mut f = Fixture::new();
        let (laptop, nas) = through_laptop(&mut f);
        let earlier = nas.join("2026/2026-11-06_Earlier");
        fs::create_dir_all(&earlier).unwrap();
        fs::write(
            earlier.join("DSC00001.ARW"),
            crate::exif::tests::sample(true),
        )
        .unwrap();
        let permissions = lock(&f);
        let r = run(&f.request, false, &AtomicBool::new(false), |_| {});
        unlock(&f, permissions);
        assert_eq!(r.status, ImportStatus::Complete, "{r:?}");
        assert!(!nas.join("2026/2026-11-06_Tokyo-秋/DSC00001.ARW").exists());
        assert_eq!(r.skipped_elsewhere, 1);
        assert_eq!(r.found_in, vec![PathBuf::from("2026/2026-11-06_Earlier")]);
        assert_eq!(r.shoots, vec![PathBuf::from("2026/2026-11-06_Earlier")]);
        // Rated beside the raw where it is.
        assert!(earlier.join("DSC00001.xmp").is_file());
        assert!(r.laptop_deleted);
        assert!(!laptop.join("2026/2026-11-06_Tokyo-秋").exists());
    }

    #[test]
    fn through_this_computer_a_different_nas_photo_leaves_an_offline_import_to_send() {
        let mut f = Fixture::new();
        let (laptop, nas) = through_laptop(&mut f);
        let shoot = nas.join("2026/2026-11-06_Tokyo-秋");
        fs::create_dir_all(&shoot).unwrap();
        let other = vec![7u8; crate::exif::tests::sample(true).len()];
        fs::write(shoot.join("DSC00001.ARW"), &other).unwrap();
        let permissions = lock(&f);
        let r = run(&f.request, false, &AtomicBool::new(false), |_| {});
        unlock(&f, permissions);
        // The card isn't done: the NAS doesn't have it.
        assert_eq!(r.status, ImportStatus::Failed, "{r:?}");
        let error = r.error.clone().unwrap();
        assert!(
            error.contains("NAS copy failed") && error.contains("different contents"),
            "{error}"
        );
        assert_eq!(fs::read(shoot.join("DSC00001.ARW")).unwrap(), other);
        // The laptop copy is complete and rated, ready for Send to NAS.
        assert!(r.offline && r.local_complete && !r.laptop_deleted, "{r:?}");
        assert_eq!(r.nas_root, laptop);
        let local = laptop.join("2026/2026-11-06_Tokyo-秋");
        assert!(local.join("DSC00001.ARW").is_file());
        assert!(local.join("DSC00001.xmp").is_file());
        assert!(
            r.local_files
                .iter()
                .any(|f| f.relative.ends_with("DSC00001.xmp"))
        );
    }

    #[test]
    fn through_this_computer_an_edited_nas_sidecar_is_kept_and_nothing_stays_behind() {
        let mut f = Fixture::new();
        let (laptop, nas) = through_laptop(&mut f);
        let shoot = nas.join("2026/2026-11-06_Tokyo-秋");
        fs::create_dir_all(&shoot).unwrap();
        // Imported before and edited in Lightroom since.
        fs::write(shoot.join("DSC00001.ARW"), crate::exif::tests::sample(true)).unwrap();
        let edited = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="2"/></rdf:RDF></x:xmpmeta>"#;
        fs::write(shoot.join("DSC00001.xmp"), edited).unwrap();
        let permissions = lock(&f);
        let r = run(&f.request, false, &AtomicBool::new(false), |_| {});
        unlock(&f, permissions);
        assert_eq!(r.status, ImportStatus::Complete, "{r:?}");
        // The rating already there is kept, as in a direct import.
        let xmp = fs::read_to_string(shoot.join("DSC00001.xmp")).unwrap();
        assert!(xmp.contains(r#"xmp:Rating="2""#), "{xmp}");
        assert!(r.laptop_deleted, "{r:?}");
        assert!(!laptop.join("2026/2026-11-06_Tokyo-秋").exists());
    }

    #[test]
    fn without_room_on_this_computer_the_card_goes_straight_to_the_nas() {
        let mut f = Fixture::new();
        let (_, nas) = through_laptop(&mut f);
        // A file where the offline folder should be: this computer can't buffer.
        let blocked = f._dir.path().join("blocked");
        fs::write(&blocked, b"not a folder").unwrap();
        f.request.nas_root = blocked.join("offline");
        let r = run(&f.request, false, &AtomicBool::new(false), |_| {});
        assert_eq!(r.status, ImportStatus::Complete, "{r:?}");
        assert!(!r.offline);
        assert_eq!(r.nas_root, nas);
        assert!(nas.join("2026/2026-11-06_Tokyo-秋/DSC00001.ARW").is_file());
        assert!(
            r.warnings[0].contains("copied straight to the NAS"),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn import_backups_use_verified_local_files_and_final_rating_hashes() {
        let mut f = Fixture::new();
        f.request.offline = true;
        f.request.backup_drives = vec![backup_target(&f)];
        let source = f.request.root.join("DCIM/100SONY/DSC00001.ARW");
        let permissions = fs::metadata(&source).unwrap().permissions();
        let mut locked = permissions.clone();
        locked.set_readonly(true);
        fs::set_permissions(&source, locked).unwrap();
        let original = fs::read(&source).unwrap();
        let r = import_card(&f.request, None, &AtomicBool::new(false), |_| {});
        fs::set_permissions(&source, permissions).unwrap();
        assert_eq!(r.status, ImportStatus::Complete, "{r:?}");
        assert_eq!(r.backups[0].counts.verified, 2);
        let dest = f.request.backup_drives[0].folder().join(&r.shoots[0]);
        assert_eq!(fs::read(dest.join("DSC00001.ARW")).unwrap(), original);
        assert_eq!(
            fs::read(dest.join("DSC00001.xmp")).unwrap(),
            fs::read(f.folder().join("DSC00001.xmp")).unwrap()
        );
    }

    #[test]
    fn failing_drive_leaves_local_copy_complete_blocks_eject_and_resume_retries_it() {
        let mut f = Fixture::new();
        f.request.offline = true;
        f.request.backup_drives = vec![backup_target(&f)];
        let drive = &f.request.backup_drives[0];
        let marker = drive.folder();
        let disconnected = drive.root.join("unplugged");
        let r = import_card(&f.request, None, &AtomicBool::new(false), |p| {
            if p.phase == "file_complete" && p.completed_files == 1 && marker.exists() {
                fs::rename(&marker, &disconnected).unwrap();
            }
        });
        assert_eq!(r.status, ImportStatus::Failed, "{r:?}");
        assert!(r.local_complete);
        assert!(!r.ejected);
        assert!(f.folder().join("DSC00001.ARW").is_file());
        assert_eq!(r.backups[0].status, ImportStatus::Failed);
        fs::rename(&disconnected, &marker).unwrap();
        let retry = import_card(&f.request, None, &AtomicBool::new(false), |_| {});
        assert_eq!(retry.status, ImportStatus::Complete, "{retry:?}");
        assert_eq!(retry.skipped_files, 1);
        assert_eq!(retry.backups[0].counts.copied, 1);
    }

    #[test]
    fn import_backups_detect_a_corrupt_laptop_copy_against_card_hash_without_reading_card_again() {
        let mut f = Fixture::new();
        f.request.offline = true;
        f.request.backup_drives = vec![backup_target(&f)];
        let local = f.folder().join("DSC00001.ARW");
        let mut corrupted = false;
        let r = import_card(&f.request, None, &AtomicBool::new(false), |p| {
            if p.phase == "backup" && local.exists() && !corrupted {
                corrupted = true;
                let modified = fs::metadata(&local).unwrap().modified().unwrap();
                let mut bytes = fs::read(&local).unwrap();
                bytes[0] ^= 1;
                fs::write(&local, bytes).unwrap();
                fs::File::options()
                    .write(true)
                    .open(&local)
                    .unwrap()
                    .set_modified(modified)
                    .unwrap();
            }
        });
        assert_eq!(r.status, ImportStatus::Failed);
        assert!(!r.local_complete);
        assert!(!r.ejected);
        assert_eq!(r.backups[0].counts.verified, 0);
        assert!(
            r.backups[0].error.as_ref().unwrap().contains("checksum"),
            "{r:?}"
        );
    }

    #[test]
    fn backup_requests_require_offline_and_verification_before_copying() {
        let mut f = Fixture::new();
        f.request.backup_drives = vec![backup_target(&f)];
        assert_eq!(f.run().status, ImportStatus::Failed);
        f.request.offline = true;
        f.request.verify = false;
        assert_eq!(f.run().status, ImportStatus::Failed);
        assert!(!f.folder().exists());
    }

    #[test]
    fn offline_import_refuses_card_and_marker_destinations_without_writing_files() {
        let mut f = Fixture::new();
        f.request.offline = true;
        fs::create_dir(f.request.nas_root.join("DCIM")).unwrap();
        assert!(f.run().error.unwrap().contains("camera card"));
        assert!(!f.folder().exists());
        fs::remove_dir(f.request.nas_root.join("DCIM")).unwrap();
        let marker = f.request.nas_root.join(".card-intake-fake");
        fs::create_dir(&marker).unwrap();
        f.request.nas_root = marker.join("offline");
        assert!(f.run().error.unwrap().contains("backup drive"));
        assert!(!f.request.nas_root.exists());
    }

    #[test]
    fn backup_keeps_local_sidecars_next_to_originals_found_in_another_title() {
        let mut f = Fixture::new();
        f.request.offline = true;
        assert_eq!(f.run().status, ImportStatus::Complete);
        fs::write(f.folder().join("DSC00001.xmp"), b"edits kept locally").unwrap();
        f.request.backup_drives = vec![backup_target(&f)];
        f.request.shoots[0].name = "New title".into();
        let result = f.run();
        assert_eq!(result.status, ImportStatus::Complete, "{result:?}");
        assert_eq!(result.skipped_elsewhere, 1);
        assert_eq!(result.backups[0].counts.verified, 2);
        assert_eq!(
            fs::read(
                f.request.backup_drives[0]
                    .folder()
                    .join("2026/2026-11-06_Tokyo-秋/DSC00001.xmp")
            )
            .unwrap(),
            b"edits kept locally"
        );
    }

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
                    offline: false,
                    backup_drives: Vec::new(),
                    relay_to: None,
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
            let result = run_with_exiftool(
                &f.request,
                false,
                false,
                tool,
                &AtomicBool::new(false),
                |_| {},
            );
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
