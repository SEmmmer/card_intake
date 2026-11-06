//! The shutter count database, in SQLite.
//!
//! Each computer keeps its own copy in app data and syncs it with the shared one in the
//! photo share (`.card-intake/shutter.sqlite`). Every row records the write that last
//! changed it, so a sync sends and fetches only rows changed since the previous sync,
//! however long the history gets. Merging keeps each day's lowest and highest count, so
//! applying the same rows twice changes nothing and copies can be merged in any order.
//!
//! The shared database never uses WAL, which needs shared memory a network share can't
//! provide, and writes to it also hold Card Intake's lock file, because SQLite's own
//! file locks aren't reliable on every network share.

use super::{CameraHistory, DayCounts, FolderStamp, SHARED_DIR, SHARED_FILE, ShutterDb};
use chrono::{NaiveDate, NaiveDateTime};
use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The shared database's file name in `.card-intake`.
pub const SHARED_DB: &str = "shutter.sqlite";
/// The schema this build reads and writes.
const SCHEMA: i64 = 1;

const TABLES: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS cameras (
    id INTEGER PRIMARY KEY,
    model TEXT NOT NULL,
    -- Empty when the photos didn't carry it.
    serial TEXT NOT NULL,
    internal_serial TEXT,
    seq INTEGER NOT NULL,
    UNIQUE (model, serial)
);
CREATE INDEX IF NOT EXISTS cameras_seq ON cameras (seq);
CREATE TABLE IF NOT EXISTS days (
    camera INTEGER NOT NULL REFERENCES cameras (id),
    date TEXT NOT NULL,
    low INTEGER NOT NULL,
    high INTEGER NOT NULL,
    electronic_high INTEGER,
    first_shot TEXT NOT NULL,
    last_shot TEXT NOT NULL,
    firmware TEXT,
    seq INTEGER NOT NULL,
    PRIMARY KEY (camera, date)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS days_seq ON days (seq);
CREATE TABLE IF NOT EXISTS folders (
    path TEXT PRIMARY KEY,
    photos INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    modified INTEGER NOT NULL,
    seq INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS folders_seq ON folders (seq);
";

/// Widens a day's range; the WHERE clause leaves a row (and its write number) alone
/// when the new reading falls inside what's known.
const UPSERT_DAY: &str = "
INSERT INTO days (camera, date, low, high, electronic_high, first_shot, last_shot, firmware, seq)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
ON CONFLICT (camera, date) DO UPDATE SET
    firmware = CASE WHEN excluded.last_shot > last_shot
        THEN coalesce(excluded.firmware, firmware) ELSE firmware END,
    low = min(low, excluded.low),
    high = max(high, excluded.high),
    electronic_high = CASE
        WHEN electronic_high IS NULL THEN excluded.electronic_high
        WHEN excluded.electronic_high IS NULL THEN electronic_high
        ELSE max(electronic_high, excluded.electronic_high) END,
    first_shot = min(first_shot, excluded.first_shot),
    last_shot = max(last_shot, excluded.last_shot),
    seq = excluded.seq
WHERE excluded.low < low OR excluded.high > high
    OR excluded.first_shot < first_shot OR excluded.last_shot > last_shot
    OR (excluded.electronic_high IS NOT NULL
        AND (electronic_high IS NULL OR excluded.electronic_high > electronic_high))
";

const UPSERT_CAMERA: &str = "
INSERT INTO cameras (model, serial, internal_serial, seq) VALUES (?1, ?2, ?3, ?4)
ON CONFLICT (model, serial) DO UPDATE SET internal_serial = excluded.internal_serial, seq = excluded.seq
WHERE internal_serial IS NULL AND excluded.internal_serial IS NOT NULL
";

/// A folder is read again when its photos change; the newer stamp wins.
const UPSERT_FOLDER: &str = "
INSERT INTO folders (path, photos, bytes, modified, seq) VALUES (?1, ?2, ?3, ?4, ?5)
ON CONFLICT (path) DO UPDATE SET
    photos = excluded.photos, bytes = excluded.bytes, modified = excluded.modified, seq = excluded.seq
WHERE (excluded.modified, excluded.photos) > (modified, photos)
";

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens this computer's database, creating it when it doesn't exist.
    pub fn open(path: &Path) -> io::Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).map_err(|e| damaged(path, e))?;
        conn.busy_timeout(Duration::from_secs(10)).map_err(sql)?;
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
            .map_err(|e| damaged(path, e))?;
        Store::init(conn, path)
    }

    /// Opens the database in the photo share.
    fn open_shared(path: &Path) -> io::Result<Store> {
        let conn = Connection::open(path).map_err(|e| damaged(path, e))?;
        conn.busy_timeout(Duration::from_secs(30)).map_err(sql)?;
        conn.query_row("PRAGMA journal_mode = DELETE", [], |_| Ok(()))
            .map_err(|e| damaged(path, e))?;
        conn.execute_batch("PRAGMA synchronous = FULL")
            .map_err(sql)?;
        Store::init(conn, path)
    }

    /// A database that lives only in memory, for when the file can't be opened.
    pub fn in_memory() -> Store {
        let conn = Connection::open_in_memory().expect("SQLite in memory");
        Store::init(conn, Path::new(":memory:")).expect("new database")
    }

    fn init(mut conn: Connection, path: &Path) -> io::Result<Store> {
        let schema: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| damaged(path, e))?;
        if schema > SCHEMA {
            return Err(io::Error::other(format!(
                "{} was written by a newer Card Intake. Update this app to use it.",
                path.display()
            )));
        }
        if schema < SCHEMA {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql)?;
            tx.execute_batch(TABLES).map_err(sql)?;
            tx.execute(
                "INSERT OR IGNORE INTO meta (key, value) VALUES ('id', ?1)",
                [new_id()],
            )
            .map_err(sql)?;
            tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA}"))
                .map_err(sql)?;
            tx.commit().map_err(sql)?;
        }
        Ok(Store { conn })
    }

    fn meta(&self, key: &str) -> io::Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .map_err(sql)
    }

    fn meta_number(&self, key: &str) -> io::Result<i64> {
        Ok(self.meta(key)?.and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    fn set_meta(&self, key: &str, value: &str) -> io::Result<()> {
        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                [key, value],
            )
            .map(drop)
            .map_err(sql)
    }

    /// The number of the last write.
    fn seq(&self) -> io::Result<i64> {
        self.meta_number("seq")
    }

    /// Identifies this database, so a sync notices when the shared one was replaced.
    fn id(&self) -> io::Result<String> {
        self.meta("id")?
            .ok_or_else(|| io::Error::other("The shutter count database has no ID."))
    }

    pub fn is_empty(&self) -> io::Result<bool> {
        self.conn
            .query_row(
                "SELECT NOT EXISTS (SELECT 1 FROM cameras) AND NOT EXISTS (SELECT 1 FROM folders)",
                [],
                |r| r.get(0),
            )
            .map_err(sql)
    }

    /// Adds readings and folder stamps. Returns whether anything changed; a write that
    /// adds nothing new leaves the file untouched.
    pub fn add(&mut self, batch: &ShutterDb) -> io::Result<bool> {
        if batch.is_empty() {
            return Ok(false);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let seq: i64 = tx
            .query_row("SELECT value FROM meta WHERE key = 'seq'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .map_err(sql)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
            + 1;
        let mut changed = 0;
        for camera in &batch.cameras {
            // A camera recorded without its serial number joins the camera with the
            // same internal serial number.
            let serial = match (&camera.serial, &camera.internal_serial) {
                (Some(serial), _) => serial.clone(),
                (None, Some(internal)) => serial_for_internal(&tx, &camera.model, internal)
                    .map_err(sql)?
                    .unwrap_or_default(),
                (None, None) => String::new(),
            };
            changed += tx
                .execute(
                    UPSERT_CAMERA,
                    params![camera.model, serial, camera.internal_serial, seq],
                )
                .map_err(sql)?;
            let id: i64 = tx
                .query_row(
                    "SELECT id FROM cameras WHERE model = ?1 AND serial = ?2",
                    params![camera.model, serial],
                    |r| r.get(0),
                )
                .map_err(sql)?;
            for day in &camera.days {
                changed += upsert_day(&tx, id, day, seq).map_err(sql)?;
            }
        }
        for (path, stamp) in &batch.folders {
            changed += tx
                .execute(
                    UPSERT_FOLDER,
                    params![
                        path,
                        stamp.photos as i64,
                        stamp.bytes as i64,
                        stamp.modified,
                        seq
                    ],
                )
                .map_err(sql)?;
        }
        changed += fold_cameras_without_serial(&tx, seq).map_err(sql)?;
        if changed == 0 {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('seq', ?1)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [seq.to_string()],
        )
        .map_err(sql)?;
        tx.commit().map_err(sql)?;
        Ok(true)
    }

    /// The whole history.
    pub fn load(&self) -> io::Result<ShutterDb> {
        changes_since(&self.conn, 0).map_err(sql)
    }

    /// The photo folders already read, by path.
    pub fn folders(&self) -> io::Result<BTreeMap<String, FolderStamp>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, photos, bytes, modified FROM folders")
            .map_err(sql)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, folder_stamp(r, 1)?)))
            .map_err(sql)?;
        rows.collect::<Result<_, _>>().map_err(sql)
    }

    pub fn folder_count(&self) -> io::Result<usize> {
        self.conn
            .query_row("SELECT count(*) FROM folders", [], |r| r.get::<_, i64>(0))
            .map(|n| n as usize)
            .map_err(sql)
    }

    /// Merges a JSON database written by an older Card Intake (`shutter-counts.json`),
    /// and again whenever that file changes, so computers that haven't been updated yet
    /// don't lose what they add. A damaged file is left alone.
    pub fn import_json(&mut self, path: &Path) -> io::Result<bool> {
        let Ok(meta) = std::fs::metadata(path) else {
            return Ok(false);
        };
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        let stamp = format!("{}:{modified}", meta.len());
        let key = format!("json:{}", path.display());
        if self.meta(&key)?.as_deref() == Some(stamp.as_str()) {
            return Ok(false);
        }
        let Ok(old) = ShutterDb::load_json(path) else {
            return Ok(false);
        };
        let changed = self.add(&old)?;
        self.set_meta(&key, &stamp)?;
        Ok(changed)
    }
}

