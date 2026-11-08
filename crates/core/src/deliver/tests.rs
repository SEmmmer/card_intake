use super::*;
use serde_json::json;

fn tool() -> Option<PathBuf> {
    std::env::var_os("CARD_INTAKE_EXIFTOOL")
        .map(PathBuf::from)
        .or_else(|| {
            crate::process::command(Path::new("exiftool"))
                .arg("-ver")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|_| PathBuf::from("exiftool"))
        })
}

fn jpeg(path: &Path, tags: &[&str], tool: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // A generated 2x2 blue JPEG, with no camera or application metadata.
    fs::write(path, include_bytes!("tiny.jpg")).unwrap();
    let mut args = tempfile::NamedTempFile::new().unwrap();
    writeln!(args, "-overwrite_original").unwrap();
    for tag in tags {
        writeln!(args, "{tag}").unwrap();
    }
    writeln!(args, "{}", path.display()).unwrap();
    args.flush().unwrap();
    let out = crate::process::command(tool)
        .args(["-charset", "filename=utf8", "-@"])
        .arg(args.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn tiny_catalog(path: &Path, shoot: &str, wal: bool) -> Connection {
    let db = Connection::open(path).unwrap();
    if wal {
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
    }
    db.execute_batch("CREATE TABLE Adobe_images(id_local INTEGER,rootFile INTEGER,rating REAL,pick REAL,masterImage INTEGER,colorLabels TEXT);
        CREATE TABLE AgLibraryFile(id_local INTEGER,baseName TEXT,extension TEXT,folder INTEGER);
        CREATE TABLE AgLibraryFolder(id_local INTEGER,pathFromRoot TEXT,rootFolder INTEGER);
        CREATE TABLE AgLibraryRootFolder(id_local INTEGER,absolutePath TEXT);").unwrap();
    db.execute(
        "INSERT INTO AgLibraryRootFolder VALUES(1,?1)",
        [format!("{shoot}/")],
    )
    .unwrap();
    db.execute_batch("INSERT INTO AgLibraryFolder VALUES(1,'',1);
        INSERT INTO AgLibraryFile VALUES(1,'EM_01624','ARW',1),(2,'EM_01415-DxO_DeepPRIME XD3','dng',1),(3,'EM_01415','ARW',1),(4,'NULL','NEF',1);
        INSERT INTO Adobe_images VALUES(1,1,5,0,NULL,''),(2,2,4,0,NULL,''),(3,3,3,0,NULL,''),(4,4,NULL,0,NULL,'');").unwrap();
    db
}

#[test]
fn classification_distinguishes_absent_and_empty_and_accepts_future_pixcake() {
    assert_eq!(
        classify(&json!({"XMP-xmp:CreatorTool":""})),
        Ok(Kind::PixCake)
    );
    assert_eq!(
        classify(&json!({"XMP-xmp:CreatorTool":"    ","IFD0:Software":""})),
        Ok(Kind::PixCake)
    );
    assert_eq!(classify(&json!({})), Err("unrecognised"));
    assert_eq!(
        classify(
            &json!({"IFD0:Software":"ILCE-7RM6 v1.01","IFD0:Make":"SONY","IFD0:Model":"ILCE-7RM6"})
        ),
        Err("camera JPEG")
    );
    for tag in ["IFD0:Software", "XMP-xmp:CreatorTool"] {
        for text in ["Pixcake 9", "像素蛋糕"] {
            assert_eq!(classify(&json!({tag:text})), Ok(Kind::PixCake));
        }
        assert_eq!(
            classify(&json!({tag:"Adobe Photoshop Lightroom Classic 15.6 (Windows)"})),
            Ok(Kind::Lightroom)
        );
    }
    assert_eq!(pair_stem("EM_01226_(2)"), "EM_01226");
    assert_eq!(pair_stem("EM_01226_(x)"), "EM_01226_(x)");
    assert_eq!(clean_stem("EM_01228-DxO_DeepPRIME XD3"), "EM_01228");
}

#[test]
fn unc_paths_and_catalog_master_ratings_include_wal_without_opening_the_original() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source.lrcat");
    let db = tiny_catalog(
        &path,
        "//nas/photos/Sony Transfer/2026/2026-11-07_test",
        true,
    );
    db.execute("UPDATE Adobe_images SET rating=2 WHERE id_local=1", [])
        .unwrap();
    let before = hash_file(&path).unwrap();
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    let before_wal = hash_file(&wal).unwrap();
    let rows = catalog_ratings(
        &path,
        Path::new(r"\\NAS\photos\Sony Transfer\2026\2026-11-07_test\"),
    )
    .unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].rating, 2);
    assert_eq!(rows[3].rating, 0);
    assert_eq!(hash_file(&path).unwrap(), before);
    assert_eq!(hash_file(&wal).unwrap(), before_wal);
    assert_eq!(normal_path("//nas/a/"), normal_path(r"\\NAS\a"));
}

/// A shoot folder and the finished photos folder next to it, as on the NAS.
struct Place {
    _temp: tempfile::TempDir,
    shoot: PathBuf,
    target: PathBuf,
}

fn place_for(name: &str) -> Place {
    let temp = tempfile::tempdir().unwrap();
    let shoot = temp.path().join("Sony Transfer/2026").join(name);
    fs::create_dir_all(&shoot).unwrap();
    let photos = temp.path().join("MyPhoto");
    fs::create_dir_all(&photos).unwrap();
    Place {
        target: photos.join(name),
        shoot,
        _temp: temp,
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut list: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    list.sort();
    list
}

fn run(p: &Place, tool: &Path, catalog: Option<&Path>) -> DeliveryResult {
    let plan = plan(&p.shoot, tool, catalog, &p.target).unwrap();
    deliver(&p.shoot, tool, catalog, &p.target, &plan.confirmed()).unwrap()
}

#[test]
fn night_layout_moves_seven_pixcake_jpegs_into_a_new_folder_and_a_repeat_adds_nothing() {
    let Some(tool) = tool() else {
        eprintln!("Set CARD_INTAKE_EXIFTOOL to run JPEG integration tests");
        return;
    };
    let p = place_for("2026-11-06_夜景");
    let shoot = &p.shoot;
    for stem in ["01226", "01249", "01257", "01263", "01360", "01388"] {
        jpeg(
            &shoot.join(format!("topix/EM_{stem}.jpg")),
            &[
                "-Software=Adobe Lightroom",
                &format!("-XMP-xmpMM:DocumentID=xmp.did:{stem}"),
            ],
            &tool,
        );
        jpeg(
            &shoot.join(format!("final/EM_{stem}_(2).jpg")),
            &[
                "-XMP-xmp:CreatorTool=PixCake",
                &format!("-XMP-xmpMM:DocumentID=xmp.did:{stem}"),
            ],
            &tool,
        );
    }
    jpeg(
        &shoot.join("final/EM_01228-DxO_DeepPRIME XD3.jpg"),
        &["-XMP-xmp:CreatorTool=PixCake"],
        &tool,
    );
    jpeg(
        &shoot.join("DSC00001.JPG"),
        &[
            "-Software=ILCE-7RM6 v1.01",
            "-Make=SONY",
            "-Model=ILCE-7RM6",
        ],
        &tool,
    );
    jpeg(&shoot.join("unknown.JpEg"), &[], &tool);
    for dir in [
        "VIDEO",
        "_catalog",
        "_deliver",
        "@eaDir",
        "#recycle",
        "previews.LRDATA",
        ".hidden",
        STAGING,
    ] {
        jpeg(
            &shoot.join(dir).join("ignored.jpg"),
            &["-Software=PixCake"],
            &tool,
        );
    }
    fs::write(shoot.join("raw.ARW"), b"raw").unwrap();

    let planned = plan(shoot, &tool, None, &p.target).unwrap();
    assert_eq!((planned.new, planned.existing), (7, 0));
    assert!(!planned.target_exists);
    assert!(planned.photos.iter().all(|p| p.kind == Kind::PixCake));
    let reasons =
        |r: &DeliveryResult, reason: &str| r.skipped.iter().filter(|s| s.reason == reason).count();
    assert_eq!(reasons(&planned, "replaced by PixCake"), 6);
    assert_eq!(reasons(&planned, "camera JPEG"), 1);
    assert_eq!(reasons(&planned, "unrecognised"), 1);
    assert!(!p.target.exists(), "planning writes nothing");

    // The test JPEGs carry no EXIF thumbnail; a missing one is simply left out.
    let mut shown = plan(shoot, &tool, None, &p.target).unwrap();
    add_thumbnails(&tool, &mut shown);
    assert!(shown.photos.iter().all(|p| p.thumbnail.is_none()));

    let r = deliver(shoot, &tool, None, &p.target, &planned.confirmed()).unwrap();
    assert_eq!(r.delivered, 7, "{:?}", r.errors);
    assert!(r.errors.is_empty());
    // Only the finished JPEGs land in MyPhoto: no staging, manifest or temporary files.
    assert_eq!(
        names(&p.target),
        [
            "EM_01226.jpg",
            "EM_01228.jpg",
            "EM_01249.jpg",
            "EM_01257.jpg",
            "EM_01263.jpg",
            "EM_01360.jpg",
            "EM_01388.jpg"
        ]
    );
    assert_eq!(names(p.target.parent().unwrap()), ["2026-11-06_夜景"]);
    assert!(!shoot.join(STAGING).exists());
    assert!(shoot.join(MANIFEST).is_file());
    for photo in &r.photos {
        assert_eq!(
            hash_file(&shoot.join(&photo.source)).unwrap(),
            hash_file(&p.target.join(&photo.name)).unwrap()
        );
    }
    assert_eq!(state(shoot).unwrap().count, 7);

    // Pressing Deliver again: nothing new, nothing touched.
    let again = plan(shoot, &tool, None, &p.target).unwrap();
    assert_eq!((again.new, again.existing), (0, 7));
    assert!(again.confirmed().is_empty());
    let before = hash_file(&p.target.join("EM_01226.jpg")).unwrap();
    let r = deliver(shoot, &tool, None, &p.target, &[]).unwrap();
    assert_eq!(r.delivered, 0);
    assert_eq!(hash_file(&p.target.join("EM_01226.jpg")).unwrap(), before);

    // A photo deleted from MyPhoto is offered again, under its old name.
    fs::remove_file(p.target.join("EM_01228.jpg")).unwrap();
    let back = plan(shoot, &tool, None, &p.target).unwrap();
    assert_eq!(
        back.confirmed(),
        [Confirmed {
            name: "EM_01228.jpg".into(),
            source: Path::new("final")
                .join("EM_01228-DxO_DeepPRIME XD3.jpg")
                .to_string_lossy()
                .into_owned()
        }]
    );
    assert_eq!(run(&p, &tool, None).delivered, 1);
    assert!(p.target.join("EM_01228.jpg").is_file());
}

#[test]
fn an_existing_folder_gets_only_new_names_and_nothing_there_is_replaced() {
    let Some(tool) = tool() else {
        return;
    };
    // MyPhoto\2026-10-30 already holds JPEGs delivered by hand.
    let p = place_for("2026-10-30");
    fs::create_dir_all(&p.target).unwrap();
    fs::write(p.target.join("DSC00001.jpg"), b"delivered by hand").unwrap();
    fs::write(p.target.join("other.jpg"), b"keep").unwrap();
    jpeg(
        &p.shoot.join("final/DSC00001_(2).jpg"),
        &["-Software=PixCake"],
        &tool,
    );
    jpeg(
        &p.shoot.join("topix/DSC00003.jpg"),
        &["-Software=Adobe Lightroom"],
        &tool,
    );
    let planned = plan(&p.shoot, &tool, None, &p.target).unwrap();
    assert!(planned.target_exists);
    assert_eq!((planned.new, planned.existing), (1, 1));
    let exists = planned
        .photos
        .iter()
        .find(|p| p.action == "exists")
        .unwrap();
    assert_eq!(exists.name, "DSC00001.jpg");
    let r = deliver(&p.shoot, &tool, None, &p.target, &planned.confirmed()).unwrap();
    assert_eq!((r.delivered, r.existing), (1, 1));
    assert_eq!(
        fs::read(p.target.join("DSC00001.jpg")).unwrap(),
        b"delivered by hand"
    );
    assert_eq!(fs::read(p.target.join("other.jpg")).unwrap(), b"keep");
    assert_eq!(
        names(&p.target),
        ["DSC00001.jpg", "DSC00003.jpg", "other.jpg"]
    );
    assert!(!p.shoot.join(STAGING).exists());

    // A PixCake redo of a photo already delivered from Lightroom keeps its name and is
    // skipped: delivered files are never overwritten.
    jpeg(
        &p.shoot.join("final/DSC00003_(2).jpg"),
        &["-Software=PixCake"],
        &tool,
    );
    let redo = plan(&p.shoot, &tool, None, &p.target).unwrap();
    assert_eq!((redo.new, redo.existing), (0, 2));
    assert_eq!(
        redo.photos
            .iter()
            .find(|p| p.source.contains("DSC00003_(2)"))
            .unwrap()
            .name,
        "DSC00003.jpg"
    );
}

#[test]
fn a_file_or_folder_that_appears_meanwhile_is_never_overwritten() {
    let Some(tool) = tool() else {
        return;
    };
    let p = place_for("2026-11-07_test");
    jpeg(&p.shoot.join("FINAL/A.jpg"), &["-Software=PixCake"], &tool);
    jpeg(&p.shoot.join("FINAL/B.jpg"), &["-Software=PixCake"], &tool);
    let planned = plan(&p.shoot, &tool, None, &p.target).unwrap();
    // While copies are being checked, someone creates the folder and puts A.jpg in it.
    let target = p.target.clone();
    let r = deliver_with(
        &p.shoot,
        &tool,
        None,
        &p.target,
        &planned.confirmed(),
        |source, copy| {
            if !target.exists() {
                fs::create_dir(&target).unwrap();
                fs::write(target.join("A.jpg"), b"theirs").unwrap();
            }
            verify(source, copy)
        },
    )
    .unwrap();
    assert_eq!((r.delivered, r.existing), (1, 1));
    assert_eq!(fs::read(p.target.join("A.jpg")).unwrap(), b"theirs");
    assert!(p.target.join("B.jpg").is_file());
    assert_eq!(names(&p.target), ["A.jpg", "B.jpg"]);
    assert!(!p.shoot.join(STAGING).exists());

    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    fs::write(&a, b"new").unwrap();
    fs::write(&b, b"old").unwrap();
    assert_eq!(
        rename_new(&a, &b).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(fs::read(&b).unwrap(), b"old");
    fs::create_dir(dir.path().join("x")).unwrap();
    fs::create_dir(dir.path().join("y")).unwrap();
    fs::write(dir.path().join("y/keep"), b"keep").unwrap();
    assert_eq!(
        rename_new(&dir.path().join("x"), &dir.path().join("y"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    assert!(dir.path().join("y/keep").is_file());
}

#[test]
fn a_changed_selection_is_refused_and_a_failed_copy_is_offered_again() {
    let Some(tool) = tool() else {
        return;
    };
    let p = place_for("2026-11-08_x");
    jpeg(&p.shoot.join("FINAL/A.jpg"), &["-Software=PixCake"], &tool);
    let planned = plan(&p.shoot, &tool, None, &p.target).unwrap();
    jpeg(&p.shoot.join("FINAL/B.jpg"), &["-Software=PixCake"], &tool);
    let err = deliver(&p.shoot, &tool, None, &p.target, &planned.confirmed()).unwrap_err();
    assert!(err.to_string().contains("changed since you checked"));
    assert!(!p.target.exists() && !p.shoot.join(STAGING).exists());

    let planned = plan(&p.shoot, &tool, None, &p.target).unwrap();
    let r = deliver_with(
        &p.shoot,
        &tool,
        None,
        &p.target,
        &planned.confirmed(),
        |source, copy| {
            if source.ends_with("A.jpg") {
                Err(io::Error::other("injected read-back failure"))
            } else {
                verify(source, copy)
            }
        },
    )
    .unwrap();
    assert_eq!((r.delivered, r.errors.len()), (1, 1));
    assert_eq!(names(&p.target), ["B.jpg"]);
    assert!(!p.shoot.join(STAGING).exists());
    let retry = plan(&p.shoot, &tool, None, &p.target).unwrap();
    assert_eq!(retry.confirmed().len(), 1);
    assert_eq!(run(&p, &tool, None).delivered, 1);
    assert_eq!(names(&p.target), ["A.jpg", "B.jpg"]);

    // A leftover staging folder from an interrupted delivery is cleared first.
    fs::create_dir(p.shoot.join(STAGING)).unwrap();
    fs::write(p.shoot.join(STAGING).join("partial.jpg"), b"x").unwrap();
    fs::remove_file(p.target.join("A.jpg")).unwrap();
    assert_eq!(run(&p, &tool, None).delivered, 1);
    assert!(!p.shoot.join(STAGING).exists());
    assert_eq!(names(&p.target), ["A.jpg", "B.jpg"]);

    // Two different photos that clean up to the same name.
    let q = place_for("2026-11-09_x");
    jpeg(
        &q.shoot.join("topix/EM_1.jpg"),
        &["-Software=Adobe Lightroom"],
        &tool,
    );
    jpeg(
        &q.shoot.join("final/EM_1-DxO_DeepPRIME XD3.jpg"),
        &["-Software=PixCake"],
        &tool,
    );
    run(&q, &tool, None);
    assert_eq!(names(&q.target), ["EM_1-2.jpg", "EM_1.jpg"]);
    // The finished photos folder can't be inside the shoot.
    assert!(plan(&q.shoot, &tool, None, &q.shoot.join("out")).is_err());
}

#[test]
fn catalog_stars_go_into_the_delivered_copy_only() {
    let Some(tool) = tool() else {
        return;
    };
    let p = place_for("shoot");
    let source = p.shoot.join("FINAL/EM_01624.jpg");
    jpeg(
        &source,
        &[
            "-Software=PixCake",
            "-XMP-xmp:Rating=0",
            "-IFD0:Rating=0",
            "-IFD0:RatingPercent=0",
        ],
        &tool,
    );
    fs::write(p.shoot.join("EM_01624.xmp"), r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="2"/></rdf:RDF></x:xmpmeta>"#).unwrap();
    let catalog = p.shoot.parent().unwrap().join("test.lrcat");
    let _db = tiny_catalog(&catalog, &p.shoot.to_string_lossy(), true);
    let before = hash_file(&source).unwrap();
    let r = run(&p, &tool, Some(&catalog));
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(
        (r.photos[0].stars, r.photos[0].stars_from),
        (Some(5), StarsFrom::Catalog)
    );
    let meta = metadata(&tool, &[p.target.join("EM_01624.jpg")]).unwrap();
    let meta = meta.values().next().unwrap();
    assert_eq!(rating(meta), Some(5));
    assert_eq!(meta["IFD0:Rating"], 5);
    assert_eq!(meta["IFD0:RatingPercent"], 99);
    assert_eq!(hash_file(&source).unwrap(), before);
    fs::remove_file(p.target.join("EM_01624.jpg")).unwrap();
    let r = plan(
        &p.shoot,
        &tool,
        Some(&p.shoot.join("unreadable.lrcat")),
        &p.target,
    )
    .unwrap();
    assert_eq!(r.warnings.len(), 1);
    assert_eq!(
        (r.photos[0].stars, r.photos[0].stars_from),
        (Some(2), StarsFrom::Sidecar)
    );
    fs::remove_file(p.shoot.join("EM_01624.xmp")).unwrap();
    let r = plan(&p.shoot, &tool, None, &p.target).unwrap();
    assert_eq!(
        (r.photos[0].stars, r.photos[0].stars_from),
        (Some(0), StarsFrom::None)
    );
}

#[test]
fn duplicate_lightroom_exports_use_document_id_and_the_newest_wins() {
    let Some(tool) = tool() else {
        return;
    };
    let p = place_for("dupes");
    jpeg(
        &p.shoot.join("arbitrary folder/old.jpg"),
        &[
            "-Software=Adobe Lightroom",
            "-XMP-xmpMM:DocumentID=xmp.did:shared",
        ],
        &tool,
    );
    jpeg(
        &p.shoot.join("another/new.jpg"),
        &[
            "-Software=Adobe Lightroom",
            "-XMP-xmpMM:DocumentID=xmp.did:shared",
        ],
        &tool,
    );
    let old_file = fs::OpenOptions::new()
        .write(true)
        .open(p.shoot.join("arbitrary folder/old.jpg"))
        .unwrap();
    old_file
        .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(1))
        .unwrap();
    drop(old_file);
    let r = plan(&p.shoot, &tool, None, &p.target).unwrap();
    assert_eq!(r.photos.len(), 1);
    assert_eq!(r.photos[0].name, "new.jpg");
    assert!(r.photos[0].source.ends_with("new.jpg"));
    assert_eq!(r.skipped[0].reason, "older version");
}

#[test]
fn a_damaged_list_of_deliveries_stops_before_anything_is_copied() {
    let p = place_for("damaged");
    fs::write(
        p.shoot.join(MANIFEST),
        r#"{"version":2,"assignments":[{"name":"../x.jpg","identities":[]}]}"#,
    )
    .unwrap();
    let err = plan(&p.shoot, Path::new("exiftool"), None, &p.target).unwrap_err();
    assert!(err.to_string().contains("damaged"));
}

#[test]
fn dng_fallback_and_catalog_preference_use_the_dxo_stem_before_the_preserved_raw() {
    let c = Candidate {
        relative: "final/EM_01415-DxO_DeepPRIME XD3.jpg".into(),
        stem: "EM_01415-DxO_DeepPRIME XD3".into(),
        meta: json!({"XMP-xmpMM:PreservedFileName":"EM_01415.ARW"}),
        kind: Kind::PixCake,
        modified_ns: 1,
        identities: vec![],
    };
    let mut fallback =
        BTreeMap::from([("dng:em_01415-dxo_deepprime xd3".into(), (4, StarsFrom::Dng))]);
    assert_eq!(stars(&c, &[], &fallback), (Some(4), StarsFrom::Dng));
    fallback.insert("xmp:em_01415".into(), (2, StarsFrom::Sidecar));
    assert_eq!(stars(&c, &[], &fallback), (Some(2), StarsFrom::Sidecar));
    let catalog = vec![
        CatalogPhoto {
            base: "EM_01415".into(),
            extension: "ARW".into(),
            rating: 3,
        },
        CatalogPhoto {
            base: c.stem.clone(),
            extension: "dng".into(),
            rating: 5,
        },
    ];
    assert_eq!(
        stars(&c, &catalog, &fallback),
        (Some(5), StarsFrom::Catalog)
    );
}
