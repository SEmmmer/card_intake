//! Import photos to an existing temporary folder without app settings or history.

use card_intake_core::{import, scan};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let root = PathBuf::from(
        args.next()
            .ok_or("Expected card, temporary destination, ExifTool, and optional shoot name")?,
    );
    let destination = std::fs::canonicalize(PathBuf::from(
        args.next().ok_or("Missing temporary destination")?,
    ))?;
    let tool = PathBuf::from(args.next().ok_or("Missing ExifTool path")?);
    let name = args
        .next()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Rating check".into());
    if !destination.starts_with(std::fs::canonicalize(std::env::temp_dir())?) {
        return Err("This example only writes inside the system temporary folder".into());
    }
    let dates: BTreeSet<_> = scan::scan_card(&root)?
        .iter()
        .filter(|f| f.kind == scan::MediaKind::Photo)
        .map(|f| f.captured.date())
        .collect();
    let request = import::ImportRequest {
        offline: false,
        backup_drives: Vec::new(),
        relay_to: None,
        root,
        nas_root: destination,
        expected_card_id: None,
        shoots: dates
            .into_iter()
            .map(|date| import::ShootSelection {
                date,
                name: name.clone(),
            })
            .collect(),
        include_videos: false,
        verify: true,
        workers: 0,
        camera: None,
    };
    let mut phase = "";
    let result = import::import_to_folder(&request, Some(&tool), &AtomicBool::new(false), |p| {
        if phase != p.phase && p.phase != "file_complete" {
            eprintln!("{} ({}/{})", p.phase, p.completed_files, p.total_files);
            phase = p.phase;
        }
    });
    println!("{}", serde_json::to_string_pretty(&result)?);
    if result.status != import::ImportStatus::Complete {
        return Err("Import did not complete".into());
    }
    Ok(())
}