fn serial_for_internal(
    conn: &Connection,
    model: &str,
    internal: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT serial FROM cameras
         WHERE model = ?1 AND internal_serial = ?2 AND serial <> ''
         ORDER BY serial LIMIT 1",
        params![model, internal],
        |r| r.get(0),
    )
    .optional()
}

fn upsert_day(
    conn: &Connection,
    camera: i64,
    day: &DayCounts,
    seq: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        UPSERT_DAY,
        params![
            camera,
            day.date.format("%Y-%m-%d").to_string(),
            day.min as i64,
            day.max as i64,
            day.electronic_max.map(|n| n as i64),
            shot(day.first_shot),
            shot(day.last_shot),
            day.firmware,
            seq
        ],
    )
}

/// Moves the days of a camera recorded without its serial number into the camera with
/// the same model and internal serial number, once that one is known.
fn fold_cameras_without_serial(conn: &Connection, seq: i64) -> rusqlite::Result<usize> {
    let pairs: Vec<(i64, i64)> = conn
        .prepare(
            "SELECT o.id, t.id FROM cameras o JOIN cameras t
               ON t.id = (SELECT id FROM cameras
                          WHERE model = o.model AND internal_serial = o.internal_serial
                            AND serial <> '' ORDER BY serial LIMIT 1)
             WHERE o.serial = '' AND o.internal_serial IS NOT NULL",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let mut changed = 0;
    for (orphan, target) in pairs {
        let days = days_of(conn, "WHERE camera = ?1", orphan)?;
        for day in days.into_iter().map(|(_, day)| day) {
            changed += upsert_day(conn, target, &day, seq)?;
        }
        conn.execute("DELETE FROM days WHERE camera = ?1", [orphan])?;
        changed += conn.execute("DELETE FROM cameras WHERE id = ?1", [orphan])?;
    }
    Ok(changed)
}

fn days_of(conn: &Connection, filter: &str, value: i64) -> rusqlite::Result<Vec<(i64, DayCounts)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT camera, date, low, high, electronic_high, first_shot, last_shot, firmware
         FROM days {filter} ORDER BY camera, date"
    ))?;
    let rows = stmt.query_map([value], |r| {
        Ok((
            r.get(0)?,
            DayCounts {
                date: parse(r, 1, |s| NaiveDate::parse_from_str(s, "%Y-%m-%d"))?,
                min: r.get::<_, i64>(2)? as u64,
                max: r.get::<_, i64>(3)? as u64,
                electronic_max: r.get::<_, Option<i64>>(4)?.map(|n| n as u64),
                first_shot: parse(r, 5, parse_shot)?,
                last_shot: parse(r, 6, parse_shot)?,
                firmware: r.get(7)?,
            },
        ))
    })?;
    rows.collect()
}

