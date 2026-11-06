//! Checks that the NAS folder is the right place before anything is copied there:
//! that it exists, sits on a network share rather than the local disk, takes a test
//! file that reads back identical, has room, and looks like the photo share.

use serde::Serialize;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CheckItem {
    pub status: Status,
    pub title: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NasCheck {
    pub path: String,
    /// True when nothing failed. Warnings still need a look.
    pub ok: bool,
    pub items: Vec<CheckItem>,
    pub free_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub write_mb_per_s: Option<f64>,
    pub years: Vec<String>,
    pub shoot_folders: usize,
}

impl NasCheck {
    fn add(&mut self, status: Status, title: &str, detail: impl Into<String>) {
        self.items.push(CheckItem {
            status,
            title: title.to_string(),
            detail: detail.into(),
        });
    }
}

const TEST_CHUNK: usize = 1 << 20;
const TEST_CHUNKS: usize = 64;
const LOW_SPACE_BYTES: u64 = 100_000_000_000;

pub fn check_nas(path_text: &str) -> NasCheck {
    let text = path_text.trim();
    let mut check = NasCheck {
        path: text.to_string(),
        ok: false,
        items: Vec::new(),
        free_bytes: None,
        total_bytes: None,
        write_mb_per_s: None,
        years: Vec::new(),
        shoot_folders: 0,
    };
    run_checks(&mut check, text);
    check.ok = !check.items.iter().any(|i| i.status == Status::Fail);
    check
}

fn run_checks(check: &mut NasCheck, text: &str) {
    if text.is_empty() {
        check.add(
            Status::Fail,
            "No folder entered",
            r"Type the photo share's path, e.g. /Volumes/photos on a Mac or \\NAS\photos on Windows.",
        );
        return;
    }
    let path = Path::new(text);
    if !path.is_absolute() {
        let detail = if cfg!(windows) {
            r"Use a full path such as \\NAS\photos or Z:\photos."
        } else {
            r"Use a full path such as /Volumes/photos. On a Mac, mount the share in Finder (Go > Connect to Server) and use its /Volumes path, not \\NAS\photos."
        };
        check.add(Status::Fail, "Not a full path", detail);
        return;
    }

    match fs::metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            check.add(
                Status::Fail,
                "Folder not found",
                "Is the share mounted? Connect to the NAS first, then check again.",
            );
            return;
        }
        Err(e) => {
            check.add(Status::Fail, "Can't open the folder", e.to_string());
            return;
        }
        Ok(meta) if !meta.is_dir() => {
            check.add(Status::Fail, "Not a folder", "This path points to a file.");
            return;
        }
        Ok(_) => check.add(Status::Pass, "Folder found", text),
    }

    match location(path) {
        Some(Location::Network(kind)) => {
            check.add(Status::Pass, "On a network share", kind);
        }
        Some(Location::Local(kind)) => check.add(
            Status::Warn,
            "On this computer's own disk",
            format!(
                "This folder is on a local {kind} disk, not on the NAS. If the share isn't mounted, photos would fill up this computer instead."
            ),
        ),
        None => {}
    }

    match write_test(path) {
        Ok(speed) => {
            check.write_mb_per_s = Some(speed);
            check.add(
                Status::Pass,
                "Test file written, read back and deleted",
                format!("64 MB came back identical. Write speed about {speed:.0} MB/s."),
            );
        }
        Err(e) => check.add(Status::Fail, "Test file failed", e),
    }

    if let Some((free, total)) = space(path) {
        check.free_bytes = Some(free);
        check.total_bytes = Some(total);
        let detail = format!("{} free of {}", human(free), human(total));
        if free < LOW_SPACE_BYTES {
            check.add(Status::Warn, "Little space left", detail);
        } else {
            check.add(Status::Pass, "Enough space", detail);
        }
    }

    let survey = survey(path);
    check.years = survey.years.clone();
    check.shoot_folders = survey.shoot_folders;
    if !survey.years.is_empty() {
        check.add(
            Status::Pass,
            "Looks like your photo share",
            format!(
                "Year folders {} · {} shoot folders",
                survey.years.join(", "),
                survey.shoot_folders
            ),
        );
    } else if survey.other_entries == 0 {
        check.add(
            Status::Warn,
            "The folder is empty",
            "Fine for a new share. Make sure it's the right folder before importing.",
        );
    } else {
        check.add(
            Status::Warn,
            "No year folders here",
            "Expected folders like 2026 at the top. Check that this is the share's top folder, not a subfolder.",
        );
    }
}

