//! Publish each batch independently; a copy waits only for its own settings.
use super::*;
use std::sync::Condvar;
#[derive(Default)]
pub(super) struct Metadata {
    ready: Mutex<HashMap<PathBuf, Option<rating::Settings>>>,
    changed: Condvar,
}
impl Metadata {
    pub fn read(
        &self,
        tool: Option<&Path>,
        sources: &[PathBuf],
        cancel: &AtomicBool,
    ) -> (rating::Readings, f64) {
        let start = Instant::now();
        let readings = rating::read_settings(tool, sources, cancel, |paths, settings| {
            let mut ready = self.ready.lock().unwrap_or_else(|e| e.into_inner());
            for path in paths {
                ready.insert(path.clone(), settings.get(path).cloned());
            }
            self.changed.notify_all();
        });
        (readings, start.elapsed().as_secs_f64())
    }
    pub fn get(&self, source: &Path, stop: &Stop) -> Option<rating::Settings> {
        let mut ready = self.ready.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(settings) = ready.get(source) {
                return settings.clone();
            }
            if stop.check().is_err() {
                return None;
            }
            ready = self
                .changed
                .wait_timeout(ready, Duration::from_millis(20))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}
