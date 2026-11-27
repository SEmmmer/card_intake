//! The folder a camera sends photos to over FTP, such as
//! `\\NAS\FTP\A\DCIM\10261127\EM_04600.ARW`. A camera set to send each photo as it's
//! taken leaves a card holding photos the FTP folder already has.
//!
//! An import can leave those photos on the card ([`Plan::skip`]), and can move the
//! FTP folder's photos from the same camera and the selected dates into the shoot
//! folders ([`merge`]): sorted by capture date like a card's, under the camera's own
//! names, each copy read back before the FTP file is deleted.
//!
//! A photo is in the FTP folder when a file there has its name and size. An FTP server
//! that already has a name stores the next file as `EM_04408_1.JPG`; that counts as the
//! same name. Before a merge relies on an FTP copy instead of the card's, both are
//! hashed and must match.

use crate::scan::{MediaFile, MediaKind};
use crate::{copies, destinations, exif, rating, scan};
use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::SystemTime;
use walkdir::WalkDir;

/// Photos read at once: EXIF headers over SMB are a few small reads each, so the
/// round trips overlap.
const READERS: usize = 8;
/// Files hashed or moved at once.
const WORKERS: usize = 4;

/// What the import is asked to do with the FTP folder. Kept with a saved import, so a
/// resumed one does the same.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Options {
    pub root: PathBuf,
    /// Leave the card's photos that the FTP folder already has.
    pub skip: bool,
    /// Move the FTP folder's photos of this camera and the selected dates into the
    /// shoots. Needs `skip`.
    pub merge: bool,
}

/// The camera that took a photo, from its EXIF.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
struct Camera {
    model: Option<String>,
    serial: Option<String>,
}

impl Camera {
    fn of(exif: Option<&exif::BasicExif>) -> Self {
        Self {
            model: exif.and_then(|e| e.model.clone()),
            serial: exif.and_then(|e| e.serial.clone()),
        }
    }
}

/// A photo in the FTP folder.
#[derive(Debug, Clone)]
struct FtpPhoto {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
    captured: Option<NaiveDateTime>,
    camera: Camera,
}

/// A photo or clip on the card.
struct CardFile {
    file: MediaFile,
    /// Relative to the card's root, as [`crate::import::ImportRequest::skip`] wants it.
    relative: PathBuf,
    camera: Camera,
}

/// The name the camera gave a photo: `EM_04408_1.JPG`, the FTP server's second
/// `EM_04408.JPG`, is `EM_04408.JPG`. A camera numbers its files with four or more
/// digits, so `DSC_0001.JPG` keeps its name.
pub fn original_name(name: &str) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) => name.split_at(i),
        None => (name, ""),
    };
    if let Some((base, suffix)) = stem.rsplit_once('_') {
        let numbered = base.chars().rev().take_while(char::is_ascii_digit).count() >= 4;
        if numbered && (1..=3).contains(&suffix.len()) && suffix.chars().all(|c| c.is_ascii_digit())
        {
            return format!("{base}{ext}");
        }
    }
    name.to_string()
}

fn name_key(path: &Path) -> String {
    original_name(&path.file_name().unwrap_or_default().to_string_lossy()).to_lowercase()
}

/// Folders and files the FTP server or the NAS keeps for itself: `#recycle`, `@eaDir`,
/// `.DS_Store`, `._` files, and Card Intake's own `.part` copies.
fn hidden(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with(['.', '#', '@', '$'])
}

/// Runs `work` on every item, a few at a time, and returns the answers in order.
fn parallel<T: Sync, R: Send>(
    items: &[T],
    workers: usize,
    work: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let answers: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..workers.min(items.len()) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else {
                        break;
                    };
                    let answer = work(item);
                    answers.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(answer);
                }
            });
        }
    });
    answers
        .into_inner()
        .unwrap_or_else(|e| e.into_inner())
        .into_iter()
        .map(|a| a.expect("every item has an answer"))
        .collect()
}