/// Writes 64 MB, reads it back, compares, deletes. Returns the write speed in MB/s.
fn write_test(dir: &Path) -> Result<f64, String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let file = dir.join(format!(
        ".card-intake-check-{}-{nanos}.tmp",
        std::process::id()
    ));
    let seed = (nanos as u64) | 1;

    let result = (|| -> io::Result<f64> {
        let mut buf = vec![0u8; TEST_CHUNK];
        let started = Instant::now();
        {
            let mut out = File::create(&file)?;
            let mut pattern = Pattern(seed);
            for _ in 0..TEST_CHUNKS {
                pattern.fill(&mut buf);
                out.write_all(&buf)?;
            }
            out.sync_all()?;
        }
        let seconds = started.elapsed().as_secs_f64().max(1e-6);

        let mut input = File::open(&file)?;
        bypass_cache(&input);
        let mut expected = vec![0u8; TEST_CHUNK];
        let mut pattern = Pattern(seed);
        for _ in 0..TEST_CHUNKS {
            pattern.fill(&mut expected);
            input.read_exact(&mut buf)?;
            if buf != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the test file came back different from what was written",
                ));
            }
        }
        if input.read(&mut buf)? != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the test file came back longer than what was written",
            ));
        }
        Ok((TEST_CHUNK * TEST_CHUNKS) as f64 / 1e6 / seconds)
    })();

    let removed = fs::remove_file(&file);
    match (result, removed) {
        (Ok(speed), Ok(())) => Ok(speed),
        (Ok(_), Err(e)) => Err(format!("Wrote the test file but couldn't delete it: {e}")),
        (Err(e), _) if e.kind() == io::ErrorKind::PermissionDenied => Err(format!(
            "No permission to write here ({e}). Check the share's permissions for your account."
        )),
        (Err(e), _) => Err(format!("Couldn't write and read back a test file: {e}")),
    }
}

/// xorshift64*: a fast, repeatable byte stream, so the read-back can be compared
/// without keeping the whole 64 MB in memory.
struct Pattern(u64);

impl Pattern {
    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            let value = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes();
            chunk.copy_from_slice(&value[..chunk.len()]);
        }
    }
}

/// Ask macOS to read from the share itself, not from the copy it still has in memory.
#[cfg(target_os = "macos")]
fn bypass_cache(file: &File) {
    use std::os::unix::io::AsRawFd;
    // SAFETY: plain fcntl on a valid, open file descriptor.
    unsafe {
        libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1);
    }
}

#[cfg(not(target_os = "macos"))]
fn bypass_cache(_file: &File) {}

struct Survey {
    years: Vec<String>,
    shoot_folders: usize,
    other_entries: usize,
}

