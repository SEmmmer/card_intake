//! Finished JPEGs, chosen by their metadata, delivered into a folder of finished photos
//! (the user's Immich external library), e.g. `MyPhoto\2026-11-06_夜景`.
//!
//! Copies are made and checked in a hidden folder inside the shoot, then moved into the
//! finished photos folder: the whole folder in one rename when it's new, otherwise file
//! by file. Nothing already there is ever replaced or removed. Catalogs are only ever
//! opened after copying them (and their WAL) to system temp.

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Kept in the shoot folder: which names Card Intake gave which photos, and when.
pub const MANIFEST: &str = ".card-intake-delivered.json";
/// Hidden folder inside the shoot where copies are made and checked.
pub const STAGING: &str = ".card-intake-delivering";
/// Hidden temporary name, used only when a move between folders isn't possible.
const TEMP: &str = ".card-intake-tmp-";
const TAGS: &[&str] = &[
    "-G1",
    "-IFD0:Software",
    "-XMP-xmp:CreatorTool",
    "-XMP-xmpMM:DocumentID",
    "-XMP-xmpMM:PreservedFileName",
    "-Make",
    "-Model",
    "-XMP-xmp:Rating",
    "-IFD0:Rating",
    "-IFD0:RatingPercent",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    PixCake,
    Lightroom,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StarsFrom {
    Catalog,
    Sidecar,
    Dng,
    #[default]
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Photo {
    /// The file name in the finished photos folder.
    pub name: String,
    /// Relative to the shoot folder.
    pub source: String,
    pub kind: Kind,
    pub stars: Option<i32>,
    pub stars_from: StarsFrom,
    /// `new` (will be copied), `exists` (that name is already in the folder and is left
    /// alone), `delivered` or `failed`.
    pub action: String,
    /// A `data:image/jpeg;base64,` URL of the JPEG's embedded thumbnail, for confirming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    #[serde(skip)]
    identities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skipped {
    pub source: String,
    pub reason: String,
}

/// A photo the user confirmed, as the plan showed it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Confirmed {
    pub name: String,
    pub source: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DeliveryResult {
    pub shoot: PathBuf,
    /// The shoot's folder among the finished photos, e.g. `MyPhoto\2026-11-06_夜景`.
    pub target: PathBuf,
    /// Whether that folder was already there when planning.
    pub target_exists: bool,
    pub plan_only: bool,
    /// Photos to copy (plan) or left to copy (after a failure).
    pub new: usize,
    /// Photos whose name is already in the folder; they are left alone.
    pub existing: usize,
    /// Photos copied by this delivery.
    pub delivered: usize,
    pub photos: Vec<Photo>,
    pub skipped: Vec<Skipped>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl DeliveryResult {
    pub fn confirmed(&self) -> Vec<Confirmed> {
        let mut list: Vec<Confirmed> = self
            .photos
            .iter()
            .filter(|p| p.action == "new")
            .map(|p| Confirmed {
                name: p.name.clone(),
                source: p.source.clone(),
            })
            .collect();
        list.sort();
        list
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Assignment {
    name: String,
    identities: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    /// The folder name among the finished photos.
    target: Option<String>,
    last_delivered: Option<DateTime<Utc>>,
    // Keep assignments, even after a source disappears, so names don't shuffle.
    assignments: Vec<Assignment>,
}

#[derive(Debug, Serialize)]
pub struct DeliveryState {
    /// Photos Card Intake has delivered from this shoot.
    pub count: usize,
    pub last_delivered: Option<DateTime<Utc>>,
}

pub fn state(shoot: &Path) -> io::Result<DeliveryState> {
    let m = manifest(shoot)?;
    Ok(DeliveryState {
        count: m.assignments.len(),
        last_delivered: m.last_delivered,
    })
}

fn simple_name(s: &str) -> bool {
    !s.is_empty() && !s.contains(['/', '\\', ':', '\r', '\n']) && s != "." && s != ".."
}

fn manifest(shoot: &Path) -> io::Result<Manifest> {
    let path = shoot.join(MANIFEST);
    let m: Manifest = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Manifest::default()),
        Err(e) => return Err(e),
    };
    if m.version != 2
        || m.assignments.iter().any(|a| {
            !simple_name(&a.name)
                || a.name.starts_with('.')
                || !a.name.to_lowercase().ends_with(".jpg")
        })
    {
        return Err(io::Error::other(format!(
            "{} is damaged or from a newer version. Nothing was delivered.",
            path.display()
        )));
    }
    Ok(m)
}

/// Normalise Lightroom's `//server/share/` paths and Windows UNC paths alike.
pub fn normal_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches("//?/")
        .trim_end_matches('/')
        .to_lowercase()
}

pub fn pair_stem(stem: &str) -> String {
    if let Some((base, n)) = stem.strip_suffix(')').and_then(|s| s.rsplit_once("_("))
        && !n.is_empty()
        && n.bytes().all(|c| c.is_ascii_digit())
    {
        return base.to_string();
    }
    stem.to_string()
}

fn clean_stem(stem: &str) -> String {
    stem.to_ascii_lowercase()
        .find("-dxo_")
        .map_or_else(|| stem.to_string(), |i| stem[..i].to_string())
}

fn excluded(entry: &walkdir::DirEntry) -> bool {
    let n = entry.file_name().to_string_lossy().to_lowercase();
    if n.starts_with('.') {
        return true;
    }
    if !entry.file_type().is_dir() {
        return false;
    }
    // `_deliver` holds copies from before Deliver used the finished photos folder.
    if matches!(
        n.as_str(),
        "_catalog" | "_deliver" | "video" | "@eadir" | "#recycle"
    ) || n.ends_with(".lrdata")
    {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if entry.metadata().is_ok_and(|m| m.file_attributes() & 6 != 0) {
            return true;
        }
    }
    false
}

#[derive(Debug)]
struct Candidate {
    relative: String,
    stem: String,
    meta: Value,
    kind: Kind,
    modified_ns: u128,
    identities: Vec<String>,
}

// PixCake pads its blank CreatorTool with spaces in real exports. Preserve the
// distinction between absent and present-but-blank after trimming that padding.
fn tag<'a>(meta: &'a Value, key: &str) -> Option<&'a str> {
    meta.get(key)?.as_str().map(str::trim)
}
fn rating(meta: &Value) -> Option<i32> {
    meta.get("XMP-xmp:Rating")
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
        .filter(|n| (0..=5).contains(n))
        .map(|n| n as i32)
}

fn classify(meta: &Value) -> Result<Kind, &'static str> {
    let software = tag(meta, "IFD0:Software");
    let creator = tag(meta, "XMP-xmp:CreatorTool");
    let tools = [software, creator]
        .into_iter()
        .flatten()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    if tools
        .iter()
        .any(|s| s.contains("pixcake") || s.contains("像素蛋糕"))
        || (creator == Some("") && software.is_none_or(str::is_empty))
    {
        return Ok(Kind::PixCake);
    }
    if tools.iter().any(|s| s.contains("lightroom")) {
        return Ok(Kind::Lightroom);
    }
    if creator.is_none()
        && ((tag(meta, "IFD0:Make").is_some() && tag(meta, "IFD0:Model").is_some())
            || software.is_some_and(|s| {
                let s = s.to_lowercase();
                s.starts_with("ilce-") || s.starts_with("ver.") || s.starts_with("dsc-")
            }))
    {
        return Err("camera JPEG");
    }
    Err("unrecognised")
}

