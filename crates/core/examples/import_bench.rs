//! Local-only import benchmark. Never accepts a card or destination outside temp.
use card_intake_core::{
    import::{self, ImportRequest, ShootSelection},
    scan,
};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

fn main() {
    let base = PathBuf::from(std::env::args_os().nth(1).expect("scratch directory"));
    let base = std::fs::canonicalize(base).unwrap();
    assert!(base.starts_with(std::fs::canonicalize(std::env::temp_dir()).unwrap()));
    let root = base.join("card");
    let suffix = std::env::args().nth(2).unwrap_or_default();
    assert!(
        suffix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    );
    let laptop = base.join(format!("laptop{suffix}"));
    let nas = base.join(format!("nas{suffix}"));
    for path in [&root, &laptop, &nas] {
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            assert!(
                !card_intake_core::copies::is_link(&meta),
                "Benchmark paths must not be links or junctions"
            );
        }
    }
    std::fs::create_dir_all(&laptop).unwrap();
    std::fs::create_dir_all(&nas).unwrap();
    assert!(
        std::fs::read_dir(&laptop).unwrap().next().is_none(),
        "Use a fresh benchmark suffix; the laptop destination must be empty"
    );
    assert!(
        std::fs::read_dir(&nas).unwrap().next().is_none(),
        "Use a fresh benchmark suffix; the NAS destination must be empty"
    );
    let files = scan::scan_card(&root).unwrap();
    let names: BTreeSet<_> = files
        .iter()
        .map(|f| f.path.file_name().unwrap().to_owned())
        .collect();
    let shoots = files
        .iter()
        .map(|f| f.captured.date())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|date| ShootSelection {
            date,
            name: "Benchmark".into(),
        })
        .collect();
    let request = ImportRequest {
        root,
        nas_root: laptop,
        relay_to: Some(nas.clone()),
        expected_card_id: None,
        shoots,
        include_videos: true,
        verify: true,
        workers: 4,
        camera: None,
        offline: true,
        backup_drives: vec![],
    };
    let tool = std::env::var_os("CARD_INTAKE_EXIFTOOL").map(PathBuf::from);
    let start = Instant::now();
    let running = AtomicBool::new(true);
    let mut last_verified = None;
    let mut card_done = None;
    let (result, published) = std::thread::scope(|scope| {
        let poll = scope.spawn(|| {
            let mut seen = BTreeSet::new();
            let mut last = Duration::ZERO;
            while running.load(std::sync::atomic::Ordering::Relaxed) {
                for entry in walkdir::WalkDir::new(&nas)
                    .into_iter()
                    .filter_map(Result::ok)
                {
                    if entry.file_type().is_file()
                        && names.contains(entry.file_name())
                        && seen.insert(entry.path().to_owned())
                    {
                        last = start.elapsed();
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            (seen.len(), last)
        });
        let result =
            import::import_to_folder(&request, tool.as_deref(), &AtomicBool::new(false), |p| {
                if p.phase == "file_complete" && p.completed_files == p.total_files {
                    last_verified = Some(start.elapsed());
                }
                if p.phase == "card_done" {
                    card_done = Some(start.elapsed());
                }
            });
        running.store(false, std::sync::atomic::Ordering::Relaxed);
        (result, poll.join().unwrap())
    });
    let end = start.elapsed();
    println!(
        "files={} bytes={} status={:?} last_card_verified={last_verified:?} card_done={card_done:?} finished={end:?} last_nas_photo={:?} tail={:?} published={}",
        files.len(),
        files.iter().map(|f| f.size).sum::<u64>(),
        result.status,
        published.1,
        end.saturating_sub(published.1),
        published.0
    );
    println!("{}", serde_json::to_string_pretty(&result).unwrap());
    assert_eq!(result.status, import::ImportStatus::Complete);
}
