//! Each Windows job holds a shared handle to `busy.lock` until its worker finishes.
//! The installer needs exclusive access from before it closes the app through setup.
//! `busy.json` reports job names and remains readable by older installers.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
static JOBS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// Where the file goes; one left by a copy of the app that stopped is removed.
pub fn open(dir: &Path) {
    let _ = fs::remove_file(dir.join("busy.json"));
    let _ = DIRECTORY.set(dir.to_path_buf());
}

pub struct Job {
    kind: &'static str,
    #[cfg(windows)]
    _lock: Option<fs::File>,
}

#[cfg(windows)]
fn job_lock(dir: &Path) -> std::io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;

    fs::create_dir_all(dir)?;
    fs::File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(3) // FILE_SHARE_READ | FILE_SHARE_WRITE. Keep the lock file in place.
        .open(dir.join("busy.lock"))
}

pub fn start(kind: &'static str) -> Result<Job, String> {
    #[cfg(windows)]
    let lock = DIRECTORY
        .get()
        .map(|dir| job_lock(dir))
        .transpose()
        .map_err(|e| {
            if e.raw_os_error()
                == Some(windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32)
            {
                "Card Intake is being updated. Try again after it reopens.".to_owned()
            } else {
                format!("Couldn't open the Card Intake job lock: {e}")
            }
        })?;
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    jobs.push(kind);
    write(&jobs);
    Ok(Job {
        kind,
        #[cfg(windows)]
        _lock: lock,
    })
}

impl Drop for Job {
    fn drop(&mut self) {
        let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = jobs.iter().position(|j| *j == self.kind) {
            jobs.remove(i);
        }
        write(&jobs);
    }
}

fn write(jobs: &[&str]) {
    let Some(dir) = DIRECTORY.get() else {
        return;
    };
    let path = dir.join("busy.json");
    let written = if jobs.is_empty() {
        match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    } else {
        fs::create_dir_all(dir).and_then(|()| {
            let file = tempfile::NamedTempFile::new_in(dir)?;
            serde_json::to_writer(
                &file,
                &serde_json::json!({ "pid": std::process::id(), "jobs": jobs, "lock": "busy.lock" }),
            )?;
            file.persist(&path)?;
            Ok(())
        })
    };
    if let Err(e) = written {
        super::log::line(format!("Couldn't update {}: {e}", path.display()));
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

    #[test]
    fn install_waits_for_the_last_job_then_excludes_new_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("busy.lock");
        let install = || {
            fs::File::options()
                .read(true)
                .write(true)
                .share_mode(0)
                .open(&path)
        };
        let busy = Some(ERROR_SHARING_VIOLATION as i32);

        let copy = job_lock(dir.path()).unwrap();
        let catalog = job_lock(dir.path()).unwrap();
        assert_eq!(install().unwrap_err().raw_os_error(), busy);
        drop(copy);
        assert_eq!(install().unwrap_err().raw_os_error(), busy);
        drop(catalog);

        let installer = install().unwrap();
        assert_eq!(job_lock(dir.path()).unwrap_err().raw_os_error(), busy);
        drop(installer);
        assert!(job_lock(dir.path()).is_ok());
    }
}