fn metadata(exiftool: &Path, paths: &[PathBuf]) -> io::Result<BTreeMap<String, Value>> {
    let mut found = BTreeMap::new();
    for chunk in paths.chunks(128) {
        let bytes = crate::exiftool::read_batch(exiftool, chunk, TAGS)?;
        let rows: Vec<Value> = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        for row in rows {
            if let Some(e) = row.get("ExifTool:Error").or_else(|| row.get("Error")) {
                return Err(io::Error::other(format!(
                    "Couldn't read JPEG metadata: {e}. Existing deliveries were left alone."
                )));
            }
            let path = tag(&row, "SourceFile")
                .ok_or_else(|| io::Error::other("ExifTool returned no source path"))?;
            found.insert(normal_path(path), row);
        }
    }
    Ok(found)
}

#[derive(Debug)]
struct CatalogPhoto {
    base: String,
    extension: String,
    rating: i32,
}

fn catalog_ratings(catalog: &Path, shoot: &Path) -> Result<Vec<CatalogPhoto>, String> {
    // Never let SQLite open the user's catalog, even read-only: it may create SHM.
    let temp = tempfile::Builder::new()
        .prefix("card-intake-catalog-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    let copy = temp.path().join("catalog.lrcat");
    let wal = PathBuf::from(format!("{}-wal", catalog.display()));
    let stamp = |p: &Path| -> io::Result<Option<(u64, std::time::SystemTime)>> {
        match fs::metadata(p) {
            Ok(m) => Ok(Some((m.len(), m.modified()?))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    };
    let before = (
        stamp(catalog).map_err(|e| e.to_string())?,
        stamp(&wal).map_err(|e| e.to_string())?,
    );
    fs::copy(catalog, &copy).map_err(|e| e.to_string())?;
    match fs::copy(&wal, temp.path().join("catalog.lrcat-wal")) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    let after = (
        stamp(catalog).map_err(|e| e.to_string())?,
        stamp(&wal).map_err(|e| e.to_string())?,
    );
    if before != after {
        return Err("Lightroom changed the catalog while it was being copied. Try Deliver again when it has finished saving.".into());
    }
    let db = Connection::open(&copy).map_err(|e| e.to_string())?;
    let check: String = db
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if check != "ok" {
        return Err(check);
    }
    let mut q = db
        .prepare(
            "SELECT f.baseName, f.extension, COALESCE(i.rating, 0), r.absolutePath, d.pathFromRoot
        FROM Adobe_images i JOIN AgLibraryFile f ON i.rootFile=f.id_local
        JOIN AgLibraryFolder d ON f.folder=d.id_local
        JOIN AgLibraryRootFolder r ON d.rootFolder=r.id_local WHERE i.masterImage IS NULL",
        )
        .map_err(|e| e.to_string())?;
    let rows = q
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut photos = Vec::new();
    for row in rows {
        let (base, extension, stars, root, folder) = row.map_err(|e| e.to_string())?;
        let path = format!(
            "{}/{}",
            root.trim_end_matches(['/', '\\']),
            folder.trim_start_matches(['/', '\\'])
        );
        if normal_path(&path) == normal_path(&shoot.to_string_lossy()) {
            photos.push(CatalogPhoto {
                base,
                extension,
                rating: (stars as i32).clamp(0, 5),
            });
        }
    }
    Ok(photos)
}

fn stars(
    c: &Candidate,
    catalog: &[CatalogPhoto],
    fallback: &BTreeMap<String, (i32, StarsFrom)>,
) -> (Option<i32>, StarsFrom) {
    let prefer_dng = c.stem.to_lowercase().contains("-dxo_");
    let mut matches: Vec<_> = catalog
        .iter()
        .filter(|p| p.base.eq_ignore_ascii_case(&c.stem))
        .collect();
    matches.sort_by_key(|p| {
        let ext = p.extension.to_lowercase();
        if prefer_dng {
            usize::from(ext != "dng")
        } else {
            usize::from(!matches!(
                ext.as_str(),
                "arw" | "nef" | "nrw" | "cr2" | "cr3" | "raf" | "orf" | "rw2"
            ))
        }
    });
    let photo = matches.first().copied().or_else(|| {
        let name = tag(&c.meta, "XMP-xmpMM:PreservedFileName")?;
        catalog
            .iter()
            .find(|p| format!("{}.{}", p.base, p.extension).eq_ignore_ascii_case(name))
    });
    if let Some(p) = photo {
        return (Some(p.rating), StarsFrom::Catalog);
    }
    let preserved = tag(&c.meta, "XMP-xmpMM:PreservedFileName")
        .and_then(|s| Path::new(s).file_stem())
        .map(|s| s.to_string_lossy().to_lowercase());
    // A raw's sidecar comes before the DNG fallback, as it does at import.
    let raw = preserved.unwrap_or_else(|| clean_stem(&c.stem).to_lowercase());
    if let Some((n, from)) = fallback
        .get(&format!("xmp:{raw}"))
        .or_else(|| fallback.get(&format!("dng:{}", c.stem.to_lowercase())))
    {
        return (Some(*n), *from);
    }
    (rating(&c.meta), StarsFrom::None)
}

fn overlap(a: &[String], b: &[String]) -> bool {
    a.iter().any(|k| b.contains(k))
}

/// A read-only plan for delivering `shoot` into `target`, its folder among the finished
/// photos. ExifTool argument files and catalog copies live in system temp.
pub fn plan(
    shoot: &Path,
    exiftool: &Path,
    catalog: Option<&Path>,
    target: &Path,
) -> io::Result<DeliveryResult> {
    if !shoot.is_dir() {
        return Err(io::Error::other(
            "The shoot folder is unavailable. Reconnect the photo share.",
        ));
    }
    if target.starts_with(shoot) || shoot.starts_with(target) {
        return Err(io::Error::other(
            "The finished photos folder must be outside the shoot folder.",
        ));
    }
    let target_exists = match fs::symlink_metadata(target) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => true,
        Ok(_) => {
            return Err(io::Error::other(format!(
                "{} isn't a regular folder. Nothing was delivered.",
                target.display()
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(e),
    };
    if !target_exists && !target.parent().is_some_and(Path::is_dir) {
        return Err(io::Error::other(format!(
            "The finished photos folder {} is unavailable. Reconnect the NAS or check the Deliver settings.",
            target.parent().unwrap_or(target).display()
        )));
    }
    let old = manifest(shoot)?;
    let mut result = DeliveryResult {
        shoot: shoot.to_path_buf(),
        target: target.to_path_buf(),
        target_exists,
        plan_only: true,
        ..Default::default()
    };
    let mut jpegs = Vec::new();
    let mut fallbacks = Vec::new();
    for entry in walkdir::WalkDir::new(shoot)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !excluded(e))
    {
        let entry = entry.map_err(io::Error::other)?;
        if !entry.file_type().is_file() {
            continue;
        }
        match entry
            .path()
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "jpg" | "jpeg" => jpegs.push(entry.into_path()),
            "xmp" | "dng" => fallbacks.push(entry.into_path()),
            _ => {}
        }
    }
    jpegs.sort();
    fallbacks.sort();
    let metas = metadata(exiftool, &jpegs)?;
    let mut candidates = Vec::new();
    for path in jpegs {
        let relative = path
            .strip_prefix(shoot)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let meta = metas
            .get(&normal_path(&path.to_string_lossy()))
            .ok_or_else(|| {
                io::Error::other(format!(
                    "No metadata returned for {}. Existing deliveries were left alone.",
                    path.display()
                ))
            })?
            .clone();
        let kind = match classify(&meta) {
            Ok(kind) => kind,
            Err(reason) => {
                result.skipped.push(Skipped {
                    source: relative,
                    reason: reason.into(),
                });
                continue;
            }
        };
        let stem = pair_stem(&path.file_stem().unwrap().to_string_lossy());
        let mut identities = vec![format!("stem:{}", stem.to_lowercase())];
        if let Some(id) = tag(&meta, "XMP-xmpMM:DocumentID").filter(|id| !id.is_empty()) {
            identities.push(format!("doc:{}", id.to_lowercase()));
        }
        let stat = fs::metadata(&path)?;
        candidates.push(Candidate {
            relative,
            stem,
            kind,
            meta,
            modified_ns: stat
                .modified()?
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            identities,
        });
    }
    // Group by either preserved document identity or the pair stem. Merge groups
    // transitively, so an older export can bridge a renamed PixCake version.
    let mut groups: Vec<Vec<Candidate>> = Vec::new();
    for c in candidates {
        let mut group = vec![c];
        let mut i = 0;
        while i < groups.len() {
            if groups[i]
                .iter()
                .any(|a| group.iter().any(|b| overlap(&a.identities, &b.identities)))
            {
                group.append(&mut groups.remove(i));
                i = 0;
            } else {
                i += 1;
            }
        }
        groups.push(group);
    }
    let mut chosen = Vec::new();
    for mut group in groups {
        group.sort_by(|a, b| {
            (b.kind == Kind::PixCake)
                .cmp(&(a.kind == Kind::PixCake))
                .then(b.modified_ns.cmp(&a.modified_ns))
                .then(a.relative.cmp(&b.relative))
        });
        let mut winner = group.remove(0);
        for older in group {
            let reason = if winner.kind == Kind::PixCake && older.kind == Kind::Lightroom {
                "replaced by PixCake"
            } else {
                "older version"
            };
            result.skipped.push(Skipped {
                source: older.relative,
                reason: reason.into(),
            });
            winner.identities.extend(older.identities);
        }
        winner.identities.sort();
        winner.identities.dedup();
        chosen.push(winner);
    }
    chosen.sort_by(|a, b| {
        a.identities
            .cmp(&b.identities)
            .then(a.relative.cmp(&b.relative))
    });
    let ratings = match catalog.map(|p| catalog_ratings(p, shoot)) {
        Some(Ok(r)) => r,
        Some(Err(e)) => {
            result.warnings.push(format!("Couldn't read a temporary copy of the Lightroom catalog: {e}. Using exported ratings or sidecars instead."));
            Vec::new()
        }
        None => Vec::new(),
    };
    let mut fallback = BTreeMap::new();
    // A malformed fallback never prevents a finished JPEG being delivered.
    for chunk in fallbacks.chunks(128) {
        match metadata(exiftool, chunk) {
            Ok(meta) => {
                for path in chunk {
                    let Some(tags) = meta.get(&normal_path(&path.to_string_lossy())) else {
                        continue;
                    };
                    let is_xmp = path.extension().unwrap().eq_ignore_ascii_case("xmp");
                    let dxo = [tag(tags, "IFD0:Software"), tag(tags, "XMP-xmp:CreatorTool")]
                        .into_iter()
                        .flatten()
                        .any(|s| s.to_ascii_lowercase().contains("dxo"))
                        || path
                            .file_stem()
                            .unwrap()
                            .to_string_lossy()
                            .to_ascii_lowercase()
                            .contains("dxo");
                    if !is_xmp && !dxo {
                        continue;
                    }
                    if let Some(n) = rating(tags) {
                        let key = format!(
                            "{}:{}",
                            if is_xmp { "xmp" } else { "dng" },
                            path.file_stem().unwrap().to_string_lossy().to_lowercase()
                        );
                        fallback.entry(key).or_insert((
                            n,
                            if is_xmp {
                                StarsFrom::Sidecar
                            } else {
                                StarsFrom::Dng
                            },
                        ));
                    }
                }
            }
            Err(e) => result
                .warnings
                .push(format!("Couldn't read fallback stars: {e}")),
        }
    }
    // Names already in the folder, whoever put them there, are never overwritten.
    let mut present = BTreeSet::new();
    if target_exists {
        for e in fs::read_dir(target)? {
            present.insert(e?.file_name().to_string_lossy().to_lowercase());
        }
    }
    let mut taken = BTreeSet::new();
    for c in chosen {
        // A photo keeps the name it was delivered under, even if that file was deleted.
        let assigned = old
            .assignments
            .iter()
            .find(|a| overlap(&a.identities, &c.identities))
            .map(|a| a.name.clone())
            .filter(|n| !taken.contains(&n.to_lowercase()));
        let name = assigned.unwrap_or_else(|| {
            let base = clean_stem(&c.stem);
            let base = if base.trim().is_empty() {
                c.stem.clone()
            } else {
                base
            };
            // Skip names given to other photos earlier, or to another photo now.
            (1..)
                .map(|n| {
                    if n == 1 {
                        format!("{base}.jpg")
                    } else {
                        format!("{base}-{n}.jpg")
                    }
                })
                .find(|n| {
                    !taken.contains(&n.to_lowercase())
                        && !old
                            .assignments
                            .iter()
                            .any(|a| a.name.eq_ignore_ascii_case(n))
                })
                .expect("an unused name")
        });
        taken.insert(name.to_lowercase());
        let (stars, stars_from) = stars(&c, &ratings, &fallback);
        let action = if present.contains(&name.to_lowercase()) {
            result.existing += 1;
            "exists"
        } else {
            result.new += 1;
            "new"
        };
        result.photos.push(Photo {
            name,
            source: c.relative,
            kind: c.kind,
            stars,
            stars_from,
            action: action.into(),
            thumbnail: None,
            identities: c.identities,
        });
    }
    result
        .photos
        .sort_by_key(|p| (p.name.to_lowercase(), p.source.clone()));
    result.skipped.sort_by(|a, b| a.source.cmp(&b.source));
    Ok(result)
}

/// Adds each new photo's embedded EXIF thumbnail, so the user sees what they confirm.
/// Thumbnails are small (about 10 KB) and read without loading the whole JPEG.
pub fn add_thumbnails(exiftool: &Path, result: &mut DeliveryResult) {
    let paths: Vec<PathBuf> = result
        .photos
        .iter()
        .filter(|p| p.action == "new")
        .map(|p| result.shoot.join(&p.source))
        .collect();
    let mut found = BTreeMap::new();
    for chunk in paths.chunks(64) {
        let Ok(bytes) = crate::exiftool::read_batch(exiftool, chunk, &["-b", "-ThumbnailImage"])
        else {
            continue;
        };
        let Ok(rows) = serde_json::from_slice::<Vec<Value>>(&bytes) else {
            continue;
        };
        for row in rows {
            let data = row
                .get("ThumbnailImage")
                .and_then(Value::as_str)
                .and_then(|s| s.strip_prefix("base64:"));
            if let (Some(path), Some(data)) = (tag(&row, "SourceFile"), data)
                // Only a JPEG (base64 of FF D8 FF), and only base64 characters.
                && data.starts_with("/9j/")
                && data
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b))
            {
                found.insert(normal_path(path), format!("data:image/jpeg;base64,{data}"));
            }
        }
    }
    let shoot = result.shoot.clone();
    for p in result.photos.iter_mut().filter(|p| p.action == "new") {
        p.thumbnail = found
            .get(&normal_path(&shoot.join(&p.source).to_string_lossy()))
            .cloned();
    }
}

#[cfg(test)]
mod tests;

pub fn hash_file(path: &Path) -> io::Result<u64> {
    let mut file = File::open(path)?;
    let mut hash = xxhash_rust::xxh64::Xxh64::new(0);
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hash.digest());
        }
        hash.update(&buf[..n]);
    }
}

