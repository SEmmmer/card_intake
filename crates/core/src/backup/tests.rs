use super::*;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

struct Fixture {
    _temp: tempfile::TempDir,
    offline: PathBuf,
    facts: DiskFacts,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let offline = temp.path().join("offline");
        let root = temp.path().join("drive");
        fs::create_dir_all(&offline).unwrap();
        fs::create_dir_all(&root).unwrap();
        Self {
            _temp: temp,
            offline,
            facts: DiskFacts {
                root,
                local: true,
                system: false,
                free: Some(u64::MAX),
                total: Some(u64::MAX),
                filesystem: Some("exFAT".into()),
                label: Some("Trip backup".into()),
            },
        }
    }
    fn init(&self) -> InitCheck {
        check_init_with(&self.offline, &self.facts, Some(&self.offline), |_| Ok(1.0))
    }
    fn target(&self) -> Target {
        let d = self.init().drive.unwrap();
        Target {
            uuid: d.uuid,
            label: d.label,
            root: self.facts.root.clone(),
        }
    }
    fn write(&self, name: &str, bytes: &[u8]) {
        let path = self.offline.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    fn files(&self) -> Vec<CopyFile> {
        copies::files(&self.offline, Path::new("")).unwrap()
    }
}

#[test]
fn init_creates_one_marker_and_adopts_it_without_changing_it() {
    let f = Fixture::new();
    let a = f.init();
    assert!(a.check.ok, "{a:?}");
    let folders = marker_folders(&f.facts.root).unwrap();
    assert_eq!(folders.len(), 1);
    let json = fs::read(folders[0].join("drive.json")).unwrap();
    let marker: Marker = serde_json::from_slice(&json).unwrap();
    assert_eq!(marker.format, 1);
    assert_eq!(marker.label, "Trip backup");
    assert_eq!(Uuid::parse_str(&marker.uuid).unwrap().get_version_num(), 4);
    let b = f.init();
    assert!(b.check.ok);
    assert_eq!(a.drive.unwrap().uuid, b.drive.unwrap().uuid);
    assert_eq!(fs::read(folders[0].join("drive.json")).unwrap(), json);
    assert!(
        b.check
            .items
            .iter()
            .any(|i| i.title == "Card Intake drive Trip backup")
    );
    assert_eq!(fs::read_dir(&f.facts.root).unwrap().count(), 1);
}

#[test]
fn init_runs_the_real_64_mb_round_trip_inside_temporary_folders() {
    let f = Fixture::new();
    let first = check_init_with(&f.offline, &f.facts, Some(&f.offline), nas::write_test);
    assert!(first.check.ok, "{first:?}");
    let second = check_init_with(&f.offline, &f.facts, Some(&f.offline), nas::write_test);
    assert!(second.check.ok, "{second:?}");
    assert_eq!(fs::read_dir(&f.facts.root).unwrap().count(), 1);
    let folder = marker_folders(&f.facts.root).unwrap().pop().unwrap();
    assert_eq!(
        fs::read_dir(folder).unwrap().count(),
        1,
        "only drive.json remains"
    );
}

#[test]
fn init_space_counts_only_missing_paths_or_sizes() {
    let mut f = Fixture::new();
    let target = f.target();
    f.write("2026/shoot/a.jpg", b"same size");
    f.write("2026/shoot/b.jpg", b"new");
    fs::create_dir_all(target.folder().join("2026/shoot")).unwrap();
    fs::write(target.folder().join("2026/shoot/a.jpg"), b"same size").unwrap();
    f.facts.free = Some(3);
    let check = f.init();
    assert!(check.check.ok, "{check:?}");
    assert!(
        check
            .check
            .items
            .iter()
            .any(|i| i.detail.contains("3 bytes needed"))
    );
    fs::write(target.folder().join("2026/shoot/a.jpg"), b"different size").unwrap();
    assert!(!f.init().check.ok);
}

