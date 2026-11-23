//! The app's log file. The installed app has no console, so what it would print
//! there (a file it couldn't read, a start that failed, a panic) goes here instead:
//! `Card Intake.log` in the app's log folder. A log over 1 MB starts again, and the
//! one before it is kept as `Card Intake.old.log`.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const NAME: &str = "Card Intake.log";
const MAX_BYTES: u64 = 1_000_000;

struct Log {
    path: PathBuf,
    file: Mutex<File>,
}

static LOG: OnceLock<Log> = OnceLock::new();

/// Where Tauri keeps an app's logs, worked out before the app itself exists.
pub fn folder(identifier: &str) -> PathBuf {
    let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
    if cfg!(windows) {
        var("LOCALAPPDATA")
            .unwrap_or_else(std::env::temp_dir)
            .join(identifier)
            .join("logs")
    } else if cfg!(target_os = "macos") {
        var("HOME")
            .unwrap_or_else(std::env::temp_dir)
            .join("Library/Logs")
            .join(identifier)
    } else {
        var("HOME")
            .map(|home| home.join(".local/share"))
            .unwrap_or_else(std::env::temp_dir)
            .join(identifier)
            .join("logs")
    }
}

fn open_in(dir: &Path) -> io::Result<(PathBuf, File)> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(NAME);
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > MAX_BYTES) {
        // Another copy of the app may have it open: then this one adds to it.
        let _ = std::fs::rename(&path, path.with_extension("old.log"));
    }
    let file = File::options().create(true).append(true).open(&path)?;
    Ok((path, file))
}

/// Starts logging to the file in `dir`, panics included. Without the file, lines
/// still go to the console.
pub fn open(dir: &Path) {
    match open_in(dir) {
        Ok((path, file)) => {
            let _ = LOG.set(Log {
                path,
                file: Mutex::new(file),
            });
        }
        Err(e) => eprintln!("Couldn't open the log in {}: {e}", dir.display()),
    }
    let print = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        write(&format!("Panic: {panic}"));
        print(panic);
    }));
}

/// The log file, once it's open.
pub fn path() -> Option<&'static Path> {
    LOG.get().map(|log| log.path.as_path())
}

fn stamped(message: &str) -> String {
    format!(
        "{} {message}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    )
}

fn write(message: &str) {
    if let Some(log) = LOG.get()
        && let Ok(mut file) = log.file.lock()
    {
        let _ = file.write_all(stamped(message).as_bytes());
    }
}

/// Adds a line to the log, and to the console when there is one.
pub fn line(message: impl std::fmt::Display) {
    let message = message.to_string();
    eprintln!("{message}");
    write(&message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_added_to_the_log_already_there() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        let (path, mut file) = open_in(&logs).unwrap();
        file.write_all(stamped("first").as_bytes()).unwrap();
        drop(file);
        let (again, mut file) = open_in(&logs).unwrap();
        assert_eq!(again, path);
        file.write_all(stamped("second").as_bytes()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with(" first") && lines[1].ends_with(" second"));
        // "2026-11-22 14:05:09 first": the time comes first.
        assert_eq!(lines[0].len(), "2026-11-22 14:05:09 first".len());
    }

    #[test]
    fn a_log_over_the_limit_starts_again_and_keeps_the_one_before() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(NAME);
        std::fs::write(&path, vec![b'x'; MAX_BYTES as usize + 1]).unwrap();
        let (_, mut file) = open_in(dir.path()).unwrap();
        file.write_all(b"new\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        let old = dir.path().join("Card Intake.old.log");
        assert_eq!(std::fs::metadata(old).unwrap().len(), MAX_BYTES + 1);
    }

    #[test]
    fn the_log_folder_is_named_after_the_app() {
        let folder = folder("io.github.semmmer.cardintake");
        assert!(
            folder
                .components()
                .any(|part| part.as_os_str() == "io.github.semmmer.cardintake")
        );
    }
}
