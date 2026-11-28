//! The camera's FTP folder: where it's set, and what it has of the inserted card.

use super::AppState;
use card_intake_core::ftp;
use serde::Serialize;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

impl AppState {
    /// The FTP folder the camera sends photos to, if one is set.
    pub fn ftp_root(&self) -> Result<Option<PathBuf>, String> {
        Ok(self
            .settings
            .lock()
            .map_err(|e| e.to_string())?
            .ftp_root
            .as_deref()
            .map(PathBuf::from))
    }
}

#[derive(Debug, Serialize)]
pub struct FtpFolder {
    pub ok: bool,
    pub root: Option<PathBuf>,
    pub detail: String,
}

/// Opens the folder, so a mistyped path isn't saved.
fn check(path: &Path) -> Result<(), String> {
    match std::fs::read_dir(path) {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("Couldn't open {}: {e}", path.display())),
    }
}

/// Saves the FTP folder; an empty path clears it.
#[tauri::command]
pub async fn save_ftp_folder(path: String, app: AppHandle) -> Result<FtpFolder, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let path = path.trim();
        let root = (!path.is_empty()).then(|| PathBuf::from(path));
        if let Some(root) = &root
            && let Err(detail) = check(root)
        {
            return Ok(FtpFolder {
                ok: false,
                root: None,
                detail,
            });
        }
        let mut settings = state.settings.lock().map_err(|e| e.to_string())?;
        let mut updated = settings.clone();
        updated.ftp_root = root.as_ref().map(|r| r.display().to_string());
        updated
            .save(&state.settings_path)
            .map_err(|e| e.to_string())?;
        *settings = updated;
        Ok(FtpFolder {
            ok: true,
            detail: match &root {
                Some(root) => format!(
                    "Saved. Photos in {} are compared with each card you import to the NAS.",
                    root.display()
                ),
                None => "No FTP folder: every photo is copied from the card.".into(),
            },
            root,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Debug, Serialize)]
pub struct FtpPreview {
    /// The FTP folder, when one is set.
    pub root: Option<PathBuf>,
    pub preview: Option<ftp::Preview>,
    /// Why the FTP folder couldn't be compared with the card.
    pub error: Option<String>,
}

/// What the FTP folder has of the card in `root`, by capture date.
#[tauri::command]
pub async fn ftp_preview(root: String, app: AppHandle) -> Result<FtpPreview, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let Some(ftp_root) = state.ftp_root()? else {
            return Ok(FtpPreview {
                root: None,
                preview: None,
                error: None,
            });
        };
        let _busy = super::busy::start("FTP preview")?;
        let _reading = state.imports.reading_card()?;
        let (preview, error) = match ftp::preview(Path::new(&root), &ftp_root) {
            Ok(preview) => (Some(preview), None),
            Err(error) => (None, Some(error)),
        };
        Ok(FtpPreview {
            root: Some(ftp_root),
            preview,
            error,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}
