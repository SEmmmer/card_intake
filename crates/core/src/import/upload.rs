//! Four writers feed a bounded queue; eight finishers overlap NAS disk waits.
//! No laptop files are deleted until the entire upload succeeds.
use super::*;

#[derive(Clone, Default)]
pub(super) struct Hooks {
    #[cfg(test)]
    pub flush_delay: Duration,
    #[cfg(test)]
    pub sidecar_delay: Duration,
    #[cfg(test)]
    pub fail_file: Option<String>,
    #[cfg(test)]
    pub max_backlog: Arc<AtomicUsize>,
    #[cfg(test)]
    pub finish_gate: Option<Arc<AtomicBool>>,
    #[cfg(test)]
    pub after_flush: Option<TestAction>,
    #[cfg(test)]
    pub before_publish: Option<TestAction>,
    #[cfg(test)]
    pub before_delete: Option<Arc<dyn Fn() + Send + Sync>>,
}
#[cfg(test)]
type TestAction = Arc<dyn Fn(&Path) + Send + Sync>;
impl Hooks {
    fn wait(&self, sidecar: bool, cancel: &AtomicBool) -> Result<(), String> {
        #[cfg(test)]
        {
            while self
                .finish_gate
                .as_ref()
                .is_some_and(|g| !g.load(Ordering::Relaxed))
            {
                copies::cancelled(cancel)?;
                std::thread::sleep(Duration::from_millis(5));
            }
            let until = Instant::now()
                + if sidecar {
                    self.sidecar_delay
                } else {
                    self.flush_delay
                };
            while Instant::now() < until {
                copies::cancelled(cancel)?;
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let _ = sidecar;
        copies::cancelled(cancel)
    }
}

pub(super) struct Job {
    pub file: copies::CopyFile,
    pub companions: Vec<copies::CopyFile>,
    pub rated: Option<(rating::InitialRating, crate::xmp::Written)>,
    pub card_sidecars: HashMap<String, (u64, u64)>,
}
pub(super) struct Written {
    job: Job,
    target: PathBuf,
    found_in: Option<PathBuf>,
    temp: Option<tempfile::NamedTempFile>,
}
pub(super) struct Resolved {
    job: Job,
    target: PathBuf,
    found_in: Option<PathBuf>,
    exists: bool,
}

pub(super) fn resolve_files(
    relay: &Relay,
    queued: &Mutex<mpsc::Receiver<Job>>,
    resolved: &mpsc::Sender<Resolved>,
    cancel: &AtomicBool,
) {
    loop {
        let Ok(job) = queued.lock().unwrap_or_else(|e| e.into_inner()).recv() else {
            return;
        };
        if stopped(relay, cancel).is_err() {
            continue;
        }
        let label = job.file.relative.clone();
        let outcome = (|| {
            check_destination(&relay.nas, relay.require_network)?;
            let shoot: PathBuf = job.file.relative.components().take(2).collect();
            let others = send::date_folders(&relay.nas, send::shoot_date(&shoot)?, &relay.cache);
            let (target, found_in) =
                send::nas_target(&relay.nas, &shoot, &job.file, &others, &relay.failed)?;
            let exists = match fs::symlink_metadata(relay.nas.join(&target)) {
                Ok(_) => true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => false,
                Err(e) => return Err(e.to_string()),
            };
            // Do not create future shoot folders while resolving paths. Missing
            // folders are made by a writer immediately before its first .part.
            if exists {
                relay.folders.ensure(&relay.nas, &target)?;
            }
            let _ = resolved.send(Resolved {
                job,
                target,
                found_in,
                exists,
            });
            Ok::<_, String>(())
        })();
        if let Err(error) = outcome
            && !cancel.load(Ordering::Relaxed)
        {
            relay.fail(format!("{}: {error}", label.display()));
        }
    }
}

/// On Windows pin every checked ancestor without FILE_SHARE_DELETE, so a folder
/// cannot be renamed/replaced by a junction after its one full inspection. Check
/// the parent and leaf again at use. Other OSes retain full path checks.
#[derive(Default)]
pub(crate) struct Folders(Mutex<HashMap<PathBuf, Vec<File>>>, Sidecars);
impl Folders {
    pub(super) fn ensure(&self, root: &Path, relative: &Path) -> Result<(), String> {
        self.prepare(root, relative, true)?;
        self.check(root, relative)
    }

    pub(crate) fn pin_existing(&self, root: &Path, relative: &Path) -> Result<(), String> {
        self.prepare(root, relative, false)
    }

    /// Deletion still checks each leaf's type, size and time itself. Pinned
    /// Windows ancestors cannot be replaced; other OSes keep full path walks.
    pub(crate) fn inspect_for_delete(&self, root: &Path, relative: &Path) -> Result<(), String> {
        #[cfg(windows)]
        if self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(relative.parent().ok_or("Missing file folder")?)
        {
            let parent = root.join(relative.parent().unwrap());
            let meta = fs::symlink_metadata(&parent).map_err(|e| e.to_string())?;
            if !meta.is_dir() || copies::is_link(&meta) {
                return Err(format!("{} is a link or junction.", parent.display()));
            }
            return Ok(());
        }
        copies::inspect(root, relative)
    }

    fn prepare(&self, root: &Path, relative: &Path, create: bool) -> Result<(), String> {
        let parent = relative.parent().ok_or("Missing upload folder")?;
        let mut folders = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !folders.contains_key(parent) {
            copies::inspect(root, relative)?;
            if create {
                ensure_parent(root, relative)?;
            }
            let mut pins = Vec::new();
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                let mut path = root.to_path_buf();
                for part in std::iter::once(None).chain(parent.components().map(Some)) {
                    if let Some(part) = part {
                        path.push(part);
                    }
                    let handle = File::options()
                        .access_mode(0x80)
                        .share_mode(3)
                        .custom_flags(0x0220_0000)
                        .open(&path)
                        .map_err(|e| e.to_string())?;
                    let meta = handle.metadata().map_err(|e| e.to_string())?;
                    if !meta.is_dir() || copies::is_link(&meta) {
                        return Err(format!("{} is a link or junction.", path.display()));
                    }
                    pins.push(handle);
                }
            }
            #[cfg(not(windows))]
            let _ = &mut pins;
            folders.insert(parent.to_path_buf(), pins);
        }
        drop(folders);
        Ok(())
    }
    fn check(&self, root: &Path, relative: &Path) -> Result<(), String> {
        #[cfg(not(windows))]
        copies::inspect(root, relative)?;
        let parent = root.join(relative.parent().ok_or("Missing upload folder")?);
        let meta = fs::symlink_metadata(&parent).map_err(|e| e.to_string())?;
        if !meta.is_dir() || copies::is_link(&meta) {
            return Err(format!("{} is a link or junction.", parent.display()));
        }
        match fs::symlink_metadata(root.join(relative)) {
            Ok(meta) if !meta.is_file() || copies::is_link(&meta) => {
                Err("Upload target is not a regular file (links and junctions are refused).".into())
            }
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

pub(super) fn report<P: FnMut(ImportProgress)>(
    relay: &Relay,
    shared: &Mutex<Shared<P>>,
    file: &Path,
) {
    if let Ok(mut shared) = shared.try_lock() {
        let phase = if relay.card_done.load(Ordering::Relaxed) {
            "sending"
        } else {
            shared.phase()
        };
        shared.emit(phase, file);
    }
}

fn stopped(relay: &Relay, cancel: &AtomicBool) -> Result<(), String> {
    copies::cancelled(cancel)?;
    if relay.failed.load(Ordering::Relaxed) {
        Err("Upload stopped after an error.".into())
    } else {
        Ok(())
    }
}

fn write(
    relay: &Relay,
    file: &copies::CopyFile,
    target: &Path,
    cancel: &AtomicBool,
    measure: bool,
    progress: impl FnMut(u64),
) -> Result<tempfile::NamedTempFile, String> {
    stopped(relay, cancel)?;
    relay.folders.ensure(&relay.nas, target)?;
    copies::unchanged(file)?;
    let mut input = copies::open(&file.source)?;
    let mut temp = tempfile::Builder::new()
        .prefix(".card-intake-")
        .suffix(".part")
        .tempfile_in(relay.nas.join(target).parent().unwrap())
        .map_err(|e| e.to_string())?;
    let start = Instant::now();
    let outcome = copies::stream(
        &mut input,
        Some(temp.as_file_mut()),
        file.size,
        &relay.failed,
        progress,
    );
    if measure {
        relay
            .timings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .writing_seconds += start.elapsed().as_secs_f64();
    }
    let hash = outcome?;
    copies::unchanged(file)?;
    if file.hash.is_some_and(|h| h != hash) {
        return Err(copies::SOURCE_MISMATCH.into());
    }
    Ok(temp)
}

fn verify(
    relay: &Relay,
    temp: &tempfile::NamedTempFile,
    file: &copies::CopyFile,
    cancel: &AtomicBool,
    measure: bool,
) -> Result<(), String> {
    stopped(relay, cancel)?;
    temp.as_file()
        .set_modified(file.modified)
        .map_err(|e| e.to_string())?;
    let start = Instant::now();
    let outcome = relay
        .hooks
        .wait(false, &relay.failed)
        .and_then(|()| temp.as_file().sync_all().map_err(|e| e.to_string()));
    if measure {
        relay
            .timings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .flushing_seconds += start.elapsed().as_secs_f64();
    }
    outcome?;
    #[cfg(test)]
    if let Some(after_flush) = &relay.hooks.after_flush {
        after_flush(temp.path());
    }
    stopped(relay, cancel)?;
    let start = Instant::now();
    let hash = readback::hash(temp.path(), file.size, &relay.failed);
    if measure {
        relay
            .timings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reading_back_seconds += start.elapsed().as_secs_f64();
    }
    let expected = file.hash.ok_or("Missing source checksum")?;
    if hash? != expected {
        return Err("Verification failed. The incomplete copy was discarded.".into());
    }
    Ok(())
}

fn publish(
    relay: &Relay,
    temp: tempfile::NamedTempFile,
    target: &Path,
    cancel: &AtomicBool,
    measure: bool,
) -> Result<bool, String> {
    stopped(relay, cancel)?;
    relay.folders.check(&relay.nas, target)?;
    #[cfg(test)]
    if let Some(before_publish) = &relay.hooks.before_publish {
        before_publish(&relay.nas.join(target));
    }
    let start = Instant::now();
    let outcome = temp.persist_noclobber(relay.nas.join(target));
    if measure {
        relay
            .timings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publishing_seconds += start.elapsed().as_secs_f64();
    }
    match outcome {
        Ok(_) => Ok(true),
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(format!(
            "Couldn't publish {} (existing files are never replaced): {e}",
            target.display()
        )),
    }
}

fn existing(
    relay: &Relay,
    file: &copies::CopyFile,
    target: &Path,
    cancel: &AtomicBool,
) -> Result<Option<bool>, String> {
    stopped(relay, cancel)?;
    relay.folders.check(&relay.nas, target)?;
    match fs::symlink_metadata(relay.nas.join(target)) {
        Ok(meta) if meta.len() != file.size => Ok(Some(false)),
        Ok(_) => Ok(Some(
            readback::hash(&relay.nas.join(target), file.size, &relay.failed)?
                == file.hash.ok_or("Missing source checksum")?,
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

struct BacklogWait<'a> {
    relay: &'a Relay,
    started: Instant,
    waited: bool,
}
impl Drop for BacklogWait<'_> {
    fn drop(&mut self) {
        if self.waited {
            self.relay
                .timings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .backlog_wait_seconds += self.started.elapsed().as_secs_f64();
        }
    }
}

pub(super) fn send_files<P: FnMut(ImportProgress)>(
    slot: usize,
    relay: &Relay,
    queued: &Mutex<mpsc::Receiver<Resolved>>,
    finish: &mpsc::SyncSender<Written>,
    cancel: &AtomicBool,
    shared: &Mutex<Shared<P>>,
) {
    loop {
        let Ok(Resolved {
            job,
            target,
            found_in,
            exists,
        }) = queued.lock().unwrap_or_else(|e| e.into_inner()).recv()
        else {
            return;
        };
        if stopped(relay, cancel).is_err() {
            continue;
        }
        let label = job.file.relative.clone();
        relay
            .first_send
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(Instant::now);
        let outcome = (|| {
            check_destination(&relay.nas, relay.require_network)?;
            copies::unchanged(&job.file)?;
            let temp = if exists {
                // Even a skipped original must still have an unchanged laptop
                // source; checking the NAS belongs to the finishers.
                if copies::hash(&job.file.source, job.file.size, &relay.failed)?
                    != job.file.hash.ok_or("Missing source checksum")?
                {
                    return Err(copies::SOURCE_MISMATCH.into());
                }
                copies::unchanged(&job.file)?;
                None
            } else {
                Some(write(relay, &job.file, &target, cancel, true, |n| {
                    relay
                        .transfer
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .in_flight[slot] = n;
                    report(relay, shared, &label);
                })?)
            };
            {
                let mut transfer = relay.transfer.lock().unwrap_or_else(|e| e.into_inner());
                if temp.is_some() {
                    transfer.written += job.file.size;
                }
                transfer.in_flight[slot] = 0;
            }
            #[cfg(test)]
            if relay.hooks.fail_file.as_deref() == label.file_name().and_then(|s| s.to_str()) {
                return Err("Injected upload failure".into());
            }
            let mut written = Written {
                job,
                target,
                found_in,
                temp,
            };
            let mut waiting = BacklogWait {
                relay,
                started: Instant::now(),
                waited: false,
            };
            loop {
                stopped(relay, cancel)?;
                let mut backlog = relay.backlog.lock().unwrap_or_else(|e| e.into_inner());
                match finish.try_send(written) {
                    Ok(()) => {
                        *backlog += 1;
                        #[cfg(test)]
                        relay
                            .hooks
                            .max_backlog
                            .fetch_max(*backlog, Ordering::Relaxed);
                        break;
                    }
                    Err(mpsc::TrySendError::Full(item)) => {
                        written = item;
                        waiting.waited = true;
                        drop(backlog);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        return Err("NAS finishers stopped".into());
                    }
                }
            }
            drop(waiting);
            Ok::<_, String>(())
        })();
        relay
            .transfer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .in_flight[slot] = 0;
        if let Err(error) = outcome
            && !cancel.load(Ordering::Relaxed)
        {
            relay.fail(format!("{}: {error}", label.display()));
        }
    }
}

fn record(relay: &Relay, file: &copies::CopyFile, copied: bool) {
    relay
        .verified
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(send::VerifiedFile {
            relative: file.relative.clone(),
            size: file.size,
            modified: file.modified,
        });
    if copied {
        let mut timings = relay.timings.lock().unwrap_or_else(|e| e.into_inner());
        timings.uploaded_bytes += file.size;
        timings.uploaded_files += 1;
    }
}

fn companions(relay: &Relay, written: &Written, cancel: &AtomicBool) -> Result<(), String> {
    let start = Instant::now();
    let outcome = (|| {
        relay.hooks.wait(true, &relay.failed)?;
        let mut rating_written = None;
        for file in &written.job.companions {
            stopped(relay, cancel)?;
            check_destination(&relay.nas, relay.require_network)?;
            let mut target = written
                .target
                .parent()
                .unwrap()
                .join(file.relative.file_name().unwrap());
            if target
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("xmp"))
            {
                target = relay
                    .folders
                    .1
                    .path(&relay.nas.join(&written.target))?
                    .strip_prefix(&relay.nas)
                    .map_err(|e| e.to_string())?
                    .to_path_buf();
            }
            let mut file = file.clone();
            file.hash = Some(copies::hash(&file.source, file.size, cancel)?);
            relay.folders.ensure(&relay.nas, &target)?;
            let is_xmp = target
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("xmp"));
            if let Some(&(size, hash)) = written.job.card_sidecars.get(
                &file
                    .relative
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_lowercase(),
            ) && let Ok(meta) = fs::symlink_metadata(relay.nas.join(&target))
            {
                relay.folders.check(&relay.nas, &target)?;
                if meta.len() != size
                    || readback::hash(&relay.nas.join(&target), size, cancel)? != hash
                {
                    relay
                        .kept_sidecars
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(file.relative.clone(), target.clone());
                    relay
                        .warnings
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(format!(
                            "{} differs from this card's; the NAS's version was kept.",
                            target.display()
                        ));
                }
            }
            let copied = match existing(relay, &file, &target, cancel)? {
                None => {
                    let temp = write(relay, &file, &target, cancel, false, |_| {})?;
                    verify(relay, &temp, &file, cancel, false)?;
                    let copied = publish(relay, temp, &target, cancel, false)?;
                    if !copied && existing(relay, &file, &target, cancel)? == Some(false) && !is_xmp
                    {
                        relay
                            .warnings
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(format!(
                                "{} differs from this card's; the NAS's version was kept.",
                                target.display()
                            ));
                    }
                    copied
                }
                Some(_) => false,
            };
            if is_xmp && let Some((initial, local_written)) = &written.job.rated {
                if copied {
                    rating_written = Some(*local_written);
                } else {
                    match rating::write_sidecar(&relay.nas.join(&target), initial) {
                        Ok(done) => {
                            let size = fs::metadata(relay.nas.join(&target))
                                .map_err(|e| e.to_string())?
                                .len();
                            if readback::hash(&relay.nas.join(&target), size, cancel)? != done.hash
                            {
                                return Err("Verification failed for the NAS sidecar. The laptop copy was kept.".into());
                            }
                            let mut timings =
                                relay.timings.lock().unwrap_or_else(|e| e.into_inner());
                            timings.uploaded_files += 1;
                            timings.uploaded_bytes += size;
                            rating_written = Some(done);
                        }
                        Err(warning) => relay
                            .warnings
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(warning),
                    }
                }
            }
            record(relay, &file, copied);
        }
        if let (Some((initial, _)), Some(done)) = (&written.job.rated, rating_written) {
            relay
                .ratings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record(initial, done);
        }
        Ok(())
    })();
    relay
        .timings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .nas_sidecar_seconds += start.elapsed().as_secs_f64();
    outcome
}

pub(super) fn finish_files<P: FnMut(ImportProgress)>(
    relay: &Relay,
    queued: &Mutex<mpsc::Receiver<Written>>,
    cancel: &AtomicBool,
    shared: &Mutex<Shared<P>>,
) {
    loop {
        let mut written = loop {
            let mut backlog = relay.backlog.lock().unwrap_or_else(|e| e.into_inner());
            let next = queued.lock().unwrap_or_else(|e| e.into_inner()).try_recv();
            match next {
                Ok(item) => {
                    *backlog -= 1;
                    break item;
                }
                Err(mpsc::TryRecvError::Disconnected) => return,
                Err(mpsc::TryRecvError::Empty) => {
                    drop(backlog);
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        };
        if stopped(relay, cancel).is_err() {
            continue;
        }
        let outcome = (|| {
            if let Some(temp) = &written.temp {
                verify(relay, temp, &written.job.file, cancel, true)?;
            } else {
                let start = Instant::now();
                let same = existing(relay, &written.job.file, &written.target, cancel);
                relay
                    .timings
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .reading_back_seconds += start.elapsed().as_secs_f64();
                if same? != Some(true) {
                    return Err(copies::conflict(&relay.nas.join(&written.target)));
                }
            }
            // Lightroom must see the complete rated packet before a new raw name.
            companions(relay, &written, cancel)?;
            let was_written = written.temp.is_some();
            let copied = if let Some(temp) = written.temp.take() {
                let copied = publish(relay, temp, &written.target, cancel, true)?;
                if !copied
                    && existing(relay, &written.job.file, &written.target, cancel)? != Some(true)
                {
                    return Err(copies::conflict(&relay.nas.join(&written.target)));
                }
                copied
            } else {
                false
            };
            record(relay, &written.job.file, copied);
            relay.sent.lock().unwrap_or_else(|e| e.into_inner()).insert(
                send::key(&written.job.file.relative),
                send::Relayed {
                    target: written.target.clone(),
                    copied,
                    found_in: written.found_in.clone(),
                    size: written.job.file.size,
                    modified: written.job.file.modified,
                },
            );
            relay.done_files.fetch_add(1, Ordering::Relaxed);
            {
                let mut transfer = relay.transfer.lock().unwrap_or_else(|e| e.into_inner());
                transfer.verified += written.job.file.size;
                if was_written {
                    transfer.verified_written += written.job.file.size;
                }
            }
            Ok::<_, String>(())
        })();
        if let Err(error) = outcome
            && !cancel.load(Ordering::Relaxed)
        {
            relay.fail(format!("{}: {error}", written.target.display()));
        }
        report(relay, shared, &written.target);
    }
}