fn verify(source: &Path, copy: &Path) -> io::Result<()> {
    if hash_file(source)? != hash_file(copy)? {
        return Err(io::Error::other(
            "The delivered copy doesn't match its source. Check the connection and try again.",
        ));
    }
    Ok(())
}

fn write_stars(exiftool: &Path, staged: &Path, stars: i32) -> io::Result<()> {
    let meta = metadata(exiftool, &[staged.to_path_buf()])?;
    let meta = meta
        .values()
        .next()
        .ok_or_else(|| io::Error::other("No rating metadata returned"))?;
    let percent = [0, 1, 25, 50, 75, 99][stars as usize];
    let agrees = |value: &Value, key: &str, expected: i32| {
        value
            .get(key)
            .is_none_or(|v| v.as_i64() == Some(i64::from(expected)))
    };
    if rating(meta) == Some(stars)
        && agrees(meta, "IFD0:Rating", stars)
        && agrees(meta, "IFD0:RatingPercent", percent)
    {
        return Ok(());
    }
    let mut args = tempfile::NamedTempFile::new()?;
    writeln!(args, "-overwrite_original\n-XMP-xmp:Rating={stars}")?;
    if meta.get("IFD0:Rating").is_some() {
        writeln!(args, "-IFD0:Rating={stars}")?;
    }
    if meta.get("IFD0:RatingPercent").is_some() {
        writeln!(args, "-IFD0:RatingPercent={}", percent)?;
    }
    writeln!(args, "{}", staged.display())?;
    args.flush()?;
    let output = crate::process::command(exiftool)
        .args(["-charset", "filename=utf8", "-@"])
        .arg(args.path())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "Couldn't write stars: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let read = metadata(exiftool, &[staged.to_path_buf()])?;
    if read.values().next().is_none_or(|read| {
        rating(read) != Some(stars)
            || (meta.get("IFD0:Rating").is_some()
                && read.get("IFD0:Rating").and_then(Value::as_i64) != Some(i64::from(stars)))
            || (meta.get("IFD0:RatingPercent").is_some()
                && read.get("IFD0:RatingPercent").and_then(Value::as_i64)
                    != Some(i64::from(percent)))
    }) {
        return Err(io::Error::other(
            "The delivered stars didn't read back correctly.",
        ));
    }
    Ok(())
}