/// Every photo in the FTP folder and its subfolders.
fn scan_ftp(root: &Path) -> io::Result<Vec<FtpPhoto>> {
    if !fs::metadata(root)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "The FTP folder is not a folder",
        ));
    }
    let mut found = Vec::new();
    let walk = WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !hidden(e.file_name()));
    for entry in walk {
        let entry = entry?;
        if !entry.file_type().is_file() || scan::kind_of(entry.path()) != Some(MediaKind::Photo) {
            continue;
        }
        let meta = entry.metadata()?;
        found.push((entry.into_path(), meta.len(), meta.modified()?));
    }
    let read = parallel(&found, READERS, |(path, _, _)| exif::read_basic(path).ok());
    Ok(found
        .into_iter()
        .zip(read)
        .map(|((path, size, modified), exif)| FtpPhoto {
            captured: exif.as_ref().and_then(|e| e.captured),
            camera: Camera::of(exif.as_ref()),
            path,
            size,
            modified,
        })
        .collect())
}

fn read_card(root: &Path) -> Result<Vec<CardFile>, String> {
    let files = scan::scan_card(root).map_err(|e| format!("Couldn't scan the card: {e}"))?;
    let cameras = parallel(&files, READERS, |f| match f.kind {
        MediaKind::Photo => Camera::of(exif::read_basic(&f.path).ok().as_ref()),
        MediaKind::Video => Camera::default(),
    });
    Ok(files
        .into_iter()
        .zip(cameras)
        .map(|(file, camera)| CardFile {
            relative: file
                .path
                .strip_prefix(root)
                .unwrap_or(&file.path)
                .to_path_buf(),
            file,
            camera,
        })
        .collect())
}

/// The FTP folder's photos grouped as the same photo (one name, one size), each
/// group's copy under the camera's own name first.
struct Groups {
    groups: Vec<Vec<usize>>,
    by_key: HashMap<(String, u64), usize>,
}

impl Groups {
    fn new(ftp: &[FtpPhoto]) -> Self {
        let mut groups: Vec<Vec<usize>> = Vec::new();
        let mut by_key = HashMap::new();
        for (i, photo) in ftp.iter().enumerate() {
            let group = *by_key
                .entry((name_key(&photo.path), photo.size))
                .or_insert_with(|| {
                    groups.push(Vec::new());
                    groups.len() - 1
                });
            groups[group].push(i);
        }
        for group in &mut groups {
            group.sort_by_key(|&i| {
                let name = ftp[i]
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                (original_name(&name) != name, ftp[i].path.clone())
            });
        }
        Self { groups, by_key }
    }

    /// The group holding the card's photo, if the FTP folder has it.
    fn of(&self, card: &CardFile) -> Option<usize> {
        (card.file.kind == MediaKind::Photo)
            .then(|| {
                self.by_key
                    .get(&(name_key(&card.file.path), card.file.size))
            })
            .flatten()
            .copied()
    }
}

/// One of the card's capture dates, as the import options show it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DateCounts {
    pub date: NaiveDate,
    /// The date's photos on the card that the FTP folder already has.
    pub on_card: usize,
    pub on_card_bytes: u64,
    /// Photos in the FTP folder from this card's camera, taken that day, that aren't
    /// on the card. Duplicates count once.
    pub ftp_only: usize,
    pub ftp_only_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Preview {
    pub root: PathBuf,
    /// Photos in the FTP folder, duplicates included.
    pub photos: usize,
    pub dates: Vec<DateCounts>,
}

/// What the FTP folder has of the card at `card_root`, by capture date.
pub fn preview(card_root: &Path, ftp_root: &Path) -> Result<Preview, String> {
    let card = read_card(card_root)?;
    let ftp = scan_ftp(ftp_root).map_err(|e| unreachable(ftp_root, &e))?;
    let groups = Groups::new(&ftp);
    let cameras: HashSet<&Camera> = card
        .iter()
        .filter(|c| c.file.kind == MediaKind::Photo)
        .map(|c| &c.camera)
        .collect();
    let mut dates: BTreeMap<NaiveDate, DateCounts> = BTreeMap::new();
    let mut twinned = HashSet::new();
    for file in &card {
        let date = file.file.captured.date();
        let counts = dates.entry(date).or_insert_with(|| DateCounts {
            date,
            ..DateCounts::default()
        });
        if let Some(group) = groups.of(file) {
            counts.on_card += 1;
            counts.on_card_bytes += file.file.size;
            twinned.insert(group);
        }
    }
    for (g, group) in groups.groups.iter().enumerate() {
        let photo = &ftp[group[0]];
        if twinned.contains(&g) || !cameras.contains(&photo.camera) {
            continue;
        }
        if let Some(counts) = photo.captured.and_then(|t| dates.get_mut(&t.date())) {
            counts.ftp_only += 1;
            counts.ftp_only_bytes += photo.size;
        }
    }
    Ok(Preview {
        root: ftp_root.to_path_buf(),
        photos: ftp.len(),
        dates: dates.into_values().collect(),
    })
}

