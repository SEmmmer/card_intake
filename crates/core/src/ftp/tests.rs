use super::*;
use crate::exif::tests::{build_tiff, jpeg};

const DAY: &str = "2026:11:27";
const SHOOT: &str = "2026/2026-11-27_fisheye";

fn date() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, 27).unwrap()
}

/// A raw (or with `.JPG`, a JPEG) taken at `time` by the camera with `serial`, `extra`
/// bytes making each photo its own.
fn photo(name: &str, day: &str, time: &str, serial: &str, extra: u8) -> Vec<u8> {
    let tiff = build_tiff(
        true,
        &[(0x0110, "ILCE-7RM6")],
        &[(0x9003, &format!("{day} {time}")), (0xA431, serial)],
    );
    let mut bytes = if name.to_ascii_lowercase().ends_with(".jpg") {
        jpeg(&tiff)
    } else {
        tiff
    };
    bytes.extend(std::iter::repeat_n(extra, 64));
    bytes
}

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }
    fn card(&self) -> PathBuf {
        self.dir.path().join("card")
    }
    fn ftp(&self) -> PathBuf {
        self.dir.path().join("FTP")
    }
    fn nas(&self) -> PathBuf {
        self.dir.path().join("photos")
    }
    fn write(&self, path: PathBuf, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    fn on_card(&self, name: &str, bytes: &[u8]) {
        self.write(self.card().join("DCIM/100MSDCF").join(name), bytes);
    }
    fn on_ftp(&self, path: &str, bytes: &[u8]) {
        self.write(self.ftp().join(path), bytes);
    }
    fn options(&self, merge: bool) -> Options {
        Options {
            root: self.ftp(),
            skip: true,
            merge,
        }
    }
    fn plan(&self, merge: bool) -> Plan {
        plan(
            &self.card(),
            &self.options(merge),
            &BTreeSet::from([date()]),
            true,
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap()
    }
    fn merge(&self, plan: &Plan) -> Merged {
        merge(
            &self.nas(),
            &self.ftp(),
            &BTreeMap::from([(date(), PathBuf::from(SHOOT))]),
            &plan.moves,
            None,
            &AtomicBool::new(false),
            |_, _, _| {},
        )
    }
    fn shoot(&self, name: &str) -> PathBuf {
        self.nas().join(SHOOT).join(name)
    }
}

/// The card has EM_04445 (raw and JPEG), which the camera also sent, and EM_04447,
/// which it didn't. The FTP folder also has EM_04446 from the same camera, a second
/// copy of EM_04445.ARW, another camera's photo, another day's, and its recycle bin.
fn shoot() -> Fixture {
    let f = Fixture::new();
    let raw = photo("EM_04445.ARW", DAY, "13:51:20", "8016296", 1);
    let jpg = photo("EM_04445.JPG", DAY, "13:51:20", "8016296", 2);
    f.on_card("EM_04445.ARW", &raw);
    f.on_card("EM_04445.JPG", &jpg);
    f.on_card(
        "EM_04447.ARW",
        &photo("EM_04447.ARW", DAY, "14:02:00", "8016296", 3),
    );
    f.on_ftp("A/DCIM/10261127/EM_04445.ARW", &raw);
    f.on_ftp("A/DCIM/10261127/EM_04445.JPG", &jpg);
    f.on_ftp("A/EM_04445_1.ARW", &raw);
    f.on_ftp(
        "A/DCIM/10261127/EM_04446.ARW",
        &photo("EM_04446.ARW", DAY, "13:55:00", "8016296", 4),
    );
    f.on_ftp(
        "B/DCIM/10261127/EM_09999.ARW",
        &photo("EM_09999.ARW", DAY, "13:56:00", "OTHER", 5),
    );
    f.on_ftp(
        "A/DCIM/10161126/EM_04400.ARW",
        &photo("EM_04400.ARW", "2026:11:26", "10:00:00", "8016296", 6),
    );
    f.on_ftp(
        "#recycle/EM_04446.ARW",
        &photo("EM_04446.ARW", DAY, "13:55:00", "8016296", 4),
    );
    f
}

#[test]
fn an_ftp_servers_numbered_copy_keeps_the_cameras_name() {
    assert_eq!(original_name("EM_04408_1.JPG"), "EM_04408.JPG");
    assert_eq!(original_name("DSC00001_12.ARW"), "DSC00001.ARW");
    // The camera's own numbers are never taken for the server's.
    assert_eq!(original_name("EM_04408.JPG"), "EM_04408.JPG");
    assert_eq!(original_name("DSC_0001.JPG"), "DSC_0001.JPG");
    assert_eq!(original_name("IMG_1_2.JPG"), "IMG_1_2.JPG");
    assert_eq!(original_name("EM_04408_1234.JPG"), "EM_04408_1234.JPG");
}

#[test]
fn the_preview_counts_the_cards_photos_in_ftp_and_the_cameras_other_ftp_photos() {
    let f = shoot();
    let found = preview(&f.card(), &f.ftp()).unwrap();
    assert_eq!(found.photos, 6);
    assert_eq!(found.dates.len(), 1);
    let day = &found.dates[0];
    assert_eq!((day.date, day.on_card, day.ftp_only), (date(), 2, 1));
    assert!(day.on_card_bytes > 0 && day.ftp_only_bytes > 0);
    assert!(preview(&f.card(), &f.dir.path().join("missing")).is_err());
}

#[test]
fn skipping_leaves_the_photos_ftp_has_on_the_card_and_moves_nothing() {
    let f = shoot();
    let only = f.plan(false);
    let mut skip = only.skip.clone();
    skip.sort();
    assert_eq!(
        skip,
        [
            Path::new("DCIM/100MSDCF/EM_04445.ARW"),
            Path::new("DCIM/100MSDCF/EM_04445.JPG")
        ]
    );
    assert!(only.moves.is_empty());
    assert_eq!(only.left_on_card, 1);
    assert!(
        plan(
            &f.card(),
            &Options {
                skip: false,
                ..f.options(true)
            },
            &BTreeSet::from([date()]),
            true,
            &AtomicBool::new(false),
            |_, _| {},
        )
        .is_err()
    );
}

#[test]
fn a_merge_moves_the_cameras_ftp_photos_into_the_shoot_and_empties_the_ftp_folder() {
    let f = shoot();
    let plan = f.plan(true);
    assert_eq!(plan.skip.len(), 2);
    let names: Vec<_> = plan.moves.iter().map(|m| m.name.as_str()).collect();
    // Oldest first; another camera's and another day's photos stay.
    assert_eq!(names, ["EM_04445.ARW", "EM_04445.JPG", "EM_04446.ARW"]);
    assert_eq!(plan.moves[0].sources.len(), 2);
    assert!(plan.moves[0].hash.is_some() && plan.moves[2].hash.is_none());

    let merged = f.merge(&plan);
    assert_eq!(
        merged.summary,
        Summary {
            root: f.ftp(),
            skipped: 0,
            moved: 3,
            already_there: 0,
            duplicates_removed: 1,
            left: 0,
        }
    );
    assert_eq!(merged.shoots, BTreeSet::from([PathBuf::from(SHOOT)]));
    assert_eq!(
        fs::read(f.shoot("EM_04445.ARW")).unwrap(),
        fs::read(f.card().join("DCIM/100MSDCF/EM_04445.ARW")).unwrap()
    );
    assert!(f.shoot("EM_04445.JPG").is_file() && f.shoot("EM_04446.ARW").is_file());
    // Without ExifTool the raws aren't rated, and the merge says so.
    assert!(
        merged.warnings.iter().any(|w| w.contains("ExifTool")),
        "{:?}",
        merged.warnings
    );
    assert!(!f.ftp().join("A/EM_04445_1.ARW").exists());
    // Emptied folders go; folders still holding photos stay.
    assert!(!f.ftp().join("A/DCIM/10261127").exists());
    assert!(f.ftp().join("A/DCIM/10161126/EM_04400.ARW").exists());
    assert!(f.ftp().join("B/DCIM/10261127/EM_09999.ARW").exists());
    assert!(f.ftp().join("#recycle/EM_04446.ARW").exists());
    let leftovers: Vec<_> = WalkDir::new(f.nas())
        .into_iter()
        .map(Result::unwrap)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn a_photo_the_shoot_has_is_only_deleted_from_ftp_and_a_different_one_stays() {
    let f = shoot();
    let plan = f.plan(true);
    let same = fs::read(f.ftp().join("A/DCIM/10261127/EM_04446.ARW")).unwrap();
    f.write(f.shoot("EM_04446.ARW"), &same);
    // A photo of another day under the shoot's name for EM_04445.JPG.
    let other = photo("EM_04445.JPG", DAY, "09:00:00", "8016296", 9);
    f.write(f.shoot("EM_04445.JPG"), &other);
    let merged = f.merge(&plan);
    assert_eq!(
        (
            merged.summary.moved,
            merged.summary.already_there,
            merged.summary.left
        ),
        (1, 1, 1)
    );
    assert!(!f.ftp().join("A/DCIM/10261127/EM_04446.ARW").exists());
    assert!(f.ftp().join("A/DCIM/10261127/EM_04445.JPG").exists());
    assert_eq!(fs::read(f.shoot("EM_04445.JPG")).unwrap(), other);
    assert!(
        merged
            .warnings
            .iter()
            .any(|w| w.contains("EM_04445.JPG") && w.contains("different contents"))
    );
}

#[test]
fn an_ftp_photo_with_the_cards_name_and_size_but_other_bytes_isnt_the_cards() {
    let f = shoot();
    let mut changed = fs::read(f.ftp().join("A/DCIM/10261127/EM_04445.JPG")).unwrap();
    let last = changed.len() - 1;
    changed[last] ^= 1;
    f.on_ftp("A/DCIM/10261127/EM_04445.JPG", &changed);
    // Only a merge compares contents.
    assert_eq!(f.plan(false).skip.len(), 2);
    let plan = f.plan(true);
    assert_eq!(plan.skip, [Path::new("DCIM/100MSDCF/EM_04445.ARW")]);
    assert_eq!(plan.left_on_card, 2);
    assert!(plan.moves.iter().all(|m| m.name != "EM_04445.JPG"));
    assert!(plan.warnings.iter().any(|w| w.contains("EM_04445.JPG")));
}

#[test]
fn a_card_whose_photos_are_all_in_ftp_has_nothing_left_to_copy() {
    let f = shoot();
    fs::remove_file(f.card().join("DCIM/100MSDCF/EM_04447.ARW")).unwrap();
    assert_eq!(f.plan(false).left_on_card, 0);
}
