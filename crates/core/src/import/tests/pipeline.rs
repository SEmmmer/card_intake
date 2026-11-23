use super::*;

fn delayed(flush_ms: u64) -> upload::Hooks {
    upload::Hooks {
        flush_delay: Duration::from_millis(flush_ms),
        ..Default::default()
    }
}

#[test]
fn eight_finishers_overlap_flushes_and_the_backlog_is_bounded() {
    let mut f = Fixture::new();
    through_laptop(&mut f);
    add_photos(&f, 31);
    let hooks = delayed(200);
    let max = hooks.max_backlog.clone();
    let start = Instant::now();
    let mut done = Vec::new();
    let mut last = None;
    let r = run_engine(
        &f.request,
        false,
        false,
        None,
        &AtomicBool::new(false),
        |_| Ok(()),
        |p| {
            if p.card_done.is_some() {
                done.push(start.elapsed());
            }
            if p.nas.is_some_and(|n| n.completed_files == 32) {
                last.get_or_insert_with(Instant::now);
            }
        },
        hooks,
    );
    let elapsed = start.elapsed();
    assert_eq!(r.status, ImportStatus::Complete, "{:?}", r.error);
    assert_eq!(done.len(), 1);
    assert!(
        done[0] + Duration::from_millis(150) < elapsed,
        "card done {done:?}, end {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(800) && elapsed < Duration::from_millis(4500),
        "{elapsed:?}; serial flushes alone would take 6.4 s"
    );
    assert!(max.load(Ordering::Relaxed) <= RELAY_BACKLOG);
    assert!(max.load(Ordering::Relaxed) > 0);
    assert!(r.timings.flushing_seconds >= 6.4);
    assert!(last.unwrap().elapsed() < Duration::from_millis(400));
    assert!(f.parts().is_empty());
    eprintln!(
        "32 files, 200 ms flush, 8 finishers: {elapsed:?}; card done {:?}; backlog {}",
        done[0],
        max.load(Ordering::Relaxed)
    );
}