fn unreachable(root: &Path, e: &io::Error) -> String {
    format!("Couldn't read the FTP folder {} ({e})", root.display())
}

/// An FTP file as it was when the import looked at it.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub path: PathBuf,
    pub size: u64,
    pub modified: SystemTime,
}

impl Source {
    fn of(photo: &FtpPhoto) -> Self {
        Self {
            path: photo.path.clone(),
            size: photo.size,
            modified: photo.modified,
        }
    }

    fn unchanged(&self) -> Result<(), String> {
        let meta = fs::metadata(&self.path).map_err(|e| format!("{}: {e}", self.path.display()))?;
        if meta.len() != self.size || meta.modified().ok() != Some(self.modified) {
            return Err(format!("{} changed in the FTP folder", self.path.display()));
        }
        Ok(())
    }
}

/// One photo to move from the FTP folder into a shoot.
#[derive(Debug, Clone, PartialEq)]
pub struct Move {
    /// Its copies in the FTP folder: the one copied first, then duplicates.
    pub sources: Vec<Source>,
    /// The name it gets in the shoot, the camera's own.
    pub name: String,
    pub captured: NaiveDateTime,
    /// The card's copy's xxHash64, when the card has it too.
    pub hash: Option<u64>,
    /// Protected in the camera (the card's copy is read-only): rated 5 stars.
    pub locked: bool,
}

#[derive(Debug, Default)]
pub struct Plan {
    /// The card's photos to leave on it, relative to the card's root.
    pub skip: Vec<PathBuf>,
    /// The photos and enabled clips the selected dates still have to copy from the card.
    pub left_on_card: usize,
    /// Oldest first.
    pub moves: Vec<Move>,
    pub warnings: Vec<String>,
}