#[test]
fn init_fails_with_two_markers_or_the_offline_volume_and_never_tests_the_drive() {
    let f = Fixture::new();
    f.target();
    fs::create_dir(
        f.facts
            .root
            .join(format!(".card-intake-{}", Uuid::new_v4())),
    )
    .unwrap();
    let check = check_init_with(&f.offline, &f.facts, Some(&f.offline), |_| {
        panic!("must not write")
    });
    assert!(!check.check.ok);
    assert!(
        check
            .check
            .items
            .iter()
            .any(|i| i.detail.contains("Two or more"))
    );
    let f = Fixture::new();
    let check = check_init_with(&f.offline, &f.facts, Some(&f.facts.root), |_| {
        panic!("must not write")
    });
    assert!(!check.check.ok);
    assert!(marker_folders(&f.facts.root).unwrap().is_empty());
}

#[test]
fn init_space_is_an_error_and_fat32_is_a_warning() {
    let mut f = Fixture::new();
    f.write("2026/shoot/a.jpg", b"abc");
    f.facts.free = Some(2);
    let c = f.init();
    assert!(!c.check.ok);
    assert!(
        c.check
            .items
            .iter()
            .any(|i| i.status == Status::Fail && i.detail.contains("3 bytes needed, 2 available"))
    );
    assert!(marker_folders(&f.facts.root).unwrap().is_empty());
    f.facts.free = Some(100);
    f.facts.filesystem = Some("FAT32".into());
    let c = f.init();
    assert!(c.check.ok);
    assert!(
        c.check
            .items
            .iter()
            .any(|i| i.status == Status::Warn && i.detail.contains("Files over 4 GB"))
    );
}

#[test]
fn init_rejects_network_system_camera_and_failed_round_trip() {
    let mut f = Fixture::new();
    f.facts.local = false;
    assert!(!f.init().check.ok);
    f.facts.local = true;
    f.facts.system = true;
    assert!(!f.init().check.ok);
    f.facts.system = false;
    fs::create_dir(f.facts.root.join("DCIM")).unwrap();
    assert!(!f.init().check.ok);
    fs::remove_dir(f.facts.root.join("DCIM")).unwrap();
    let c = check_init_with(&f.offline, &f.facts, Some(&f.offline), |_| {
        Err("Test read-back differs".into())
    });
    assert!(!c.check.ok);
    assert!(marker_folders(&f.facts.root).unwrap().is_empty());
}

#[test]
fn backup_copies_missing_files_preserves_times_and_skips_junk_and_empty_folders() {
    let f = Fixture::new();
    let target = f.target();
    f.write("2026/2026-11-08_Kyoto/a.ARW", b"raw");
    f.write("2026/2026-11-08_Kyoto/video/c.mp4", b"clip");
    for junk in [
        "Thumbs.db",
        "desktop.ini",
        ".DS_Store",
        ".card-intake-junk/x",
        "unfinished.part",
    ] {
        f.write(junk, b"junk");
    }
    fs::create_dir(f.offline.join("empty")).unwrap();
    let files = f.files();
    assert_eq!(files.len(), 2);
    let c = back_up(&files, &target, &AtomicBool::new(false), |_| {});
    assert_eq!(c.status, ImportStatus::Complete, "{c:?}");
    assert_eq!(c.counts.copied, 2);
    assert!(!target.folder().join("empty").exists());
    for file in &files {
        assert!(copies::same_time(
            file.modified,
            fs::metadata(target.folder().join(&file.relative))
                .unwrap()
                .modified()
                .unwrap()
        ));
    }
    let c = back_up(&files, &target, &AtomicBool::new(false), |_| {});
    assert_eq!((c.counts.copied, c.counts.skipped), (0, 2));
}

#[test]
fn size_and_time_skip_without_reading_but_different_time_checks_hash() {
    let f = Fixture::new();
    let target = f.target();
    f.write("a.jpg", b"one");
    let files = f.files();
    assert_eq!(
        back_up(&files, &target, &AtomicBool::new(false), |_| {}).status,
        ImportStatus::Complete
    );
    // Same size and time deliberately trust the drive without opening either file.
    fs::write(target.folder().join("a.jpg"), b"two").unwrap();
    fs::File::options()
        .write(true)
        .open(target.folder().join("a.jpg"))
        .unwrap()
        .set_modified(files[0].modified)
        .unwrap();
    assert!(copies::existing(&files[0], &target.folder(), true, &AtomicBool::new(false)).unwrap());
    fs::File::options()
        .write(true)
        .open(target.folder().join("a.jpg"))
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(10))
        .unwrap();
    let c = back_up(&files, &target, &AtomicBool::new(false), |_| {});
    assert_eq!(c.status, ImportStatus::Failed);
    assert_eq!(fs::read(target.folder().join("a.jpg")).unwrap(), b"two");
    fs::write(target.folder().join("a.jpg"), b"one").unwrap();
    fs::File::options()
        .write(true)
        .open(target.folder().join("a.jpg"))
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH)
        .unwrap();
    let c = back_up(&files, &target, &AtomicBool::new(false), |_| {});
    assert_eq!((c.status, c.counts.skipped), (ImportStatus::Complete, 1));
}