#[test]
fn a_new_raw_is_never_visible_before_its_rated_sidecar_even_with_slow_sidecars() {
    let mut f = Fixture::new();
    let (_, nas) = through_laptop(&mut f);
    let permissions = lock(&f);
    let finished = AtomicBool::new(false);
    let raw = nas.join("2026/2026-11-06_Tokyo-秋/DSC00001.ARW");
    let r = std::thread::scope(|scope| {
        let observer = scope.spawn(|| {
            let mut saw = false;
            while !finished.load(Ordering::Relaxed) {
                if raw.exists() {
                    let xmp = fs::read_to_string(raw.with_extension("xmp"))
                        .expect("sidecar must precede raw");
                    assert!(xmp.contains("Locked in camera") && xmp.contains("xmp:Rating=\"5\""));
                    saw = true;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            saw
        });
        let hooks = upload::Hooks {
            sidecar_delay: Duration::from_millis(200),
            ..delayed(50)
        };
        let r = run_engine(
            &f.request,
            false,
            false,
            None,
            &AtomicBool::new(false),
            |_| Ok(()),
            |_| {},
            hooks,
        );
        finished.store(true, Ordering::Relaxed);
        assert!(observer.join().unwrap());
        r
    });
    unlock(&f, permissions);
    assert_eq!(r.status, ImportStatus::Complete, "{:?}", r.error);
    assert!(r.timings.nas_sidecar_seconds >= 0.2);
}

#[test]
fn a_jpeg_that_sorts_first_cannot_race_its_raws_card_sidecar_or_send_it_elsewhere() {
    let mut f = Fixture::new();
    let (_, nas) = through_laptop(&mut f);
    let raw = f.request.root.join("DCIM/100SONY/DSC00001.arw");
    fs::rename(f.request.root.join("DCIM/100SONY/DSC00001.ARW"), &raw).unwrap();
    f.add(
        "DCIM/100SONY/DSC00001.JPG",
        &crate::exif::tests::jpeg(&crate::exif::tests::sample(true)),
    );
    f.add("DCIM/100SONY/DSC00001.xmp", br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:custom="urn:card" custom:Keep="card setting"/></rdf:RDF></x:xmpmeta>"#);
    let original = fs::metadata(&raw).unwrap().permissions();
    let mut locked = original.clone();
    locked.set_readonly(true);
    fs::set_permissions(&raw, locked).unwrap();
    offline::prepare(&f.request.nas_root).unwrap();
    let p = plan(&f.request, false, None).unwrap();
    let jpeg = p
        .groups
        .iter()
        .find(|g| p.entries[g[0]].source.extension().unwrap() == "JPG")
        .unwrap();
    let raw_group = p
        .groups
        .iter()
        .find(|g| scan::uses_raw_sidecar(&p.entries[g[0]].source))
        .unwrap();
    assert_eq!(jpeg.len(), 1);
    assert_eq!(raw_group.len(), 2);
    let r = run(&f.request, false, &AtomicBool::new(false), |_| {});
    fs::set_permissions(raw, original).unwrap();
    assert_eq!(r.status, ImportStatus::Complete, "{:?}", r.error);
    let xmp = fs::read_to_string(nas.join("2026/2026-11-06_Tokyo-秋/DSC00001.xmp")).unwrap();
    assert!(xmp.contains("card setting") && xmp.contains("Locked in camera"));
}

#[test]
fn card_done_needs_no_more_card_reads_and_interrupted_recovery_can_be_sent() {
    let mut f = Fixture::new();
    let (laptop, nas) = through_laptop(&mut f);
    add_photos(&f, 31);
    let cancel = AtomicBool::new(false);
    let saved = f._dir.path().join("recovery.json");
    let ledger_path = f._dir.path().join("ledger.json");
    let mut notifications = 0;
    let r = run_engine(
        &f.request,
        false,
        false,
        None,
        &cancel,
        |snapshot| {
            send::Manifest {
                offline_root: snapshot.nas_root.clone(),
                shoots: snapshot.shoots.clone(),
                files: snapshot
                    .local_files
                    .iter()
                    .map(|f| send::VerifiedFile {
                        relative: f.relative.clone(),
                        size: f.size,
                        modified: f.modified,
                    })
                    .collect(),
            }
            .save(&saved)?;
            let mut ledger = crate::ledger::Ledger::default();
            ledger.record_import(snapshot.clone());
            ledger.save(&ledger_path).unwrap();
            // The card has left: any subsequent attempt to read it will fail.
            fs::rename(&f.request.root, f._dir.path().join("removed-card")).unwrap();
            Ok(())
        },
        |p| {
            if p.card_done.is_some() {
                notifications += 1;
                cancel.store(true, Ordering::Relaxed);
            }
        },
        delayed(200),
    );
    assert_eq!(notifications, 1);
    assert_eq!(r.status, ImportStatus::Cancelled);
    assert!(r.local_complete && r.offline);
    assert!(r.error.as_ref().unwrap().contains("Send to NAS"));
    assert!(!r.error.as_ref().unwrap().contains("not ejected"));
    assert!(f.parts().is_empty());
    for file in &r.local_files {
        assert!(file.source.is_file());
    }
    let manifest = send::Manifest::load(&saved).unwrap();
    let record = crate::ledger::Ledger::load(&ledger_path)
        .unwrap()
        .imports
        .remove(0);
    assert!(record.uploading && record.offline && record.local_complete);
    assert_eq!(record.started_at, r.started_at);
    let sent = send::send_import(&manifest, &[], &nas, false, &AtomicBool::new(false), |_| {});
    assert_eq!(sent.status, ImportStatus::Complete, "{:?}", sent.error);
    assert_eq!(sent.counts.verified, 32);
    assert!(laptop.join(&record.shoots[0]).is_dir());
}

#[test]
fn removing_the_card_at_card_done_does_not_interrupt_a_successful_upload() {
    let mut f = Fixture::new();
    through_laptop(&mut f);
    let permissions = lock(&f);
    let removed = f._dir.path().join("removed");
    let r = run_engine(
        &f.request,
        false,
        false,
        None,
        &AtomicBool::new(false),
        |snapshot| {
            fs::rename(&f.request.root, &removed).unwrap();
            snapshot.warnings.push("Fixture ejection note".into());
            Ok(())
        },
        |_| {},
        delayed(150),
    );
    fs::set_permissions(removed.join("DCIM/100SONY/DSC00001.ARW"), permissions).unwrap();
    assert_eq!(r.status, ImportStatus::Complete, "{:?}", r.error);
    assert_eq!(r.ratings.locked, 1);
    assert!(r.warnings.iter().any(|w| w == "Fixture ejection note"));
    assert!(r.laptop_deleted);
}

#[test]
fn a_failing_upload_removes_its_parts_and_keeps_every_laptop_file() {
    let mut f = Fixture::new();
    through_laptop(&mut f);
    add_photos(&f, 31);
    let hooks = upload::Hooks {
        fail_file: Some("DSC00008.ARW".into()),
        ..delayed(100)
    };
    let r = run_engine(
        &f.request,
        false,
        false,
        None,
        &AtomicBool::new(false),
        |_| Ok(()),
        |_| {},
        hooks,
    );
    assert_eq!(r.status, ImportStatus::Failed);
    assert!(r.local_complete && r.offline);
    assert_eq!(r.local_files.len(), 32);
    for file in &r.local_files {
        copies::unchanged(file).unwrap();
    }
    assert!(f.parts().is_empty());
}

#[test]
fn an_unexpected_card_worker_failure_cannot_declare_card_done() {
    let mut f = Fixture::new();
    through_laptop(&mut f);
    let mut ready = false;
    let mut panicked = false;
    let r = run_engine(
        &f.request,
        false,
        false,
        None,
        &AtomicBool::new(false),
        |_| {
            ready = true;
            Ok(())
        },
        |p| {
            if p.phase == "file_complete" && !panicked {
                panicked = true;
                panic!("Injected card worker failure");
            }
        },
        Default::default(),
    );
    assert_eq!(r.status, ImportStatus::Failed);
    assert!(!ready && !r.local_complete);
    assert!(r.error.unwrap().contains("worker stopped unexpectedly"));
    assert!(f.parts().is_empty());
}

#[test]
fn stop_after_the_nas_has_everything_does_not_misreport_deleted_laptop_files_as_safe_locally() {
    let mut f = Fixture::new();
    let (_, nas) = through_laptop(&mut f);
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    let hooks = upload::Hooks {
        before_delete: Some(Arc::new(move || stop.store(true, Ordering::Relaxed))),
        ..Default::default()
    };
    let r = run_engine(
        &f.request,
        false,
        false,
        None,
        &cancel,
        |_| Ok(()),
        |_| {},
        hooks,
    );
    assert!(cancel.load(Ordering::Relaxed));
    assert_eq!(r.status, ImportStatus::Complete, "{:?}", r.error);
    assert!(r.error.is_none() && r.laptop_deleted && !r.offline);
    assert!(nas.join("2026/2026-11-06_Tokyo-秋/DSC00001.ARW").is_file());
}

#[test]
fn uncached_readback_handles_empty_partial_sector_and_multiple_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = AtomicBool::new(false);
    for size in [0, 1, 511, 512, 4095, 4096, CHUNK + 31] {
        let data = vec![113u8; size];
        let path = dir.path().join("file");
        let mut file = File::create(&path).unwrap();
        file.write_all(&data).unwrap();
        file.sync_all().unwrap();
        drop(file);
        assert_eq!(
            readback::hash(&path, size as u64, &cancel).unwrap(),
            xxhash_rust::xxh64::xxh64(&data, 0)
        );
    }
}

#[test]
fn a_full_backlog_stalls_writers_without_holding_the_card() {
    let mut f = Fixture::new();
    through_laptop(&mut f);
    add_photos(&f, 39);
    let gate = Arc::new(AtomicBool::new(false));
    let hooks = upload::Hooks {
        finish_gate: Some(gate.clone()),
        ..Default::default()
    };
    let max = hooks.max_backlog.clone();
    let r = run_engine(
        &f.request,
        false,
        false,
        None,
        &AtomicBool::new(false),
        |_| {
            let start = Instant::now();
            while max.load(Ordering::Relaxed) < RELAY_BACKLOG
                && start.elapsed() < Duration::from_secs(3)
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            let backlog = max.load(Ordering::Relaxed);
            gate.store(true, Ordering::Relaxed);
            assert_eq!(backlog, RELAY_BACKLOG);
            Ok(())
        },
        |_| {},
        hooks,
    );
    assert_eq!(r.status, ImportStatus::Complete, "{:?}", r.error);
    assert_eq!(max.load(Ordering::Relaxed), RELAY_BACKLOG);
    assert!(r.timings.backlog_wait_seconds > 0.0);
}

#[test]
fn a_corrupt_readback_is_not_published_and_keeps_the_complete_laptop_import() {
    let mut f = Fixture::new();
    let (_, nas) = through_laptop(&mut f);
    let hooks = upload::Hooks {
        after_flush: Some(Arc::new(|part| {
            let len = fs::metadata(part).unwrap().len();
            fs::write(part, vec![17u8; len as usize]).unwrap();
        })),
        ..Default::default()
    };
    let r = run_engine(
        &f.request,
        false,
        false,
        None,
        &AtomicBool::new(false),
        |_| Ok(()),
        |_| {},
        hooks,
    );
    assert_eq!(r.status, ImportStatus::Failed);
    assert!(r.error.unwrap().contains("Verification failed"));
    assert!(r.local_complete && r.local_files.iter().all(|f| f.source.is_file()));
    assert!(!nas.join("2026/2026-11-06_Tokyo-秋/DSC00001.ARW").exists());
    assert!(f.parts().is_empty());
}

#[test]
fn a_different_file_created_at_publication_is_never_replaced() {
    let mut f = Fixture::new();
    let (_, nas) = through_laptop(&mut f);
    let hooks = upload::Hooks {
        before_publish: Some(Arc::new(|target| {
            if scan::uses_raw_sidecar(target) {
                fs::write(target, b"another writer's photo").unwrap();
            }
        })),
        ..Default::default()
    };
    let r = run_engine(
        &f.request,
        false,
        false,
        None,
        &AtomicBool::new(false),
        |_| Ok(()),
        |_| {},
        hooks,
    );
    assert_eq!(r.status, ImportStatus::Failed);
    assert!(r.error.unwrap().contains("different contents"));
    assert_eq!(
        fs::read(nas.join("2026/2026-11-06_Tokyo-秋/DSC00001.ARW")).unwrap(),
        b"another writer's photo"
    );
    assert!(r.local_complete && f.parts().is_empty());
}

#[test]
fn a_different_card_sidecar_kept_on_the_nas_still_keeps_its_local_original() {
    let mut f = Fixture::new();
    let (laptop, nas) = through_laptop(&mut f);
    f.add("M4ROOT/CLIP/C0001.MP4", b"clip");
    f.add("M4ROOT/CLIP/C0001M01.XML", SIDECAR);
    let shoot = PathBuf::from("2026/2026-11-06_Tokyo-秋");
    fs::create_dir_all(nas.join(&shoot).join("video")).unwrap();
    fs::write(nas.join(&shoot).join("video/C0001M01.XML"), b"NAS edits").unwrap();
    let r = run(&f.request, false, &AtomicBool::new(false), |_| {});
    assert_eq!(r.status, ImportStatus::Complete, "{:?}", r.error);
    assert!(!r.offline && !r.laptop_deleted);
    assert!(r.sent.is_some());
    assert!(!laptop.join(&shoot).join("DSC00001.ARW").exists());
    assert!(!laptop.join(&shoot).join("video/C0001.MP4").exists());
    assert_eq!(
        fs::read(laptop.join(&shoot).join("video/C0001M01.XML")).unwrap(),
        SIDECAR
    );
    assert_eq!(
        fs::read(nas.join(&shoot).join("video/C0001M01.XML")).unwrap(),
        b"NAS edits"
    );
    assert!(r.warnings.iter().any(|w| w.contains("version was kept")));
}

#[test]
fn corrupt_laptop_bytes_with_unchanged_size_and_time_still_fail_the_card_checksum() {
    let mut f = Fixture::new();
    through_laptop(&mut f);
    add_photos(&f, 63);
    let gate = Arc::new(AtomicBool::new(false));
    let hooks = upload::Hooks {
        finish_gate: Some(gate.clone()),
        ..Default::default()
    };
    let r = run_engine(
        &f.request,
        false,
        false,
        None,
        &AtomicBool::new(false),
        |snapshot| {
            let file = snapshot
                .local_files
                .iter()
                .find(|f| f.relative.file_name().unwrap() == "DSC00064.ARW")
                .unwrap();
            let mut bytes = fs::read(&file.source).unwrap();
            bytes[0] ^= 255;
            fs::write(&file.source, bytes).unwrap();
            File::options()
                .write(true)
                .open(&file.source)
                .unwrap()
                .set_modified(file.modified)
                .unwrap();
            gate.store(true, Ordering::Relaxed);
            Ok(())
        },
        |_| {},
        hooks,
    );
    assert_eq!(r.status, ImportStatus::Failed);
    assert!(r.error.unwrap().contains(copies::SOURCE_MISMATCH));
    assert!(f.parts().is_empty());
}

#[test]
fn nas_progress_retains_queued_bytes_and_uses_a_recent_speed_window() {
    let dir = tempfile::tempdir().unwrap();
    let relay = Relay::new(dir.path(), false, &[]);
    {
        let mut transfer = relay.transfer.lock().unwrap();
        transfer.written = 10_000;
        transfer.verified = 8_000;
        transfer.verified_written = 8_000;
    }
    let now = Instant::now();
    *relay.window.lock().unwrap() = VecDeque::from([
        (now - Duration::from_secs(10), 0, 0),
        (now - Duration::from_secs(2), 8_000, 8_000),
    ]);
    let p = relay.progress();
    assert_eq!(p.processed_bytes, 9_000); // queued writes keep their half credit
    assert!((990.0..=1010.0).contains(&p.bytes_per_sec.unwrap()));
    let mut transfer = relay.transfer.lock().unwrap();
    transfer.verified = 10_000;
    transfer.verified_written = 10_000;
    drop(transfer);
    assert_eq!(relay.progress().processed_bytes, 10_000);
}

#[test]
fn the_rating_tail_stays_small_as_the_number_of_raws_grows() {
    for count in [16, 96] {
        let mut f = Fixture::new();
        through_laptop(&mut f);
        add_photos(&f, count - 1);
        let mut permissions = Vec::new();
        for file in scan::scan_card(&f.request.root).unwrap() {
            let original = fs::metadata(&file.path).unwrap().permissions();
            let mut locked = original.clone();
            locked.set_readonly(true);
            fs::set_permissions(&file.path, locked).unwrap();
            permissions.push((file.path, original));
        }
        let mut last = None;
        let r = run_engine(
            &f.request,
            false,
            false,
            None,
            &AtomicBool::new(false),
            |_| Ok(()),
            |p| {
                if p.nas.is_some_and(|n| n.completed_files == count) {
                    last.get_or_insert_with(Instant::now);
                }
            },
            upload::Hooks {
                sidecar_delay: Duration::from_millis(40),
                ..delayed(10)
            },
        );
        for (path, permissions) in permissions {
            fs::set_permissions(path, permissions).unwrap();
        }
        assert_eq!(r.status, ImportStatus::Complete, "{:?}", r.error);
        assert_eq!(r.ratings.rated, count);
        let tail = last.unwrap().elapsed();
        assert!(
            tail < Duration::from_millis(900),
            "{count} raws: tail {tail:?}; a serial rating pass alone would take {:?}",
            Duration::from_millis(count as u64 * 40)
        );
    }
}

#[cfg(windows)]
#[test]
fn destination_junctions_are_refused_and_checked_folders_cannot_be_replaced() {
    let mut f = Fixture::new();
    let (_, nas) = through_laptop(&mut f);
    let outside = f._dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let junction = nas.join("2026");
    let output = crate::process::command(Path::new("cmd"))
        .args(["/c", "mklink", "/J"])
        .arg(&junction)
        .arg(&outside)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let r = run_engine(
        &f.request,
        false,
        false,
        None,
        &AtomicBool::new(false),
        |_| Ok(()),
        |_| {},
        Default::default(),
    );
    assert_eq!(r.status, ImportStatus::Failed);
    assert!(r.error.unwrap().contains("junction"));
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    fs::remove_dir(&junction).unwrap();
    let folders = upload::Folders::default();
    folders
        .ensure(&nas, Path::new("2026/test/file.ARW"))
        .unwrap();
    assert!(fs::rename(nas.join("2026"), nas.join("moved")).is_err());
    drop(folders);
    fs::rename(nas.join("2026"), nas.join("moved")).unwrap();
}

/// A Sony `M01.XML` for a clip shot on the fixture's date.
fn sony_xml(width: u32, height: u32, capture: &str, format: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<NonRealTimeMeta xmlns="urn:schemas-professionalDisc:nonRealTimeMeta:ver.2.20">
	<CreationDate value="2026-11-06T12:00:00+09:00"/>
	<VideoFormat>
		<VideoRecPort port="DIRECT"/>
		<VideoFrame videoCodec="HEVC" captureFps="{capture}" formatFps="{format}"/>
		<VideoLayout pixel="{width}" numOfVerticalLine="{height}" aspectRatio="16:9"/>
	</VideoFormat>
	<SubStream codec="AVC_Proxy_1280_720_MP@L4"/>
</NonRealTimeMeta>"#
    )
    .into_bytes()
}

/// A clip as an α7R VI writes it: the clip and its XML in `M4ROOT/CLIP`, the proxy
/// in `M4ROOT/SUB`.
fn add_clip(f: &Fixture, name: &str, xml: &[u8]) {
    f.add(
        &format!("M4ROOT/CLIP/{name}.MP4"),
        format!("{name} clip").as_bytes(),
    );
    f.add(&format!("M4ROOT/CLIP/{name}M01.XML"), xml);
    f.add(
        &format!("M4ROOT/SUB/{name}S03.MP4"),
        format!("{name} proxy").as_bytes(),
    );
}

fn assert_clip(folder: &Path, name: &str, xml: &[u8]) {
    let read = |file: String| {
        fs::read(folder.join(&file))
            .unwrap_or_else(|e| panic!("{}: {e}", folder.join(file).display()))
    };
    assert_eq!(
        read(format!("{name}.MP4")),
        format!("{name} clip").as_bytes()
    );
    assert_eq!(read(format!("{name}M01.XML")), xml);
    assert_eq!(
        read(format!("{name}S03.MP4")),
        format!("{name} proxy").as_bytes()
    );
}

fn files_in(folder: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(folder)
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.file_type().unwrap().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn clips_go_into_spec_folders_with_their_xml_and_proxy() {
    let f = Fixture::new();
    let clips = [
        ("C0001", sony_xml(3840, 2160, "29.97p", "29.97p"), "4k30"),
        ("C0002", sony_xml(3840, 2160, "59.94p", "59.94p"), "4k60"),
        ("C0003", sony_xml(7680, 4320, "29.97p", "29.97p"), "8k30"),
        // Slow motion: shot at 120, saved at 30.
        (
            "C0004",
            sony_xml(3840, 2160, "119.88p", "29.97p"),
            "4k120-30",
        ),
        ("C0005", sony_xml(1920, 1080, "59.94p", "59.94p"), "1080p60"),
    ];
    for (name, xml, _) in &clips {
        add_clip(&f, name, xml);
    }
    let r = f.run();
    assert_eq!(r.status, ImportStatus::Complete, "{:?}", r.error);
    assert_eq!((r.photos, r.videos, r.total_files), (1, 5, 16));
    let video = f.folder().join("video");
    for (name, xml, spec) in &clips {
        assert_clip(&video.join(spec), name, xml);
    }
    let mut specs: Vec<_> = fs::read_dir(&video)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    specs.sort();
    assert_eq!(specs, ["1080p60", "4k120-30", "4k30", "4k60", "8k30"]);

    // Importing the card again finds every file where it went.
    let again = f.run();
    assert_eq!(again.status, ImportStatus::Complete, "{:?}", again.error);
    assert_eq!(again.copied_files, 0);
    assert_eq!(again.skipped_files, again.total_files);
    assert!(
        !again.warnings.iter().any(|w| w.contains("kept")),
        "{:?}",
        again.warnings
    );
}

#[test]
fn a_clip_already_in_the_flat_video_folder_gets_its_proxy_beside_it() {
    let f = Fixture::new();
    let xml = sony_xml(3840, 2160, "29.97p", "29.97p");
    add_clip(&f, "C0001", &xml);
    // An import from before spec folders left the clip and its XML in `video`.
    let video = f.folder().join("video");
    fs::create_dir_all(&video).unwrap();
    fs::write(video.join("C0001.MP4"), b"C0001 clip").unwrap();
    fs::write(video.join("C0001M01.XML"), &xml).unwrap();
    let r = f.run();
    assert_eq!(r.status, ImportStatus::Complete, "{:?}", r.error);
    assert_clip(&video, "C0001", &xml);
    assert!(!video.join("4k30").exists(), "the clip was copied twice");
    assert_eq!(r.copied_files, 2, "the raw and the proxy");
}

#[test]
fn a_clip_under_another_title_in_a_spec_folder_is_not_copied_again() {
    let f = Fixture::new();
    let xml = sony_xml(7680, 4320, "29.97p", "29.97p");
    add_clip(&f, "C0001", &xml);
    let other = f
        .request
        .nas_root
        .join("2026/2026-11-06_first-card/video/8k30");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("C0001.MP4"), b"C0001 clip").unwrap();
    let r = f.run();
    assert_eq!(r.status, ImportStatus::Complete, "{:?}", r.error);
    assert_clip(&other, "C0001", &xml);
    assert!(!f.folder().join("video").exists());
    assert_eq!(r.skipped_elsewhere, 1);
}

#[test]
fn through_this_computer_clips_and_proxies_reach_their_spec_folders() {
    let mut f = Fixture::new();
    let (laptop, nas) = through_laptop(&mut f);
    let shoot = PathBuf::from("2026/2026-11-06_Tokyo-秋");
    let four_k = sony_xml(3840, 2160, "59.94p", "59.94p");
    let eight_k = sony_xml(7680, 4320, "29.97p", "29.97p");
    add_clip(&f, "C0001", &four_k);
    add_clip(&f, "C0002", &eight_k);
    // The NAS already has C0002 from before spec folders, without its proxy.
    let flat = nas.join(&shoot).join("video");
    fs::create_dir_all(&flat).unwrap();
    fs::write(flat.join("C0002.MP4"), b"C0002 clip").unwrap();
    fs::write(flat.join("C0002M01.XML"), &eight_k).unwrap();
    let r = run(&f.request, false, &AtomicBool::new(false), |_| {});
    assert_eq!(r.status, ImportStatus::Complete, "{:?}", r.error);
    assert!(r.laptop_deleted, "{:?}", r.warnings);
    assert_clip(&nas.join(&shoot).join("video/4k60"), "C0001", &four_k);
    assert_clip(&flat, "C0002", &eight_k);
    assert!(!flat.join("8k30").exists(), "C0002 was copied twice");
    assert_eq!(
        files_in(&flat),
        ["C0002.MP4", "C0002M01.XML", "C0002S03.MP4"]
    );
    assert!(!laptop.join(&shoot).join("video/4k60/C0001S03.MP4").exists());
    assert!(f.parts().is_empty());
}
