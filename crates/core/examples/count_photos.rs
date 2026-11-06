//! Adds the shutter counts of photos kept outside the photo share — an old archive such
//! as "Nikon Transfer 2026 5" — to the shared shutter count database. Every photo is
//! opened read-only: nothing is moved, copied or changed.
//!
//!     cargo run -p card_intake_core --example count_photos -- <photo share> <folder>...
//!
//! Each folder under the given roots is read once, and skipped on later runs unless its
//! photos changed. The counts are merged into `<photo share>/.card-intake/shutter.sqlite`,
//! the database the app reads, the same way the app syncs with it; merging is idempotent,
//! so a run can be repeated and an interrupted run keeps what it already saved.

use card_intake_core::shutter::{self, FolderResult, LibraryFolder, ShutterDb, Store};
use chrono::NaiveDate;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

/// ExifTool runs at once. Reading over SMB waits on the network more than on the CPU.
const WORKERS: usize = 4;
/// Save to the shared database this often, so an interrupted run keeps its readings.
const SAVE_EVERY: Duration = Duration::from_secs(20);
/// Folder keys from outside the photo share start with this, so they can't collide with
/// the share's own relative paths.
const EXTRA_PREFIX: &str = "extra:";

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(share) = args.next().map(PathBuf::from) else {
        usage();
    };
    let roots: Vec<PathBuf> = args.map(PathBuf::from).collect();
    if roots.is_empty() {
        usage();
    }
    let Some(exiftool) = find_exiftool() else {
        eprintln!("ExifTool wasn't found. Put exiftool on PATH or set CARD_INTAKE_EXIFTOOL.");
        std::process::exit(1);
    };
    if !share.is_dir() {
        eprintln!(
            "{} isn't a folder. Connect the photo share first.",
            share.display()
        );
        std::process::exit(1);
    }
    if let Err(e) = run(&share, &roots, &exiftool) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn usage() -> ! {
    eprintln!("usage: count_photos <photo share> <folder>...");
    eprintln!(r#"example: count_photos \\NAS\photos "\\NAS\old\Nikon Transfer 2026 5""#);
    std::process::exit(2);
}

fn run(share: &Path, roots: &[PathBuf], exiftool: &Path) -> Result<(), String> {
    let database = shutter::shared_path(share);
    // A working copy in memory, filled from the shared database and synced back to it.
    let mut local = Store::in_memory();
    shutter::sync_shared(share, &mut local)
        .map_err(|e| format!("Couldn't read {} ({e}).", database.display()))?;
    let before = local.load().map_err(|e| e.to_string())?;
    let cancel = AtomicBool::new(false);
    let mut todo: Vec<LibraryFolder> = Vec::new();
    let mut seen = BTreeSet::new();
    let (mut folders_total, mut unchanged, mut photos_total) = (0usize, 0usize, 0usize);
    for root in roots {
        if !root.is_dir() {
            println!("Skipping {}: not a folder.", root.display());
            continue;
        }
        let folders = shutter::list_library(root, &cancel)
            .map_err(|e| format!("Couldn't list {} ({e}).", root.display()))?;
        for mut folder in folders {
            folder.relative = extra_key(root, &folder.relative);
            if !seen.insert(folder.relative.clone()) {
                continue;
            }
            folders_total += 1;
            photos_total += folder.photos.len();
            if before.folders.get(&folder.relative) == Some(&folder.stamp) {
                unchanged += 1;
            } else {
                todo.push(folder);
            }
        }
    }
    println!(
        "{photos_total} photos in {folders_total} folders: {} to read, {unchanged} already read",
        todo.len()
    );
    if todo.is_empty() {
        println!("Nothing new. {}", database.display());
        return Ok(());
    }

    let started = Instant::now();
    let mut batch = ShutterDb::default();
    let mut saved = Instant::now();
    let (mut photos_read, mut counts, mut failures) = (0usize, 0usize, 0usize);
    shutter::read_folders(
        exiftool,
        todo,
        WORKERS,
        &cancel,
        |_progress| {},
        |result| match result {
            FolderResult::Read { folder, samples } => {
                photos_read += folder.photos.len();
                counts += samples.len();
                println!(
                    "  {}: {} photos, {} with a shutter count",
                    label(&folder.relative),
                    folder.photos.len(),
                    samples.len()
                );
                for sample in &samples {
                    batch.add(sample);
                }
                batch.folders.insert(folder.relative.clone(), folder.stamp);
                if saved.elapsed() >= SAVE_EVERY {
                    let synced = local
                        .add(&batch)
                        .and_then(|_| shutter::sync_shared(share, &mut local));
                    match synced {
                        Ok(()) => {
                            batch = ShutterDb::default();
                            saved = Instant::now();
                        }
                        Err(e) => eprintln!("  couldn't save yet ({e}); keeping counts in memory"),
                    }
                }
            }
            FolderResult::Failed { folder, error } => {
                failures += 1;
                eprintln!("  {}: {error}", label(&folder.relative));
            }
        },
    );

    local
        .add(&batch)
        .and_then(|_| shutter::sync_shared(share, &mut local))
        .map_err(|e| format!("Couldn't write {} ({e}).", database.display()))?;
    let merged = local.load().map_err(|e| e.to_string())?;
    println!();
    println!(
        "Read {} photos in {} ms: {} with a shutter count, {failures} folders failed",
        photos_read,
        started.elapsed().as_millis(),
        counts
    );
    report(&before, &merged);
    println!("Saved in {}", database.display());
    Ok(())
}

/// What each camera's history gained, and where it now stands.
fn report(before: &ShutterDb, after: &ShutterDb) {
    let mut added_days = 0usize;
    for camera in &after.cameras {
        let (Some(first), Some(last)) = (camera.days.first(), camera.days.last()) else {
            continue;
        };
        let old: BTreeSet<NaiveDate> = before
            .cameras
            .iter()
            .find(|c| c.model == camera.model && c.serial == camera.serial)
            .map(|c| c.days.iter().map(|d| d.date).collect())
            .unwrap_or_default();
        let added = camera
            .days
            .iter()
            .filter(|d| !old.contains(&d.date))
            .count();
        added_days += added;
        println!(
            "{} SN {}: {} days, {added} new · {} ({}) … {} ({})",
            camera.model,
            camera.serial.as_deref().unwrap_or("unknown"),
            camera.days.len(),
            first.min,
            first.date,
            last.max,
            last.date
        );
    }
    println!("{added_days} camera-days added");
}

/// A folder outside the photo share, keyed by its full path so it can't be confused with
/// a share-relative folder.
fn extra_key(root: &Path, relative: &str) -> String {
    let root = root.to_string_lossy().replace('\\', "/");
    format!("{EXTRA_PREFIX}{}/{}", root.trim_end_matches('/'), relative)
}

fn label(key: &str) -> &str {
    key.strip_prefix(EXTRA_PREFIX).unwrap_or(key)
}

/// `exiftool.exe` from `CARD_INTAKE_EXIFTOOL` or the PATH.
fn find_exiftool() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CARD_INTAKE_EXIFTOOL") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let name = if cfg!(windows) {
        "exiftool.exe"
    } else {
        "exiftool"
    };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}