#[test]
fn expected_card_hash_checks_existing_drive_copies_and_local_sources() {
    let f = Fixture::new();
    let target = f.target();
    f.write("a.ARW", b"raw");
    let mut files = f.files();
    files[0].hash = Some(123);
    let c = back_up(&files, &target, &AtomicBool::new(false), |_| {});
    assert_eq!(c.status, ImportStatus::Failed);
    assert!(!target.folder().join("a.ARW").exists());
    files[0].hash =
        Some(copies::hash(&files[0].source, files[0].size, &AtomicBool::new(false)).unwrap());
    assert_eq!(
        back_up(&files, &target, &AtomicBool::new(false), |_| {}).status,
        ImportStatus::Complete
    );
    fs::write(target.folder().join("a.ARW"), b"bad").unwrap();
    fs::File::options()
        .write(true)
        .open(target.folder().join("a.ARW"))
        .unwrap()
        .set_modified(files[0].modified)
        .unwrap();
    assert_eq!(
        back_up(&files, &target, &AtomicBool::new(false), |_| {}).status,
        ImportStatus::Failed
    );
}

#[test]
fn fat32_large_file_and_space_fail_before_any_copy() {
    let f = Fixture::new();
    let target = f.target();
    f.write("a.jpg", b"small");
    f.write("z.mp4", b"");
    fs::File::options()
        .write(true)
        .open(f.offline.join("z.mp4"))
        .unwrap()
        .set_len(copies::FAT32_MAX + 1)
        .unwrap();
    let files = f.files();
    let error = preflight_with(
        &files,
        &target.folder(),
        Some(u64::MAX),
        Some("FAT32"),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(error.contains("z.mp4"));
    assert!(!target.folder().join("a.jpg").exists());
    assert!(
        preflight_with(
            &files[..1],
            &target.folder(),
            Some(0),
            Some("exFAT"),
            &AtomicBool::new(false)
        )
        .unwrap_err()
        .contains("Not enough space")
    );
}

#[test]
fn stop_resume_and_marker_change_keep_completed_files() {
    let f = Fixture::new();
    let target = f.target();
    f.write("a.jpg", b"a");
    f.write("b.jpg", b"b");
    let files = f.files();
    let cancel = AtomicBool::new(false);
    let c = back_up(&files, &target, &cancel, |p| {
        if p.phase == "file_complete" {
            cancel.store(true, Ordering::Relaxed);
        }
    });
    assert_eq!((c.status, c.counts.verified), (ImportStatus::Cancelled, 1));
    let c = back_up(&files, &target, &AtomicBool::new(false), |_| {});
    assert_eq!((c.counts.copied, c.counts.skipped), (1, 1));
    fs::rename(target.folder(), target.root.join("disconnected")).unwrap();
    let c = back_up(&files, &target, &AtomicBool::new(false), |_| {});
    assert_eq!(c.status, ImportStatus::Failed);
}

#[test]
fn remembered_drive_is_found_after_its_mount_path_changes_and_duplicates_are_refused() {
    let f = Fixture::new();
    let d = f.init().drive.unwrap();
    let moved = f._temp.path().join("new-letter");
    fs::rename(&f.facts.root, &moved).unwrap();
    let views = connected(std::slice::from_ref(&d), std::slice::from_ref(&moved));
    assert_eq!(views[0].root, Some(moved.clone()));
    assert!(connected(std::slice::from_ref(&d), &[])[0].root.is_none());
    assert!(connected(&[d], &[moved.clone(), moved])[0].root.is_none());
}
