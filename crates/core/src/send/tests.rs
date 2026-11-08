use super::*;
use crate::{backup, offline::DiskFacts};
use std::time::Duration;

struct Fixture {
    _dir: tempfile::TempDir,
    offline: PathBuf,
    nas: PathBuf,
    shoot: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let offline = dir.path().join("offline");
        let nas = dir.path().join("nas");
        fs::create_dir(&offline).unwrap();
        fs::create_dir(&nas).unwrap();
        Self {
            _dir: dir,
            offline,
            nas,
            shoot: PathBuf::from("2026/2026-11-08_Kyoto"),
        }
    }
    fn write(&self, root: &Path, relative: &Path, bytes: &[u8]) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    fn local(&self, name: &str, bytes: &[u8]) {
        self.write(&self.offline, &self.shoot.join(name), bytes);
    }
    fn send(&self) -> SendResult {
        send(
            &self.offline,
            std::slice::from_ref(&self.shoot),
            &[],
            &self.nas,
            false,
            &AtomicBool::new(false),
            |_| {},
        )
    }
    fn drive(&self) -> Target {
        let root = self._dir.path().join("drive");
        fs::create_dir(&root).unwrap();
        let facts = DiskFacts {
            root: root.clone(),
            local: true,
            system: false,
            free: Some(u64::MAX),
            total: Some(u64::MAX),
            filesystem: Some("exFAT".into()),
            label: None,
        };
        let d = backup::check_init_with(&self.offline, &facts, Some(&self.offline), |_| Ok(1.0))
            .drive
            .unwrap();
        Target {
            root,
            uuid: d.uuid,
            label: d.label,
        }
    }
}

#[test]
fn merges_into_existing_shoot_checks_identical_and_keeps_different_sidecar() {
    let f = Fixture::new();
    f.local("a.ARW", b"photo");
    f.local("a.xmp", b"laptop edits");
    f.local("video/c.mp4", b"video");
    f.write(&f.nas, &f.shoot.join("a.ARW"), b"photo");
    f.write(&f.nas, &f.shoot.join("a.xmp"), b"NAS edits");
    f.write(&f.nas, &f.shoot.join("unrelated.jpg"), b"keep");
    let r = f.send();
    assert_eq!(r.status, ImportStatus::Complete, "{r:?}");
    assert_eq!(
        (r.counts.copied, r.counts.skipped, r.counts.verified),
        (1, 2, 3)
    );
    assert_eq!(r.sent.unwrap().kept_sidecars, vec![f.shoot.join("a.xmp")]);
    assert_eq!(
        fs::read(f.nas.join(&f.shoot).join("a.xmp")).unwrap(),
        b"NAS edits"
    );
    assert_eq!(
        r.manifest.files.len(),
        2,
        "different sidecar was not verified for deletion"
    );
    assert_eq!(f.send().counts.copied, 0);
}

#[test]
fn different_photo_stops_before_copying_without_replacing_anything() {
    let f = Fixture::new();
    f.local("a.ARW", b"one");
    f.local("b.ARW", b"two");
    f.write(&f.nas, &f.shoot.join("b.ARW"), b"bad");
    let r = f.send();
    assert_eq!(r.status, ImportStatus::Failed);
    assert!(r.error.unwrap().contains("different contents"));
    assert_eq!(
        fs::read(f.nas.join(&f.shoot).join("b.ARW")).unwrap(),
        b"bad"
    );
    assert!(!f.nas.join(&f.shoot).join("a.ARW").exists());
}

#[test]
fn files_in_other_date_folders_are_skipped_and_sidecars_follow_the_original() {
    let f = Fixture::new();
    f.local("a.ARW", b"one");
    f.local("a.xmp", b"edits");
    let other = Path::new("2026/2026-11-08_Old");
    f.write(&f.nas, &other.join("a.ARW"), b"one");
    let r = f.send();
    assert_eq!(r.status, ImportStatus::Complete, "{r:?}");
    assert_eq!(r.sent.unwrap().found_in, vec![other.to_path_buf()]);
    assert_eq!((r.counts.copied, r.counts.skipped), (1, 1));
    assert_eq!(fs::read(f.nas.join(other).join("a.xmp")).unwrap(), b"edits");
    assert!(!f.nas.join(&f.shoot).exists());
}

#[test]
fn nested_files_are_found_under_another_date_title_too() {
    let f = Fixture::new();
    f.local("edits/finished/a.jpg", b"finished");
    f.local("notes/trip.txt", b"notes");
    let other = Path::new("2026/2026-11-08_Old");
    f.write(&f.nas, &other.join("edits/finished/a.jpg"), b"finished");
    f.write(&f.nas, &other.join("notes/trip.txt"), b"notes");
    let result = f.send();
    assert_eq!(result.status, ImportStatus::Complete, "{result:?}");
    assert_eq!((result.counts.copied, result.counts.skipped), (0, 2));
    assert_eq!(result.sent.unwrap().found_in, vec![other.to_path_buf()]);
}