/// Renames `from` to `to`, never replacing anything already at `to`.
fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    let moved = {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
        let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(Some(0)).collect() };
        let (a, b) = (wide(from), wide(to));
        // No MOVEFILE_REPLACE_EXISTING and no MOVEFILE_COPY_ALLOWED: this fails when
        // `to` exists, or is on another volume, instead of overwriting or copying.
        // SAFETY: both are NUL-terminated UTF-16 paths that outlive the call.
        if unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), 0) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    #[cfg(not(windows))]
    let moved = if fs::symlink_metadata(to).is_ok() {
        Err(io::Error::from(io::ErrorKind::AlreadyExists))
    } else {
        fs::rename(from, to)
    };
    match moved {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists && fs::symlink_metadata(to).is_ok() => {
            Err(io::Error::new(io::ErrorKind::AlreadyExists, e))
        }
        other => other,
    }
}

/// Delivers what the user confirmed. Nothing in the finished photos folder is replaced
/// or removed: a name that's already there is skipped.
pub fn deliver(
    shoot: &Path,
    exiftool: &Path,
    catalog: Option<&Path>,
    target: &Path,
    confirmed: &[Confirmed],
) -> io::Result<DeliveryResult> {
    deliver_with(shoot, exiftool, catalog, target, confirmed, verify)
}