/// What an import of `dates` does with the FTP folder. With `merge`, every card photo
/// it leaves is hashed against its FTP copy first, and `progress` gets
/// `(hashed, to hash)`.
pub fn plan(
    card_root: &Path,
    options: &Options,
    dates: &BTreeSet<NaiveDate>,
    include_videos: bool,
    cancel: &AtomicBool,
    progress: impl FnMut(usize, usize) + Send,
) -> Result<Plan, String> {
    if options.merge && !options.skip {
        return Err("Merging the FTP folder needs Skip photos already in FTP.".into());
    }
    let card = read_card(card_root)?;
    let ftp = scan_ftp(&options.root).map_err(|e| unreachable(&options.root, &e))?;
    let groups = Groups::new(&ftp);
    let selected: Vec<&CardFile> = card
        .iter()
        .filter(|c| dates.contains(&c.file.captured.date()))
        .collect();
    let twins: Vec<(&CardFile, usize)> = selected
        .iter()
        .filter_map(|c| groups.of(c).map(|g| (*c, g)))
        .collect();
    let mut plan = Plan::default();
    let mut used = HashSet::new();
    if !options.merge {
        plan.skip = twins.iter().map(|(c, _)| c.relative.clone()).collect();
    } else {
        let done = AtomicUsize::new(0);
        let progress = Mutex::new(progress);
        let total = twins.len();
        let hashes = parallel(&twins, WORKERS, |(card, group)| {
            let ftp_copy = &ftp[groups.groups[*group][0]];
            let card_hash = copies::hash(&card.file.path, card.file.size, cancel);
            let ftp_hash = copies::hash(&ftp_copy.path, ftp_copy.size, cancel);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            (progress.lock().unwrap_or_else(|e| e.into_inner()))(n, total);
            (card_hash, ftp_hash)
        });
        if cancel.load(Ordering::Relaxed) {
            return Err("Stopped.".into());
        }
        for ((card, group), (card_hash, ftp_hash)) in twins.iter().zip(hashes) {
            used.insert(*group);
            let card_hash = card_hash
                .map_err(|e| format!("Couldn't read {}: {e}", card.file.path.display()))?;
            let first = &ftp[groups.groups[*group][0]];
            let name = card
                .file
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            match ftp_hash {
                Ok(ftp_hash) if ftp_hash == card_hash => {
                    plan.skip.push(card.relative.clone());
                    plan.moves.push(Move {
                        sources: groups.groups[*group].iter().map(|&i| Source::of(&ftp[i])).collect(),
                        name: name.to_string(),
                        captured: card.file.captured,
                        hash: Some(card_hash),
                        locked: rating::locked_on_card(&card.file.path).unwrap_or(false),
                    });
                }
                Ok(_) => plan.warnings.push(format!(
                    "{name} in the FTP folder ({}) isn't the card's {name}: the card's was imported, and the FTP file left where it is.",
                    first.path.display()
                )),
                Err(e) => plan.warnings.push(format!(
                    "Couldn't read {} ({e}): {name} was imported from the card, and the FTP file left where it is.",
                    first.path.display()
                )),
            }
        }
        let cameras: HashSet<&Camera> = card
            .iter()
            .filter(|c| c.file.kind == MediaKind::Photo)
            .map(|c| &c.camera)
            .collect();
        for (g, group) in groups.groups.iter().enumerate() {
            let first = &ftp[group[0]];
            let Some(captured) = first.captured else {
                continue;
            };
            if used.contains(&g)
                || !cameras.contains(&first.camera)
                || !dates.contains(&captured.date())
            {
                continue;
            }
            plan.moves.push(Move {
                sources: group.iter().map(|&i| Source::of(&ftp[i])).collect(),
                name: original_name(&first.path.file_name().unwrap_or_default().to_string_lossy()),
                captured,
                hash: None,
                locked: false,
            });
        }
        plan.moves.sort_by(|a, b| {
            a.captured
                .cmp(&b.captured)
                .then_with(|| a.name.cmp(&b.name))
        });
    }
    let skipped: HashSet<&PathBuf> = plan.skip.iter().collect();
    plan.left_on_card = selected
        .iter()
        .filter(|c| match c.file.kind {
            MediaKind::Photo => !skipped.contains(&c.relative),
            MediaKind::Video => include_videos,
        })
        .count();
    Ok(plan)
}

/// What the import did with the FTP folder, kept in its history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub root: PathBuf,
    /// Card photos left on the card because the FTP folder has them.
    pub skipped: usize,
    /// Photos moved from the FTP folder into the shoots.
    pub moved: usize,
    /// FTP photos the shoots already had: deleted from the FTP folder, not copied.
    pub already_there: usize,
    /// Extra FTP copies of a moved photo, deleted.
    pub duplicates_removed: usize,
    /// FTP photos left where they are (see the warnings).
    pub left: usize,
}

#[derive(Debug, Default)]
pub struct Merged {
    pub summary: Summary,
    pub warnings: Vec<String>,
    pub ratings: rating::RatingSummary,
    /// The shoot folders that got photos, relative to the NAS root.
    pub shoots: BTreeSet<PathBuf>,
}

enum Placed {
    Copied(PathBuf),
    AlreadyThere,
}