#[test]
fn a_raw_jpeg_pair_keeps_the_xmp_with_the_raw_when_the_raw_is_elsewhere() {
    let f = Fixture::new();
    f.local("a.NEF", b"raw");
    f.local("a.JPG", b"jpeg");
    f.local("a.xmp", b"raw edits");
    let other = Path::new("2026/2026-11-08_Old");
    f.write(&f.nas, &other.join("a.NEF"), b"raw");
    let result = f.send();
    assert_eq!(result.status, ImportStatus::Complete, "{result:?}");
    assert_eq!(
        fs::read(f.nas.join(other).join("a.xmp")).unwrap(),
        b"raw edits"
    );
    assert!(f.nas.join(&f.shoot).join("a.JPG").is_file());
    assert!(!f.nas.join(&f.shoot).join("a.xmp").exists());
}

#[test]
fn source_sidecar_changes_cannot_be_mistaken_for_a_different_nas_sidecar() {
    let f = Fixture::new();
    f.local("a.ARW", b"photo");
    f.local("a.xmp", b"edits");
    f.write(&f.nas, &f.shoot.join("a.xmp"), b"other");
    let result = send(
        &f.offline,
        std::slice::from_ref(&f.shoot),
        &[],
        &f.nas,
        false,
        &AtomicBool::new(false),
        |p| {
            if p.phase == "file_complete" && p.completed_files == 1 {
                fs::write(f.offline.join(&f.shoot).join("a.xmp"), b"new laptop edits").unwrap();
            }
        },
    );
    assert_eq!(result.status, ImportStatus::Failed);
    assert!(result.sent.is_none());
    assert_eq!(
        fs::read(f.nas.join(&f.shoot).join("a.xmp")).unwrap(),
        b"other"
    );
}

#[test]
fn missing_laptop_shoot_falls_back_to_connected_backup_and_reports_source() {
    let f = Fixture::new();
    f.local("a.jpg", b"photo");
    let d = f.drive();
    f.write(&d.folder(), &f.shoot.join("a.jpg"), b"photo");
    fs::remove_file(f.offline.join(&f.shoot).join("a.jpg")).unwrap();
    fs::remove_dir(f.offline.join(&f.shoot)).unwrap();
    let r = send(
        &f.offline,
        std::slice::from_ref(&f.shoot),
        std::slice::from_ref(&d),
        &f.nas,
        false,
        &AtomicBool::new(false),
        |_| {},
    );
    assert_eq!(r.status, ImportStatus::Complete, "{r:?}");
    assert!(r.sent.unwrap().sources[0].source.contains(&d.uuid));
    assert!(r.manifest.files.is_empty());
    assert_eq!(
        fs::read(d.folder().join(&f.shoot).join("a.jpg")).unwrap(),
        b"photo"
    );
}

#[test]
fn stop_and_resume_skip_completed_files_and_clean_staging() {
    let f = Fixture::new();
    for name in ["a.jpg", "b.jpg"] {
        f.local(name, b"data");
    }
    let cancel = AtomicBool::new(false);
    let r = send(
        &f.offline,
        std::slice::from_ref(&f.shoot),
        &[],
        &f.nas,
        false,
        &cancel,
        |p| {
            if p.phase == "file_complete" {
                cancel.store(true, Ordering::Relaxed);
            }
        },
    );
    assert_eq!((r.status, r.counts.verified), (ImportStatus::Cancelled, 1));
    assert!(r.sent.is_none());
    let r = f.send();
    assert_eq!(
        (r.status, r.counts.copied, r.counts.skipped),
        (ImportStatus::Complete, 1, 1)
    );
    assert!(
        !walkdir::WalkDir::new(&f.nas).into_iter().any(|e| e
            .unwrap()
            .path()
            .extension()
            .is_some_and(|e| e == "part"))
    );
}

#[test]
fn deletion_only_removes_verified_unchanged_files_leaves_edits_extras_and_all_drives() {
    let f = Fixture::new();
    f.local("a.ARW", b"raw");
    f.local("video/c.mp4", b"clip");
    f.local("b.jpg", b"photo");
    let d = f.drive();
    f.write(&d.folder(), &f.shoot.join("a.ARW"), b"raw");
    let r = f.send();
    assert_eq!(r.status, ImportStatus::Complete);
    let manifest_path = f._dir.path().join("app-data/manifest.json");
    r.manifest.save(&manifest_path).unwrap();
    let manifest = Manifest::load(&manifest_path).unwrap();
    // Same-size edit with a different time is kept, and extra files are never
    // granted deletion permission by a directory-wide scan.
    f.local("b.jpg", b"edits");
    fs::File::options()
        .write(true)
        .open(f.offline.join(&f.shoot).join("b.jpg"))
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
        .unwrap();
    f.local("extra.txt", b"new");
    let deleted = delete_laptop(&manifest).unwrap();
    assert_eq!(deleted.deleted, 2);
    assert!(!deleted.complete);
    assert_eq!(deleted.kept.len(), 2);
    assert!(!f.offline.join(&f.shoot).join("video").exists());
    assert!(f.offline.join("2026").is_dir());
    assert_eq!(
        fs::read(f.offline.join(&f.shoot).join("b.jpg")).unwrap(),
        b"edits"
    );
    assert_eq!(
        fs::read(d.folder().join(&f.shoot).join("a.ARW")).unwrap(),
        b"raw"
    );
    assert_eq!(
        fs::read(f.nas.join(&f.shoot).join("a.ARW")).unwrap(),
        b"raw"
    );
}

