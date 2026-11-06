//! Prints what Card Intake sees on a card. Reads only; never writes to the card.
//!
//!     cargo run -p card_intake_core --example scan_card -- /Volumes/Untitled

use card_intake_core::{
    exif,
    grouping::group_by_date,
    scan::{self, MediaKind, TimeSource},
};
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: scan_card <card mount path>"),
    );
    let started = Instant::now();
    let files = scan::scan_card(&root).expect("couldn't read the card");
    println!(
        "Scanned {} files in {} ms",
        files.len(),
        started.elapsed().as_millis()
    );

    let count = |src| files.iter().filter(|f| f.time_source == src).count();
    println!(
        "Capture times: {} from EXIF, {} from clip sidecars, {} from file dates",
        count(TimeSource::Exif),
        count(TimeSource::Sidecar),
        count(TimeSource::FileTime)
    );

    for g in group_by_date(&files) {
        println!(
            "{}  {}–{}  {} photos ({:.1} GB)  {} clips ({:.1} GB)",
            g.date,
            g.first.format("%H:%M"),
            g.last.format("%H:%M"),
            g.photos,
            g.photo_bytes as f64 / 1e9,
            g.videos,
            g.video_bytes as f64 / 1e9
        );
    }

    if let Some(newest) = files.iter().rev().find(|f| f.kind == MediaKind::Photo) {
        println!("Newest photo: {}", newest.path.display());
        match exif::read_basic(&newest.path) {
            Ok(e) => println!("  {e:?}"),
            Err(err) => println!("  EXIF error: {err}"),
        }
    }
    if let Some(clip) = files.iter().find(|f| f.kind == MediaKind::Video) {
        println!(
            "Serial from clip sidecar: {:?}",
            scan::sidecar_serial(&clip.path)
        );
    }
}