/// Rows written after write number `since`, as a database that can be merged elsewhere.
fn changes_since(conn: &Connection, since: i64) -> rusqlite::Result<ShutterDb> {
    let mut cameras: BTreeMap<i64, CameraHistory> = BTreeMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, model, serial, internal_serial FROM cameras
             WHERE seq > ?1 OR id IN (SELECT camera FROM days WHERE seq > ?1)",
        )?;
        let rows = stmt.query_map([since], |r| {
            let serial: String = r.get(2)?;
            Ok((
                r.get::<_, i64>(0)?,
                CameraHistory {
                    model: r.get(1)?,
                    serial: (!serial.is_empty()).then_some(serial),
                    internal_serial: r.get(3)?,
                    days: Vec::new(),
                },
            ))
        })?;
        for row in rows {
            let (id, camera) = row?;
            cameras.insert(id, camera);
        }
    }
    for (camera, day) in days_of(conn, "WHERE seq > ?1", since)? {
        if let Some(c) = cameras.get_mut(&camera) {
            c.days.push(day);
        }
    }
    let mut cameras: Vec<CameraHistory> = cameras.into_values().collect();
    cameras.sort_by(|a, b| (&a.model, &a.serial).cmp(&(&b.model, &b.serial)));
    let mut stmt =
        conn.prepare("SELECT path, photos, bytes, modified FROM folders WHERE seq > ?1")?;
    let folders = stmt
        .query_map([since], |r| Ok((r.get(0)?, folder_stamp(r, 1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(ShutterDb {
        version: super::VERSION,
        cameras,
        folders,
    })
}

fn folder_stamp(r: &rusqlite::Row, at: usize) -> rusqlite::Result<FolderStamp> {
    Ok(FolderStamp {
        photos: r.get::<_, i64>(at)? as u64,
        bytes: r.get::<_, i64>(at + 1)? as u64,
        modified: r.get(at + 2)?,
    })
}

/// Shot times are stored as ISO text, which sorts in time order.
fn shot(t: NaiveDateTime) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.f").to_string()
}

fn parse_shot(s: &str) -> chrono::ParseResult<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
}

fn parse<T>(
    r: &rusqlite::Row,
    at: usize,
    f: impl Fn(&str) -> chrono::ParseResult<T>,
) -> rusqlite::Result<T> {
    let text: String = r.get(at)?;
    f(&text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(at, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn sql(e: rusqlite::Error) -> io::Error {
    io::Error::other(e)
}

fn damaged(path: &Path, e: rusqlite::Error) -> io::Error {
    match e.sqlite_error_code() {
        Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt) => io::Error::other(format!(
            "{} is damaged ({e}). Move it aside to start a new one.",
            path.display()
        )),
        _ => io::Error::other(format!("Couldn't open {} ({e}).", path.display())),
    }
}

fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seed = format!("{nanos}-{}-{:p}", std::process::id(), &nanos);
    format!("{:016x}", xxhash_rust::xxh64::xxh64(seed.as_bytes(), 0))
}

/// The shared database's path in the photo share.
pub fn shared_path(nas_root: &Path) -> PathBuf {
    nas_root.join(SHARED_DIR).join(SHARED_DB)
}

/// Sends this computer's new readings to the shared database and brings in what other
/// computers added since the last sync. Only rows changed since then are read or
/// written.
pub fn sync_shared(nas_root: &Path, local: &mut Store) -> io::Result<()> {
    let dir = nas_root.join(SHARED_DIR);
    local.import_json(&dir.join(SHARED_FILE))?;
    let path = dir.join(SHARED_DB);
    let lock = dir.join("shutter.lock");
    if !path.exists() {
        if local.is_empty()? {
            return Ok(());
        }
        let _lock = Lock::acquire(&lock)?;
        Store::open_shared(&path)?;
    }
    let mut shared = Store::open_shared(&path)?;
    let id = shared.id()?;
    let (pushed_key, pulled_key) = (format!("pushed:{id}"), format!("pulled:{id}"));

    let outgoing = changes_since(&local.conn, local.meta_number(&pushed_key)?).map_err(sql)?;
    if !outgoing.is_empty() {
        let _lock = Lock::acquire(&lock)?;
        shared.add(&outgoing)?;
    }
    // Read the write number and the rows in one transaction, so a write from another
    // computer in between is fetched next time rather than missed.
    let (incoming, shared_seq) = {
        let tx = shared.conn.transaction().map_err(sql)?;
        let seq: i64 = tx
            .query_row("SELECT value FROM meta WHERE key = 'seq'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .map_err(sql)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let rows = changes_since(&tx, local.meta_number(&pulled_key)?).map_err(sql)?;
        tx.commit().map_err(sql)?;
        (rows, seq)
    };
    local.add(&incoming)?;
    // Everything in the local copy is now in the shared one as well.
    local.set_meta(&pushed_key, &local.seq()?.to_string())?;
    local.set_meta(&pulled_key, &shared_seq.to_string())?;
    Ok(())
}

/// A lock file; removed when dropped. A lock older than a minute is from a computer
/// that crashed or lost the share, and is taken over.
pub(crate) struct Lock(PathBuf);

impl Lock {
    pub(crate) fn acquire(path: &Path) -> io::Result<Lock> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match std::fs::File::create_new(path) {
                Ok(_) => return Ok(Lock(path.to_path_buf())),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok())
                        .is_some_and(|age| age > Duration::from_secs(60));
                    if stale {
                        let _ = std::fs::remove_file(path);
                        continue;
                    }
                    if std::time::Instant::now() > deadline {
                        return Err(io::Error::other(
                            "Another computer is updating the shutter count database on the NAS. Try again in a minute.",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exiftool::ShutterSample;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    fn sample(count: u64, taken: &str) -> ShutterSample {
        ShutterSample {
            model: "ILCE-7RM6".into(),
            serial: Some("03506694".into()),
            internal_serial: Some("73 255 0 0 25 9".into()),
            firmware: Some("1.01".into()),
            count,
            electronic_count: None,
            taken: at(taken),
        }
    }

    fn batch(samples: &[ShutterSample]) -> ShutterDb {
        let mut db = ShutterDb::default();
        for s in samples {
            db.add(s);
        }
        db
    }

    #[test]
    fn stores_the_same_history_as_merging_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shutter.sqlite");
        let mut store = Store::open(&path).unwrap();
        let first = batch(&[
            sample(700, "2026-11-06 19:00"),
            sample(650, "2026-11-06 17:00"),
            ShutterSample {
                electronic_count: Some(1234),
                firmware: Some("1.02".into()),
                ..sample(894, "2026-11-07 10:00")
            },
        ]);
        let mut second = batch(&[
            sample(600, "2026-11-06 12:00"),
            ShutterSample {
                model: "ILCE-1".into(),
                ..sample(5, "2026-10-01 09:00")
            },
        ]);
        second.folders.insert(
            "2026/2026-11-06_夜景".into(),
            FolderStamp {
                photos: 2,
                bytes: 10,
                modified: 200,
            },
        );
        assert!(store.add(&first).unwrap());
        assert!(store.add(&second).unwrap());
        // Adding the same readings again changes nothing.
        assert!(!store.add(&first).unwrap());
        drop(store);

        let mut expected = first.clone();
        expected.merge(&second);
        let store = Store::open(&path).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.cameras, expected.cameras);
        assert_eq!(loaded.folders, expected.folders);
        assert_eq!(store.folder_count().unwrap(), 1);
        let views = loaded.views();
        assert_eq!(
            (views[0].count, views[0].electronic_count),
            (894, Some(1234))
        );
        assert_eq!(views[0].firmware.as_deref(), Some("1.02"));
    }

    #[test]
    fn photos_without_a_serial_join_their_camera_by_internal_serial() {
        let no_serial = |count, taken| ShutterSample {
            serial: None,
            ..sample(count, taken)
        };
        let mut store = Store::in_memory();
        store
            .add(&batch(&[no_serial(2, "2026-10-30 17:54")]))
            .unwrap();
        assert_eq!(store.load().unwrap().cameras.len(), 1);
        store
            .add(&batch(&[sample(30, "2026-10-30 23:03")]))
            .unwrap();
        store
            .add(&batch(&[no_serial(18, "2026-10-30 20:47")]))
            .unwrap();
        let cameras = store.load().unwrap().cameras;
        assert_eq!(cameras.len(), 1, "{cameras:?}");
        assert_eq!(cameras[0].serial.as_deref(), Some("03506694"));
        assert_eq!((cameras[0].days[0].min, cameras[0].days[0].max), (2, 30));
    }

    #[test]
    fn computers_sync_through_the_share_reading_only_new_rows() {
        let nas = tempfile::tempdir().unwrap();
        let mut pc = Store::in_memory();
        let mut mac = Store::in_memory();
        // Nothing to share and no shared database: nothing is created.
        sync_shared(nas.path(), &mut pc).unwrap();
        assert!(!shared_path(nas.path()).exists());

        pc.add(&batch(&[sample(700, "2026-11-06 19:00")])).unwrap();
        sync_shared(nas.path(), &mut pc).unwrap();
        mac.add(&batch(&[sample(894, "2026-11-07 10:00")])).unwrap();
        sync_shared(nas.path(), &mut mac).unwrap();
        sync_shared(nas.path(), &mut pc).unwrap();
        for store in [&pc, &mac] {
            let days = &store.load().unwrap().cameras[0].days;
            assert_eq!(days.iter().map(|d| d.max).collect::<Vec<_>>(), [700, 894]);
        }

        // A sync with nothing new sends nothing and leaves the shared file alone.
        let path = shared_path(nas.path());
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let id = Store::open_shared(&path).unwrap().id().unwrap();
        let pushed = pc.meta_number(&format!("pushed:{id}")).unwrap();
        assert!(changes_since(&pc.conn, pushed).unwrap().is_empty());
        sync_shared(nas.path(), &mut pc).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
        assert!(!nas.path().join(SHARED_DIR).join("shutter.lock").exists());

        // A replaced shared database gets everything again.
        std::fs::remove_file(&path).unwrap();
        sync_shared(nas.path(), &mut mac).unwrap();
        let fresh = Store::open_shared(&path).unwrap().load().unwrap();
        assert_eq!(fresh.cameras[0].days.len(), 2);
    }

    #[test]
    fn brings_in_the_json_file_older_builds_write() {
        let nas = tempfile::tempdir().unwrap();
        let json = nas.path().join(SHARED_DIR).join(SHARED_FILE);
        std::fs::create_dir_all(json.parent().unwrap()).unwrap();
        let old = batch(&[sample(31, "2026-10-31 11:44")]);
        std::fs::write(&json, serde_json::to_vec(&old).unwrap()).unwrap();
        let mut pc = Store::in_memory();
        sync_shared(nas.path(), &mut pc).unwrap();
        assert_eq!(pc.load().unwrap().cameras[0].days[0].min, 31);
        let shared = Store::open_shared(&shared_path(nas.path())).unwrap();
        assert_eq!(shared.load().unwrap().cameras[0].days[0].min, 31);
        // Read again only when it changes.
        assert!(!pc.import_json(&json).unwrap());
    }

    #[test]
    fn refuses_a_damaged_or_newer_shared_database() {
        let nas = tempfile::tempdir().unwrap();
        let path = shared_path(nas.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![7u8; 4096]).unwrap();
        let mut pc = Store::in_memory();
        pc.add(&batch(&[sample(700, "2026-11-06 19:00")])).unwrap();
        let err = sync_shared(nas.path(), &mut pc).unwrap_err().to_string();
        assert!(err.contains("damaged"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), vec![7u8; 4096]);

        std::fs::remove_file(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA user_version = 99").unwrap();
        drop(conn);
        let err = sync_shared(nas.path(), &mut pc).unwrap_err().to_string();
        assert!(err.contains("newer"), "{err}");
    }

    #[test]
    fn a_stale_lock_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("x.lock");
        std::fs::write(&lock, b"").unwrap();
        let old = SystemTime::now() - Duration::from_secs(120);
        std::fs::File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let held = Lock::acquire(&lock).unwrap();
        drop(held);
        assert!(!lock.exists());
    }
}
