//! Verified copies used by backup drives and Send to NAS. Publication never
//! replaces a file, even if another writer creates it while we are copying.

use crate::{import::ImportProgress, nas};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, Metadata};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};
use xxhash_rust::xxh64::Xxh64;

pub const FAT32_MAX: u64 = (1u64 << 32) - 1;
pub const STOPPED: &str = "Stopped. Completed copies are kept; press again to resume.";
pub const SOURCE_MISMATCH: &str = "The laptop copy no longer matches the card's checksum.";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CopyFile {
    pub source: PathBuf,
    pub source_root: PathBuf,
    pub relative: PathBuf,
    pub size: u64,
    pub modified: SystemTime,
    /// An import supplies its card checksum, so the backup is checked against
    /// the card without opening the card a second time.
    pub hash: Option<u64>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct CopyCounts {
    pub copied: usize,
    pub skipped: usize,
    pub verified: usize,
    pub bytes: u64,
}

pub fn cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err(STOPPED.into())
    } else {
        Ok(())
    }
}

pub fn is_link(meta: &Metadata) -> bool {
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

pub fn skip(name: &str) -> bool {
    let name = name.to_lowercase();
    matches!(name.as_str(), "thumbs.db" | "desktop.ini" | ".ds_store")
        || name.starts_with(".card-intake-")
        || name.ends_with(".part")
}

/// Reject links and parent components on either side of a copy or deletion.
pub fn inspect(root: &Path, relative: &Path) -> Result<(), String> {
    let mut path = root.to_path_buf();
    let meta = fs::symlink_metadata(root).map_err(|e| e.to_string())?;
    if !meta.is_dir() || is_link(&meta) {
        return Err("Choose a regular folder, without links or junctions.".into());
    }
    for part in relative.components() {
        let Component::Normal(name) = part else {
            return Err("Invalid file path.".into());
        };
        path.push(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) if is_link(&meta) => {
                return Err(format!("{} is a link or junction.", path.display()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

pub fn file(root: &Path, relative: PathBuf) -> Result<CopyFile, String> {
    inspect(root, &relative)?;
    let source = root.join(&relative);
    let meta = fs::symlink_metadata(&source).map_err(|e| e.to_string())?;
    if !meta.is_file() || is_link(&meta) {
        return Err(format!("{} is not a regular file.", source.display()));
    }
    Ok(CopyFile {
        source,
        source_root: root.to_path_buf(),
        relative,
        size: meta.len(),
        modified: meta.modified().map_err(|e| e.to_string())?,
        hash: None,
    })
}

/// Empty folders have no entries. Links are refused rather than followed into
/// another disk; a read error fails the job instead of silently losing files.
pub fn files(root: &Path, relative: &Path) -> Result<Vec<CopyFile>, String> {
    inspect(root, relative)?;
    let dir = root.join(relative);
    if !dir.is_dir() {
        return Err(format!("{} is not available.", dir.display()));
    }
    let mut result = Vec::new();
    for entry in walkdir::WalkDir::new(&dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.path() == dir || !skip(&e.file_name().to_string_lossy()))
    {
        let entry = entry.map_err(|e| e.to_string())?;
        let meta = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
        if is_link(&meta) {
            return Err(format!("{} is a link or junction.", entry.path().display()));
        }
        if meta.is_file() {
            result.push(file(
                root,
                entry
                    .path()
                    .strip_prefix(root)
                    .map_err(|e| e.to_string())?
                    .to_path_buf(),
            )?);
        }
    }
    result.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(result)
}

pub fn open(path: &Path) -> Result<File, String> {
    let mut options = File::options();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1);
    }
    options
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

pub fn unchanged(file: &CopyFile) -> Result<(), String> {
    inspect(
        &file.source_root,
        file.source
            .strip_prefix(&file.source_root)
            .map_err(|e| e.to_string())?,
    )?;
    let meta = fs::symlink_metadata(&file.source).map_err(|e| e.to_string())?;
    if !meta.is_file()
        || is_link(&meta)
        || meta.len() != file.size
        || meta.modified().ok() != Some(file.modified)
    {
        Err(format!(
            "{} changed. Press again to check it.",
            file.source.display()
        ))
    } else {
        Ok(())
    }
}

fn stream(
    input: &mut File,
    mut output: Option<&mut File>,
    size: u64,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<u64, String> {
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut hash = Xxh64::new(0);
    let mut read = 0;
    loop {
        cancelled(cancel)?;
        let n = input.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        read += n as u64;
        if read > size {
            return Err("A file grew while it was being read.".into());
        }
        hash.update(&buf[..n]);
        if let Some(out) = output.as_deref_mut() {
            out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        }
        progress(read);
    }
    if read != size {
        return Err("A file was truncated or disconnected while being read.".into());
    }
    Ok(hash.digest())
}

pub fn hash(path: &Path, size: u64, cancel: &AtomicBool) -> Result<u64, String> {
    stream(&mut open(path)?, None, size, cancel, |_| {})
}

pub fn same_time(a: SystemTime, b: SystemTime) -> bool {
    a.duration_since(b)
        .or_else(|_| b.duration_since(a))
        .is_ok_and(|d| d.as_secs_f64() <= 2.0)
}

/// Backup may trust size and time; Send and import backups always check contents.
pub fn existing(
    file: &CopyFile,
    root: &Path,
    trust_time: bool,
    cancel: &AtomicBool,
) -> Result<bool, String> {
    match compare(file, root, trust_time, cancel)? {
        None => Ok(false),
        Some(true) => Ok(true),
        Some(false) => Err(conflict(&root.join(&file.relative))),
    }
}

pub fn conflict(path: &Path) -> String {
    format!(
        "{} has different contents. Nothing was overwritten.",
        path.display()
    )
}

/// None means missing; false means different contents. Read errors are always
/// errors, so Send cannot mistake an unreadable sidecar for an edited one.
pub fn compare(
    file: &CopyFile,
    root: &Path,
    trust_time: bool,
    cancel: &AtomicBool,
) -> Result<Option<bool>, String> {
    inspect(root, &file.relative)?;
    unchanged(file)?;
    let target = root.join(&file.relative);
    let meta = match fs::symlink_metadata(&target) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    if !meta.is_file() || is_link(&meta) {
        return Err(format!("{} is not a regular file.", target.display()));
    }
    if meta.len() == file.size {
        if trust_time
            && file.hash.is_none()
            && meta.modified().is_ok_and(|t| same_time(t, file.modified))
        {
            return Ok(Some(true));
        }
        let source_hash = match file.hash {
            Some(h) => h,
            None => hash(&file.source, file.size, cancel)?,
        };
        let target_hash = hash(&target, file.size, cancel)?;
        unchanged(file)?;
        return Ok(Some(source_hash == target_hash));
    }
    Ok(Some(false))
}

pub fn check_space(needed: u64, free: Option<u64>, name: &str) -> Result<(), String> {
    let free = free.ok_or_else(|| format!("Couldn't read free space on {name}."))?;
    if free < needed {
        Err(format!(
            "Not enough space on {name}: {needed} bytes needed, {free} available."
        ))
    } else {
        Ok(())
    }
}

pub fn check_large<'a>(
    files: impl IntoIterator<Item = &'a CopyFile>,
    filesystem: Option<&str>,
) -> Result<(), String> {
    if filesystem.is_some_and(|s| s.eq_ignore_ascii_case("FAT32")) {
        for file in files {
            if file.size > FAT32_MAX {
                return Err(format!(
                    "{} is over 4 GB and can't go on a FAT32 drive.",
                    file.relative.display()
                ));
            }
        }
    }
    Ok(())
}

/// A single checked copy. The caller rechecks the volume identity before calling.
pub fn copy(
    file: &CopyFile,
    root: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(&'static str, u64),
) -> Result<u64, String> {
    cancelled(cancel)?;
    inspect(root, &file.relative)?;
    unchanged(file)?;
    let target = root.join(&file.relative);
    let parent = target.parent().ok_or("Missing destination folder")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    inspect(root, &file.relative)?;
    check_space(file.size, nas::space(root).map(|s| s.0), "the destination")?;
    let mut input = open(&file.source)?;
    let mut temp = tempfile::Builder::new()
        .prefix(".card-intake-")
        .suffix(".part")
        .tempfile_in(parent)
        .map_err(|e| e.to_string())?;
    progress("copying", 0);
    let source_hash = stream(
        &mut input,
        Some(temp.as_file_mut()),
        file.size,
        cancel,
        |n| progress("copying", n / 2),
    )?;
    unchanged(file)?;
    if file.hash.is_some_and(|h| h != source_hash) {
        return Err(SOURCE_MISMATCH.into());
    }
    temp.as_file().sync_all().map_err(|e| e.to_string())?;
    progress("verifying", file.size / 2);
    let target_hash = stream(
        &mut File::open(temp.path()).map_err(|e| e.to_string())?,
        None,
        file.size,
        cancel,
        |n| progress("verifying", file.size / 2 + n / 2),
    )?;
    if source_hash != target_hash {
        return Err("Verification failed. The incomplete copy was discarded.".into());
    }
    temp.as_file()
        .set_modified(file.modified)
        .map_err(|e| e.to_string())?;
    temp.as_file().sync_all().map_err(|e| e.to_string())?;
    cancelled(cancel)?;
    inspect(root, &file.relative)?;
    temp.persist_noclobber(&target).map_err(|e| {
        format!(
            "Couldn't publish {} (existing files are never replaced): {e}",
            target.display()
        )
    })?;
    Ok(source_hash)
}

pub struct Progress<'a> {
    pub counts: CopyCounts,
    total: usize,
    bytes: u64,
    started: Instant,
    label: &'a str,
}

impl<'a> Progress<'a> {
    pub fn new(files: &[CopyFile], label: &'a str) -> Self {
        Self {
            counts: CopyCounts::default(),
            total: files.len(),
            bytes: files.iter().map(|f| f.size).sum(),
            started: Instant::now(),
            label,
        }
    }

    pub fn emit(
        &self,
        phase: &'static str,
        file: &CopyFile,
        current: u64,
        progress: &mut impl FnMut(ImportProgress),
    ) {
        let done = self.counts.bytes + current;
        let seconds = self.started.elapsed().as_secs_f64();
        let speed = (seconds >= 1.0).then(|| done as f64 / seconds);
        progress(ImportProgress {
            phase,
            current_file: Some(Path::new(self.label).join(&file.relative)),
            completed_files: self.counts.verified,
            skipped_files: self.counts.skipped,
            total_files: self.total,
            processed_bytes: done,
            total_bytes: self.bytes,
            bytes_per_sec: speed,
            eta_secs: speed
                .filter(|s| *s > 0.0)
                .map(|s| self.bytes.saturating_sub(done) as f64 / s),
            nas: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_readback_and_a_publication_race_never_replace_existing_files() {
        for corrupt in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("source");
            let dest = temp.path().join("dest");
            fs::create_dir(&source).unwrap();
            fs::create_dir(&dest).unwrap();
            fs::write(source.join("a.jpg"), b"photo").unwrap();
            let file = file(&source, PathBuf::from("a.jpg")).unwrap();
            let error = copy(&file, &dest, &AtomicBool::new(false), |phase, _| {
                if phase == "verifying" {
                    if corrupt {
                        let part = fs::read_dir(&dest).unwrap().next().unwrap().unwrap().path();
                        fs::write(part, b"other").unwrap();
                    } else {
                        fs::write(dest.join("a.jpg"), b"another writer").unwrap();
                    }
                }
            })
            .unwrap_err();
            if corrupt {
                assert!(error.contains("Verification failed"));
                assert!(!dest.join("a.jpg").exists());
            } else {
                assert_eq!(fs::read(dest.join("a.jpg")).unwrap(), b"another writer");
            }
            assert!(
                !fs::read_dir(&dest).unwrap().any(|e| e
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|e| e == "part"))
            );
        }
    }
}