/// Year folders (`2026`) and shoot folders inside them (`2026-11-02_...`).
/// Hidden and system folders (`.DS_Store`, Synology's `@eaDir` and `#recycle`) don't count.
fn survey(path: &Path) -> Survey {
    let mut survey = Survey {
        years: Vec::new(),
        shoot_folders: 0,
        other_entries: 0,
    };
    let Ok(entries) = fs::read_dir(path) else {
        return survey;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(['.', '@', '#']) || name == "_inbox" {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && name.len() == 4 && name.bytes().all(|b| b.is_ascii_digit()) {
            survey.shoot_folders += fs::read_dir(entry.path())
                .map(|inner| {
                    inner
                        .flatten()
                        .filter(|e| is_shoot_folder(&e.file_name().to_string_lossy()))
                        .count()
                })
                .unwrap_or(0);
            survey.years.push(name);
        } else {
            survey.other_entries += 1;
        }
    }
    survey.years.sort();
    survey
}

fn is_shoot_folder(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() >= 10
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
}

fn human(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb >= 1000.0 {
        format!("{:.1} TB", gb / 1000.0)
    } else {
        format!("{gb:.0} GB")
    }
}

pub enum Location {
    /// A network file system, e.g. "smbfs".
    Network(String),
    /// A local file system, e.g. "apfs".
    Local(String),
}

#[cfg(unix)]
fn c_path(path: &Path) -> Option<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes()).ok()
}

/// Free and total bytes of the volume holding `path`.
#[cfg(target_os = "macos")]
pub fn space(path: &Path) -> Option<(u64, u64)> {
    // statfs, not statvfs: macOS's statvfs uses 32-bit block counts that overflow on big NAS volumes.
    let s = statfs(path)?;
    let block = s.f_bsize as u64;
    Some((s.f_bavail * block, s.f_blocks * block))
}

#[cfg(target_os = "macos")]
fn statfs(path: &Path) -> Option<libc::statfs> {
    let c = c_path(path)?;
    // SAFETY: c is a valid C string and s is a zeroed statfs the call fills in.
    unsafe {
        let mut s: libc::statfs = std::mem::zeroed();
        (libc::statfs(c.as_ptr(), &mut s) == 0).then_some(s)
    }
}

