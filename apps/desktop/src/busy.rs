//! `busy.json`, in the app's local data folder while Card Intake has a job running
//! (an import, a backup or Send, a delivery, a catalog's Finish): the app's process ID
//! and its jobs. The installer script waits for it to go before it closes the app.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static FILE: OnceLock<PathBuf> = OnceLock::new();
static JOBS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// Where the file goes; one left by a copy of the app that stopped is removed.
pub fn open(dir: &Path) {
    let path = dir.join("busy.json");
    let _ = fs::remove_file(&path);
    let _ = FILE.set(path);
}

/// A running job, in the file until it's dropped.
pub struct Job(&'static str);

pub fn start(kind: &'static str) -> Job {
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    jobs.push(kind);
    write(&jobs);
    Job(kind)
}

impl Drop for Job {
    fn drop(&mut self) {
        let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = jobs.iter().position(|j| *j == self.0) {
            jobs.remove(i);
        }
        write(&jobs);
    }
}

fn write(jobs: &[&str]) {
    let Some(path) = FILE.get() else {
        return;
    };
    let written = if jobs.is_empty() {
        match fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    } else {
        let text = serde_json::json!({ "pid": std::process::id(), "jobs": jobs }).to_string();
        fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))
            .and_then(|()| fs::write(path, text))
    };
    if let Err(e) = written {
        super::log::line(format!("Couldn't update {}: {e}", path.display()));
    }
}