fn deliver_with(
    shoot: &Path,
    exiftool: &Path,
    catalog: Option<&Path>,
    target: &Path,
    confirmed: &[Confirmed],
    verify_copy: impl Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<DeliveryResult> {
    let mut result = plan(shoot, exiftool, catalog, target)?;
    let mut wanted = confirmed.to_vec();
    wanted.sort();
    wanted.dedup();
    if result.confirmed() != wanted {
        return Err(io::Error::other(
            "The photos to deliver changed since you checked them. Press Deliver again to see the new list.",
        ));
    }
    result.plan_only = false;
    if result.new == 0 {
        return Ok(result);
    }
    // 1. Copy and check each new photo in a hidden folder inside the shoot.
    let staging = shoot.join(STAGING);
    match fs::symlink_metadata(&staging) {
        // Left by an interrupted delivery: it only ever holds copies.
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => fs::remove_dir_all(&staging)?,
        Ok(_) => {
            return Err(io::Error::other(format!(
                "{} isn't a folder Card Intake made. Nothing was delivered.",
                staging.display()
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    fs::create_dir(&staging)?;
    let mut staged = Vec::new();
    for (i, p) in result.photos.iter_mut().enumerate() {
        if p.action != "new" {
            continue;
        }
        let copy = staging.join(&p.name);
        match stage(&shoot.join(&p.source), &copy, exiftool, p, &verify_copy) {
            Ok(()) => staged.push(i),
            Err(e) => {
                result.errors.push(format!("{}: {e}", p.source));
                p.action = "failed".into();
                let _ = fs::remove_file(&copy);
            }
        }
    }
    // 2. Move the copies in: a new folder in one rename, so Immich never sees half of
    // it, or file by file into a folder that's already there.
    let mut whole = false;
    if !staged.is_empty() && !target.exists() {
        match rename_new(&staging, target) {
            Ok(()) => whole = true,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            // Another volume: make the folder and move file by file.
            Err(_) => match fs::create_dir(target) {
                Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
                    let _ = fs::remove_dir_all(&staging);
                    return Err(e);
                }
                _ => {}
            },
        }
    }
    for i in staged {
        let p = &mut result.photos[i];
        let placed = if whole {
            Ok(true)
        } else {
            place(&staging.join(&p.name), &target.join(&p.name), &verify_copy)
        };
        match placed {
            Ok(true) => {
                p.action = "delivered".into();
                result.delivered += 1;
            }
            // Something with this name arrived meanwhile: leave it alone.
            Ok(false) => {
                p.action = "exists".into();
                result.existing += 1;
            }
            Err(e) => {
                result.errors.push(format!("{}: {e}", p.source));
                p.action = "failed".into();
            }
        }
    }
    if !whole {
        let _ = fs::remove_dir_all(&staging);
    }
    // 3. Remember the names, so each photo keeps its name next time.
    if result.delivered > 0
        && let Err(e) = remember(shoot, target, &result)
    {
        result.warnings.push(format!(
            "The photos were delivered, but Card Intake couldn't record them in the shoot folder: {e}"
        ));
    }
    Ok(result)
}

fn stage(
    source: &Path,
    copy: &Path,
    exiftool: &Path,
    photo: &Photo,
    verify_copy: impl Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    // Reject a path ExifTool's argument file can't hold instead of silently dropping it.
    if source.to_string_lossy().contains(['\r', '\n'])
        || copy.to_string_lossy().contains(['\r', '\n'])
    {
        return Err(io::Error::other("A photo path contains a newline."));
    }
    fs::copy(source, copy)?;
    // Exporters may mark files read-only. Only the new copy is made writable.
    let mut permissions = fs::metadata(copy)?.permissions();
    #[cfg(windows)]
    {
        // Windows only: readonly is a file attribute, so this grants nothing else.
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(permissions.mode() | 0o200);
    }
    fs::set_permissions(copy, permissions)?;
    verify_copy(source, copy)?;
    if photo.stars_from != StarsFrom::None
        && let Some(n) = photo.stars
    {
        write_stars(exiftool, copy, n)?;
    }
    fs::OpenOptions::new().write(true).open(copy)?.sync_all()
}

/// Moves a checked copy into place. `Ok(false)` when something with its name is there.
fn place(
    from: &Path,
    to: &Path,
    verify_copy: impl Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<bool> {
    match rename_new(from, to) {
        Ok(()) => return Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) if e.kind() != io::ErrorKind::CrossesDevices => return Err(e),
        Err(_) => {}
    }
    // Another volume: copy beside it under a hidden name, check it, then rename it.
    let name = to.file_name().unwrap_or_default().to_string_lossy();
    let temp = to.with_file_name(format!("{TEMP}{name}"));
    let _ = fs::remove_file(&temp);
    fs::copy(from, &temp)?;
    match verify_copy(from, &temp).and_then(|()| rename_new(&temp, to)) {
        Ok(()) => Ok(true),
        Err(e) => {
            let _ = fs::remove_file(&temp);
            if e.kind() == io::ErrorKind::AlreadyExists {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}

fn remember(shoot: &Path, target: &Path, result: &DeliveryResult) -> io::Result<()> {
    let mut m = manifest(shoot)?;
    for p in result.photos.iter().filter(|p| p.action == "delivered") {
        match m
            .assignments
            .iter_mut()
            .find(|a| a.name.eq_ignore_ascii_case(&p.name))
        {
            Some(a) => {
                a.identities.extend(p.identities.iter().cloned());
                a.identities.sort();
                a.identities.dedup();
            }
            None => m.assignments.push(Assignment {
                name: p.name.clone(),
                identities: p.identities.clone(),
            }),
        }
    }
    m.version = 2;
    m.target = target.file_name().map(|n| n.to_string_lossy().into_owned());
    m.last_delivered = Some(Utc::now());
    let mut file = tempfile::Builder::new().prefix(TEMP).tempfile_in(shoot)?;
    file.write_all(&serde_json::to_vec_pretty(&m).map_err(io::Error::other)?)?;
    file.as_file().sync_all()?;
    file.persist(shoot.join(MANIFEST))
        .map_err(io::Error::other)?;
    Ok(())
}