#[test]
fn deletion_removes_empty_shoot_but_keeps_year_and_refuses_manifest_escape() {
    let f = Fixture::new();
    f.local("a.jpg", b"photo");
    let r = f.send();
    let deleted = delete_laptop(&r.manifest).unwrap();
    assert!(deleted.complete);
    assert!(!f.offline.join(&f.shoot).exists());
    assert!(f.offline.join("2026").is_dir());
    let mut bad = r.manifest;
    bad.files[0].relative = PathBuf::from("../outside");
    assert!(delete_laptop(&bad).is_err());
}

#[test]
fn production_send_refuses_local_destination_and_source_paths_outside_shoots() {
    let f = Fixture::new();
    f.local("a.jpg", b"photo");
    assert_eq!(
        send(
            &f.offline,
            std::slice::from_ref(&f.shoot),
            &[],
            &f.nas,
            true,
            &AtomicBool::new(false),
            |_| {}
        )
        .status,
        ImportStatus::Failed
    );
    assert_eq!(
        send(
            &f.offline,
            &[PathBuf::from("../escape")],
            &[],
            &f.nas,
            false,
            &AtomicBool::new(false),
            |_| {}
        )
        .status,
        ImportStatus::Failed
    );
}

fn source_manifest(f: &Fixture) -> Manifest {
    Manifest {
        offline_root: f.offline.clone(),
        shoots: vec![f.shoot.clone()],
        files: copies::files(&f.offline, &f.shoot)
            .unwrap()
            .into_iter()
            .map(|f| VerifiedFile {
                relative: f.relative,
                size: f.size,
                modified: f.modified,
            })
            .collect(),
    }
}

#[test]
fn a_partly_deleted_laptop_shoot_cannot_be_marked_as_fully_sent() {
    let f = Fixture::new();
    f.local("a.ARW", b"one");
    f.local("b.ARW", b"two");
    let source = source_manifest(&f);
    let drive = f.drive();
    f.write(&drive.folder(), &f.shoot.join("a.ARW"), b"one");
    f.write(&drive.folder(), &f.shoot.join("b.ARW"), b"two");
    fs::remove_file(f.offline.join(&f.shoot).join("b.ARW")).unwrap();
    let r = send_import(
        &source,
        std::slice::from_ref(&drive),
        &f.nas,
        false,
        &AtomicBool::new(false),
        |_| {},
    );
    assert_eq!(r.status, ImportStatus::Failed);
    assert!(r.error.unwrap().contains("b.ARW is missing"));
    assert!(r.sent.is_none());
    assert!(!f.nas.join(&f.shoot).exists());
    // The specified source rule uses a backup when the whole laptop shoot is
    // absent. Its file list is checked too before publishing anything.
    fs::remove_file(f.offline.join(&f.shoot).join("a.ARW")).unwrap();
    fs::remove_dir(f.offline.join(&f.shoot)).unwrap();
    let r = send_import(
        &source,
        &[drive],
        &f.nas,
        false,
        &AtomicBool::new(false),
        |_| {},
    );
    assert_eq!(
        (r.status, r.counts.verified),
        (ImportStatus::Complete, 2),
        "{r:?}"
    );
}

#[test]
fn source_manifest_allows_later_edits_but_delete_uses_the_new_send_snapshot() {
    let f = Fixture::new();
    f.local("a.ARW", b"raw");
    f.local("a.xmp", b"initial");
    let source = source_manifest(&f);
    f.local("a.xmp", b"later edits");
    let r = send_import(&source, &[], &f.nas, false, &AtomicBool::new(false), |_| {});
    assert_eq!(r.status, ImportStatus::Complete, "{r:?}");
    assert_eq!(
        fs::read(f.nas.join(&f.shoot).join("a.xmp")).unwrap(),
        b"later edits"
    );
    assert_eq!(
        r.manifest
            .files
            .iter()
            .find(|f| f.relative.extension().is_some_and(|e| e == "xmp"))
            .unwrap()
            .size,
        11
    );
    assert!(delete_laptop(&r.manifest).unwrap().complete);
}