/// Moves `moves` into the shoot folders of `nas_root` (`shoots` holds each date's,
/// relative to it), raws rated like a card's. A photo is copied (on one server, the
/// server copies it), its copy read back and compared, and only then is the FTP file
/// deleted. Folders left empty in the FTP folder are removed. `progress` gets
/// `(done, total, photo)`.
pub fn merge(
    nas_root: &Path,
    ftp_root: &Path,
    shoots: &BTreeMap<NaiveDate, PathBuf>,
    moves: &[Move],
    exiftool: Option<&Path>,
    cancel: &AtomicBool,
    progress: impl FnMut(usize, usize, &Path) + Send,
) -> Merged {
    let merged = Mutex::new(Merged {
        summary: Summary {
            root: ftp_root.to_path_buf(),
            ..Summary::default()
        },
        ..Merged::default()
    });
    let lock = || merged.lock().unwrap_or_else(|e| e.into_inner());
    let progress = Mutex::new(progress);
    let done = AtomicUsize::new(0);
    let removed = Mutex::new(Vec::new());
    let raws = Mutex::new(Vec::new());
    let folders: Mutex<HashMap<NaiveDate, std::sync::Arc<Vec<destinations::FolderIndex>>>> =
        Mutex::default();
    parallel(moves, WORKERS, |m| {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let report = || {
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            (progress.lock().unwrap_or_else(|e| e.into_inner()))(
                n,
                moves.len(),
                Path::new(&m.name),
            );
        };
        let Some(shoot) = shoots.get(&m.captured.date()) else {
            lock().summary.left += 1;
            report();
            return;
        };
        let others = folders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(m.captured.date())
            .or_insert_with(|| destinations::date_folders(nas_root, m.captured.date()).into())
            .clone();
        let outcome = place(nas_root, shoot, m, &others, cancel)
            .map(|(placed, hash)| (placed, delete_sources(m, hash, cancel)));
        match outcome {
            Ok((placed, (deleted, duplicates, warnings))) => {
                let mut merged = lock();
                match placed {
                    Placed::Copied(target) => {
                        merged.summary.moved += 1;
                        if scan::uses_raw_sidecar(&target) {
                            raws.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push((target, m.locked));
                        }
                    }
                    Placed::AlreadyThere => merged.summary.already_there += 1,
                }
                merged.shoots.insert(shoot.clone());
                merged.summary.duplicates_removed += duplicates;
                merged.summary.left += warnings.len();
                merged.warnings.extend(warnings);
                removed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(deleted);
            }
            Err(warning) => {
                let mut merged = lock();
                merged.summary.left += 1;
                if !cancel.load(Ordering::Relaxed) {
                    merged.warnings.push(warning);
                }
            }
        }
        report();
    });
    let mut merged = merged.into_inner().unwrap_or_else(|e| e.into_inner());
    let unvisited = moves.len() - done.load(Ordering::Relaxed);
    if unvisited > 0 {
        merged.summary.left += unvisited;
        merged.warnings.push(format!(
            "Stopped: {unvisited} FTP {} left where they are. Import again to move them.",
            if unvisited == 1 {
                "photo was"
            } else {
                "photos were"
            }
        ));
    }
    rate(
        raws.into_inner().unwrap_or_else(|e| e.into_inner()),
        exiftool,
        cancel,
        &mut merged,
    );
    remove_empty_folders(
        ftp_root,
        removed.into_inner().unwrap_or_else(|e| e.into_inner()),
    );
    merged
}