#[cfg(target_os = "macos")]
pub fn location(path: &Path) -> Option<Location> {
    let s = statfs(path)?;
    // SAFETY: f_fstypename is a NUL-terminated C string filled in by statfs.
    let kind = unsafe { std::ffi::CStr::from_ptr(s.f_fstypename.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    Some(
        if matches!(kind.as_str(), "smbfs" | "nfs" | "afpfs" | "webdav" | "ftp") {
            Location::Network(kind)
        } else {
            Location::Local(kind)
        },
    )
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn space(path: &Path) -> Option<(u64, u64)> {
    let c = c_path(path)?;
    // SAFETY: c is a valid C string and s is a zeroed statvfs the call fills in.
    let s = unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        (libc::statvfs(c.as_ptr(), &mut s) == 0).then_some(s)?
    };
    let block = s.f_frsize as u64;
    Some((s.f_bavail as u64 * block, s.f_blocks as u64 * block))
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn location(path: &Path) -> Option<Location> {
    let c = c_path(path)?;
    // SAFETY: c is a valid C string and s is a zeroed statfs the call fills in.
    let s = unsafe {
        let mut s: libc::statfs = std::mem::zeroed();
        (libc::statfs(c.as_ptr(), &mut s) == 0).then_some(s)?
    };
    let magic = (s.f_type as u64) & 0xFFFF_FFFF;
    Some(match magic {
        0xFE53_4D42 => Location::Network("smb2".into()),
        0xFF53_4D42 => Location::Network("cifs".into()),
        0x517B => Location::Network("smb".into()),
        0x6969 => Location::Network("nfs".into()),
        _ => Location::Local(format!("0x{magic:x}")),
    })
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

#[cfg(windows)]
pub fn space(path: &Path) -> Option<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let p = wide(&path.to_string_lossy());
    let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
    // SAFETY: p is NUL-terminated and the three outputs are valid u64s.
    let ok = unsafe { GetDiskFreeSpaceExW(p.as_ptr(), &mut free, &mut total, &mut total_free) };
    (ok != 0).then_some((free, total))
}

#[cfg(windows)]
pub fn location(path: &Path) -> Option<Location> {
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    let text = path.to_string_lossy();
    if text.starts_with(r"\\") {
        return Some(Location::Network("network path".into()));
    }
    let drive: String = text.chars().take(2).collect();
    if !drive.ends_with(':') {
        return None;
    }
    let root = wide(&format!("{drive}\\"));
    // SAFETY: root is a NUL-terminated wide string.
    let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
    const DRIVE_REMOTE: u32 = 4;
    Some(if kind == DRIVE_REMOTE {
        Location::Network(format!("mapped drive {drive}"))
    } else {
        Location::Local(format!("drive {drive}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statuses(check: &NasCheck) -> Vec<(Status, String)> {
        check
            .items
            .iter()
            .map(|i| (i.status, i.title.clone()))
            .collect()
    }

    #[test]
    fn empty_and_relative_paths_fail() {
        let empty = check_nas("  ");
        assert!(!empty.ok);
        assert_eq!(empty.items[0].title, "No folder entered");

        let relative = check_nas("photos");
        assert!(!relative.ok);
        assert_eq!(relative.items[0].title, "Not a full path");
    }

    #[test]
    fn missing_folder_fails() {
        let dir = tempfile::tempdir().unwrap();
        let check = check_nas(dir.path().join("not-there").to_str().unwrap());
        assert!(!check.ok);
        assert_eq!(check.items[0].title, "Folder not found");
    }

    #[test]
    fn local_photo_share_passes_with_a_warning_about_the_local_disk() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("2025/2025-12-24_Christmas")).unwrap();
        fs::create_dir_all(dir.path().join("2026/2026-11-02_Yosemite")).unwrap();
        fs::create_dir_all(dir.path().join("2026/notes")).unwrap();
        fs::create_dir_all(dir.path().join("@eaDir")).unwrap();

        let check = check_nas(dir.path().to_str().unwrap());
        assert!(check.ok, "{check:#?}");
        assert_eq!(check.years, ["2025", "2026"]);
        assert_eq!(check.shoot_folders, 2);
        assert!(check.write_mb_per_s.is_some());

        let items = statuses(&check);
        assert!(items.contains(&(Status::Pass, "Folder found".into())));
        assert!(items.contains(&(Status::Warn, "On this computer's own disk".into())));
        assert!(items.contains(&(
            Status::Pass,
            "Test file written, read back and deleted".into()
        )));
        assert!(items.contains(&(Status::Pass, "Looks like your photo share".into())));

        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "the test file is deleted");
    }

    #[test]
    fn empty_folder_and_unrelated_folder_warn() {
        let empty = tempfile::tempdir().unwrap();
        fs::write(empty.path().join(".DS_Store"), b"x").unwrap();
        let check = check_nas(empty.path().to_str().unwrap());
        assert!(check.ok);
        assert!(statuses(&check).contains(&(Status::Warn, "The folder is empty".into())));

        let other = tempfile::tempdir().unwrap();
        fs::create_dir_all(other.path().join("Documents")).unwrap();
        let check = check_nas(other.path().to_str().unwrap());
        assert!(statuses(&check).contains(&(Status::Warn, "No year folders here".into())));
    }

    #[test]
    fn a_file_is_not_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("photos");
        fs::write(&file, b"x").unwrap();
        let check = check_nas(file.to_str().unwrap());
        assert!(!check.ok);
        assert_eq!(check.items[0].title, "Not a folder");
    }

    #[test]
    fn shoot_folder_names() {
        assert!(is_shoot_folder("2026-11-02_Yosemite"));
        assert!(is_shoot_folder("2026-11-02"));
        assert!(!is_shoot_folder("2026-11"));
        assert!(!is_shoot_folder("Yosemite"));
    }

    #[test]
    fn pattern_is_repeatable() {
        let (mut a, mut b) = (vec![0u8; 100], vec![0u8; 100]);
        Pattern(7).fill(&mut a);
        Pattern(7).fill(&mut b);
        assert_eq!(a, b);
        assert_ne!(a, vec![0u8; 100]);
    }
}