/// Puts the photo in its shoot, or finds it already there or in another folder for
/// the date. Returns the photo's hash too.
fn place(
    nas_root: &Path,
    shoot: &Path,
    m: &Move,
    others: &[destinations::FolderIndex],
    cancel: &AtomicBool,
) -> Result<(Placed, u64), String> {
    let first = &m.sources[0];
    let failed =
        |e: &dyn std::fmt::Display| format!("{} wasn't moved from the FTP folder: {e}", m.name);
    first.unchanged().map_err(|e| failed(&e))?;
    let dir = nas_root.join(shoot);
    fs::create_dir_all(&dir).map_err(|e| failed(&e))?;
    let target = dir.join(&m.name);
    let expected = match m.hash {
        Some(hash) => hash,
        None => copies::hash(&first.path, first.size, cancel).map_err(|e| failed(&e))?,
    };
    let same = |path: &Path| copies::hash(path, first.size, cancel).map(|h| h == expected);
    match fs::symlink_metadata(&target) {
        Ok(meta) => {
            if meta.is_file()
                && meta.len() == first.size
                && same(&target).map_err(|e| failed(&e))?
            {
                return Ok((Placed::AlreadyThere, expected));
            }
            return Err(format!(
                "{} is already in {} with different contents: the FTP file {} was left where it is.",
                m.name,
                shoot.display(),
                first.path.display()
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(failed(&e)),
    }
    // An earlier import under another name may hold it in another folder for the date.
    for folder in others.iter().filter(|f| f.path != dir) {
        if let Some(found) = folder.find(Path::new(&m.name), first.size)
            && same(found).map_err(|e| failed(&e))?
        {
            return Ok((Placed::AlreadyThere, expected));
        }
    }
    let temp = tempfile::Builder::new()
        .prefix(".card-intake-")
        .suffix(".part")
        .tempfile_in(&dir)
        .map_err(|e| failed(&e))?
        .into_temp_path();
    // Between two folders of one server, Windows has the server copy the file: the
    // photo doesn't cross the network.
    fs::copy(&first.path, &temp).map_err(|e| failed(&e))?;
    File::options()
        .write(true)
        .open(&temp)
        .and_then(|f| f.set_modified(first.modified))
        .map_err(|e| failed(&e))?;
    if !same(&temp).map_err(|e| failed(&e))? {
        return Err(failed(&"its copy didn't match, and was discarded"));
    }
    first.unchanged().map_err(|e| failed(&e))?;
    temp.persist_noclobber(&target)
        .map_err(|e| failed(&e.error))?;
    Ok((Placed::Copied(target), expected))
}

/// Deletes the photo's FTP copies once the shoot has it. A duplicate is deleted only
/// when it holds the same bytes. Returns the deleted files, how many were duplicates,
/// and a warning for each copy left.
fn delete_sources(m: &Move, hash: u64, cancel: &AtomicBool) -> (Vec<PathBuf>, usize, Vec<String>) {
    let (mut deleted, mut duplicates, mut warnings) = (Vec::new(), 0, Vec::new());
    for (i, source) in m.sources.iter().enumerate() {
        let checked = source.unchanged().and_then(|()| {
            if i == 0 {
                return Ok(());
            }
            match copies::hash(&source.path, source.size, cancel) {
                Ok(h) if h == hash => Ok(()),
                Ok(_) => Err(format!(
                    "{} has the name and size of {} but other contents",
                    source.path.display(),
                    m.name
                )),
                Err(e) => Err(e),
            }
        });
        match checked.and_then(|()| fs::remove_file(&source.path).map_err(|e| e.to_string())) {
            Ok(()) => {
                deleted.push(source.path.clone());
                duplicates += usize::from(i > 0);
            }
            Err(e) if !cancel.load(Ordering::Relaxed) => warnings.push(format!(
                "{} is in its shoot, but {} was left in the FTP folder: {e}",
                m.name,
                source.path.display()
            )),
            Err(_) => {}
        }
    }
    (deleted, duplicates, warnings)
}

/// Rates the moved raws as a card import does.
fn rate(
    raws: Vec<(PathBuf, bool)>,
    exiftool: Option<&Path>,
    cancel: &AtomicBool,
    merged: &mut Merged,
) {
    if raws.is_empty() {
        return;
    }
    let paths: Vec<PathBuf> = raws.iter().map(|(p, _)| p.clone()).collect();
    let readings = rating::read_settings(exiftool, &paths, cancel, |_, _| {});
    merged.warnings.extend(readings.warnings);
    for (raw, locked) in raws {
        let Some(settings) = readings
            .settings
            .get(&raw)
            .cloned()
            .or_else(|| locked.then(rating::Settings::default))
        else {
            continue;
        };
        let initial = rating::initial_rating(&settings, locked);
        match rating::write_sidecar(&raw.with_extension("xmp"), &initial) {
            Ok(written) => merged.ratings.record(&initial, written),
            Err(warning) => merged.warnings.push(warning),
        }
    }
}

/// Removes the folders the moved photos leave empty, up to the FTP folder itself.
fn remove_empty_folders(root: &Path, removed: Vec<PathBuf>) {
    let mut folders: Vec<PathBuf> = removed
        .iter()
        .filter_map(|p| p.parent().map(Path::to_path_buf))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    folders.sort_by_key(|f| std::cmp::Reverse(f.components().count()));
    for folder in folders {
        let mut folder = folder.as_path();
        while folder != root && folder.starts_with(root) && fs::remove_dir(folder).is_ok() {
            match folder.parent() {
                Some(parent) => folder = parent,
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests;
