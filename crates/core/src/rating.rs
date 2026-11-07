//! Initial ratings from the card's attributes and shooting settings.

use crate::{exiftool, scan, xmp};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// This URI identifies the sidecar fields and the Lightroom plug-in's Pick request.
pub const NAMESPACE: &str = "https://github.com/semmmer/card_intake/ns/1.0/";

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct RatingSummary {
    pub rated: usize,
    pub locked: usize,
    pub camera: usize,
    pub computed: usize,
    pub kept: usize,
    pub stars: [usize; 6],
}

impl RatingSummary {
    pub(crate) fn record(&mut self, rating: &InitialRating, written: xmp::Written) {
        self.rated += 1;
        match rating.source {
            RatingSource::Locked => self.locked += 1,
            RatingSource::Camera => self.camera += 1,
            RatingSource::Computed => self.computed += 1,
        }
        self.kept += usize::from(written.kept);
        if let Ok(stars @ 0..=5) = usize::try_from(written.rating) {
            self.stars[stars] += 1;
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Settings {
    pub iso: Option<u64>,
    pub exposure: Option<f64>,
    pub focal_length: Option<f64>,
    pub crop_factor: Option<f64>,
    pub camera_rating: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RatingSource {
    Locked,
    Camera,
    Computed,
}

impl RatingSource {
    pub fn name(self) -> &'static str {
        match self {
            Self::Locked => "locked in camera",
            Self::Camera => "camera rating",
            Self::Computed => "computed",
        }
    }

    fn keyword(self) -> &'static str {
        match self {
            Self::Locked => "Locked in camera",
            Self::Camera => "Camera rating",
            Self::Computed => "Computed rating",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InitialRating {
    pub stars: u8,
    pub source: RatingSource,
    pub pick: bool,
    pub settings: Settings,
    pub iso_deduction: u8,
    pub iso_skipped: Option<&'static str>,
    pub safe_speed: Option<f64>,
    pub shutter_deduction: u8,
    pub shutter_skipped: Option<&'static str>,
}

pub fn iso_deduction(iso: u64) -> u8 {
    match iso {
        0..=800 => 0,
        801..=3200 => 1,
        3201..=6400 => 2,
        6401..=12800 => 3,
        12801..=25600 => 4,
        _ => 5,
    }
}

fn positive(value: f64) -> Option<f64> {
    (value.is_finite() && value > 0.0).then_some(value)
}

pub fn initial_rating(settings: &Settings, locked: bool) -> InitialRating {
    let mut rating = InitialRating {
        stars: 5,
        source: RatingSource::Computed,
        pick: locked,
        settings: settings.clone(),
        iso_deduction: 0,
        iso_skipped: None,
        safe_speed: None,
        shutter_deduction: 0,
        shutter_skipped: None,
    };
    if locked {
        rating.source = RatingSource::Locked;
        return rating;
    }
    if let Some(stars @ 1..=5) = settings.camera_rating {
        rating.source = RatingSource::Camera;
        rating.stars = stars;
        return rating;
    }
    match settings.iso.filter(|n| *n > 0) {
        Some(iso) => rating.iso_deduction = iso_deduction(iso),
        None => rating.iso_skipped = Some("ISO not recorded"),
    }
    let focal = settings.focal_length.and_then(positive);
    let crop = settings.crop_factor.and_then(positive);
    rating.safe_speed = focal.zip(crop).and_then(|(f, c)| positive(1.0 / (f * c)));
    rating.shutter_skipped = match settings.exposure.and_then(positive) {
        None => Some("Exposure time not recorded"),
        Some(exposure) if exposure >= 1.0 => Some("Exposure is at least 1 second"),
        Some(_) if focal.is_none() => Some("Focal length not recorded"),
        Some(_) if crop.is_none() => Some("Crop factor not recorded"),
        Some(exposure) => match rating.safe_speed {
            Some(safe) => {
                // A tiny tolerance keeps an exact stop from falling below its integer.
                let stops = (exposure / safe).log2() + 1e-9;
                rating.shutter_deduction = stops.floor().max(0.0) as u8;
                None
            }
            None => Some("Safe shutter speed unavailable"),
        },
    };
    rating.stars = 5u8
        .saturating_sub(rating.iso_deduction)
        .saturating_sub(rating.shutter_deduction);
    rating
}

impl InitialRating {
    pub(crate) fn keywords(&self) -> Vec<String> {
        let mut labels = vec![self.source.keyword().to_string()];
        if self.source == RatingSource::Computed {
            if self.iso_deduction > 0 {
                labels.push(format!("High ISO -{}", self.iso_deduction));
            }
            if self.shutter_deduction > 0 {
                labels.push(format!("Slow shutter -{}", self.shutter_deduction));
            }
            if self.iso_skipped.is_some() {
                labels.push("ISO not recorded".into());
            }
            if self.shutter_skipped.is_some() {
                labels.push("Shutter not checked".into());
            }
        }
        labels
    }
}

/// Protect is a source-file attribute, independent of EXIF and destination permissions.
pub fn locked_on_card(path: &Path) -> std::io::Result<bool> {
    let meta = fs::metadata(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Ok(meta.file_attributes() & 1 != 0)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let locked = meta.mode() & 0o222 == 0;
        #[cfg(target_os = "macos")]
        {
            use std::os::macos::fs::MetadataExt;
            // UF_IMMUTABLE and SF_IMMUTABLE (Finder's Locked flag).
            Ok(locked || meta.st_flags() & 0x0002_0002 != 0)
        }
        #[cfg(not(target_os = "macos"))]
        Ok(locked)
    }
    #[cfg(not(any(windows, unix)))]
    Ok(meta.permissions().readonly())
}

#[derive(Default)]
pub(crate) struct Readings {
    pub settings: HashMap<PathBuf, Settings>,
    pub warnings: Vec<String>,
}

pub(crate) fn read_settings(tool: Option<&Path>, photos: &[PathBuf]) -> Readings {
    let mut result = Readings::default();
    if photos.is_empty() {
        return result;
    }
    let Some(tool) = tool else {
        result.warnings.push("Initial raw ratings need ExifTool. Install it or set CARD_INTAKE_EXIFTOOL, then import again; existing copies will be checked and rated.".into());
        return result;
    };
    let tags = [
        "-G1",
        "-a",
        "-ISO",
        "-PhotographicSensitivity",
        "-ISOSpeed",
        "-RecommendedExposureIndex",
        "-ExposureTime",
        "-FocalLength",
        "-FocalLengthIn35mmFormat",
        "-ScaleFactor35efl",
        "-FocalLength35efl",
        "-APS-CSizeCapture",
        "-Rating",
        "-FileType",
        "-Warning",
        "-Error",
    ];
    for chunk in photos.chunks(256) {
        let requested: HashMap<_, _> = chunk.iter().map(|p| (path_key(p), p)).collect();
        let parsed = exiftool::read_batch(tool, chunk, &tags).and_then(|bytes| {
            serde_json::from_slice::<Vec<Map<String, Value>>>(&bytes).map_err(std::io::Error::other)
        });
        match parsed {
            Ok(items) => {
                for obj in items {
                    let Some(path) = obj.get("SourceFile").and_then(Value::as_str) else {
                        continue;
                    };
                    if let Some(warning) =
                        obj.get("ExifTool:Warning").or_else(|| obj.get("Warning"))
                    {
                        result
                            .warnings
                            .push(format!("Rating metadata warning for {path}: {warning}"));
                    }
                    if let Some(error) = obj.get("ExifTool:Error").or_else(|| obj.get("Error")) {
                        result
                            .warnings
                            .push(format!("Couldn't read rating settings for {path}: {error}"));
                    } else if !readable_raw(&obj) {
                        result.warnings.push(format!("Couldn't read rating settings for {path}: ExifTool didn't recognise a proprietary raw photo."));
                    } else if let Some(source) = requested.get(&path_key(Path::new(path))) {
                        result
                            .settings
                            .insert((*source).clone(), settings_from_tags(&obj));
                    }
                }
            }
            Err(error) => result
                .warnings
                .push(format!("Couldn't read initial raw ratings: {error}")),
        }
    }
    result
}

fn readable_raw(obj: &Map<String, Value>) -> bool {
    obj.get("File:FileType")
        .or_else(|| obj.get("FileType"))
        .and_then(Value::as_str)
        .is_some_and(|kind| {
            kind == "TIFF" || scan::uses_raw_sidecar(Path::new(&format!("photo.{kind}")))
        })
}

fn path_key(path: &Path) -> String {
    let name = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        name.strip_prefix("//?/").unwrap_or(&name).to_lowercase()
    }
    #[cfg(not(windows))]
    {
        name
    }
}

fn number(obj: &Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        let value = obj.get(*key)?;
        value
            .as_f64()
            .or_else(|| {
                let text = value.as_str()?.trim();
                if let Some((n, d)) = text.split_once('/') {
                    positive(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?)
                } else {
                    text.parse().ok()
                }
            })
            .and_then(positive)
    })
}

fn settings_from_tags(obj: &Map<String, Value>) -> Settings {
    let focal = number(
        obj,
        &["ExifIFD:FocalLength", "EXIF:FocalLength", "FocalLength"],
    );
    let equivalent = number(
        obj,
        &["ExifIFD:FocalLengthIn35mmFormat", "FocalLengthIn35mmFormat"],
    )
    .or_else(|| {
        number(obj, &["Composite:FocalLength35efl", "FocalLength35efl"])
            // Without a scale, this composite can just repeat the actual focal length.
            .filter(|e| focal.is_some_and(|f| (e / f - 1.0).abs() > 1e-6))
    });
    let scale = number(obj, &["Composite:ScaleFactor35efl", "ScaleFactor35efl"]);
    let equivalent_crop = equivalent.zip(focal).map(|(e, f)| e / f);
    let mut crop = scale.or(equivalent_crop);
    if let (Some(scale), Some(eq)) = (crop, equivalent_crop)
        && eq > scale * 1.15
    {
        crop = Some(eq);
    }
    // Some Sony composites describe the whole sensor even in APS-C capture mode.
    let sony_crop = obj
        .get("Sony:APS-CSizeCapture")
        .or_else(|| obj.get("APS-CSizeCapture"));
    if sony_crop.is_some_and(|v| v.as_u64() == Some(1) || v.as_str() == Some("On")) {
        crop = Some(crop.unwrap_or(1.5).max(1.5));
    }
    // ISO 65535 is EXIF's overflow marker. Extended EXIF fields hold the actual ISO.
    let iso = number(
        obj,
        &[
            "ExifIFD:ISO",
            "ExifIFD:PhotographicSensitivity",
            "ISO",
            "PhotographicSensitivity",
        ],
    );
    let extended = number(
        obj,
        &[
            "ExifIFD:ISOSpeed",
            "ISOSpeed",
            "ExifIFD:RecommendedExposureIndex",
            "RecommendedExposureIndex",
        ],
    );
    let iso = if iso.is_none_or(|n| n >= 65535.0) {
        extended.or(iso.filter(|n| *n != 65535.0))
    } else {
        iso
    };
    let camera_rating = [
        "XMP-xmp:Rating",
        "IFD0:Rating",
        "ExifIFD:Rating",
        "Sony:Rating",
        "Nikon:Rating",
        "Rating",
    ]
    .iter()
    .copied()
    .chain(
        obj.keys()
            .filter(|key| key.ends_with(":Rating"))
            .map(String::as_str),
    )
    .find_map(|key| {
        let n = number(obj, &[key])?;
        (n.fract() == 0.0 && (1.0..=5.0).contains(&n)).then_some(n as u8)
    });
    Settings {
        iso: iso
            .filter(|n| n.fract() == 0.0 && *n < u64::MAX as f64)
            .map(|n| n as u64),
        exposure: number(obj, &["ExifIFD:ExposureTime", "ExposureTime"]),
        focal_length: focal,
        crop_factor: crop,
        camera_rating,
    }
}

pub(crate) fn write_sidecar(path: &Path, rating: &InitialRating) -> Result<xmp::Written, String> {
    xmp::write(path, rating).map_err(|e| {
        format!(
            "Couldn't rate {}: {e}. The sidecar was left untouched.",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            iso: Some(100),
            exposure: Some(0.01),
            focal_length: Some(100.0),
            crop_factor: Some(1.0),
            camera_rating: None,
        }
    }

    #[test]
    fn every_iso_edge() {
        for (iso, deduction) in [
            (1, 0),
            (800, 0),
            (801, 1),
            (3200, 1),
            (3201, 2),
            (6400, 2),
            (6401, 3),
            (12800, 3),
            (12801, 4),
            (25600, 4),
            (25601, 5),
            (102400, 5),
        ] {
            assert_eq!(iso_deduction(iso), deduction, "ISO {iso}");
        }
    }

    #[test]
    fn shutter_stops_and_crop() {
        for (exposure, deduction) in [
            (0.005, 0),
            (0.01, 0),
            (0.02, 1),
            (0.01 * 2f64.powf(1.9), 1),
            (0.04, 2),
            (1.0, 0),
            (30.0, 0),
        ] {
            let s = Settings {
                exposure: Some(exposure),
                ..settings()
            };
            assert_eq!(
                initial_rating(&s, false).shutter_deduction,
                deduction,
                "{exposure}"
            );
        }
        let s = Settings {
            focal_length: Some(50.0),
            crop_factor: Some(1.5),
            exposure: Some(2.0 / 75.0),
            ..settings()
        };
        let rating = initial_rating(&s, false);
        assert_eq!(rating.safe_speed, Some(1.0 / 75.0));
        assert_eq!(rating.shutter_deduction, 1);
    }

    #[test]
    fn missing_settings_and_long_exposure_are_explained() {
        for focal in [None, Some(0.0), Some(f64::NAN)] {
            let r = initial_rating(
                &Settings {
                    iso: Some(6400),
                    focal_length: focal,
                    ..settings()
                },
                false,
            );
            assert_eq!(r.stars, 3);
            assert_eq!(r.shutter_skipped, Some("Focal length not recorded"));
        }
        for s in [
            Settings {
                crop_factor: None,
                ..settings()
            },
            Settings {
                exposure: None,
                ..settings()
            },
        ] {
            assert!(
                initial_rating(&s, false)
                    .keywords()
                    .contains(&"Shutter not checked".into())
            );
        }
        let r = initial_rating(
            &Settings {
                iso: None,
                exposure: Some(1.0),
                ..settings()
            },
            false,
        );
        assert_eq!(r.stars, 5);
        assert_eq!(r.iso_skipped, Some("ISO not recorded"));
        assert_eq!(r.shutter_skipped, Some("Exposure is at least 1 second"));
        let r = initial_rating(
            &Settings {
                iso: Some(12800),
                exposure: Some(1.0),
                ..settings()
            },
            false,
        );
        assert_eq!((r.stars, r.iso_deduction, r.shutter_deduction), (2, 3, 0));
    }

    #[test]
    fn lock_beats_camera_beats_computed_and_ratings_clamp() {
        let mut s = Settings {
            iso: Some(102400),
            exposure: Some(0.5),
            camera_rating: Some(2),
            ..settings()
        };
        let lock = initial_rating(&s, true);
        assert_eq!(
            (lock.stars, lock.source, lock.pick),
            (5, RatingSource::Locked, true)
        );
        assert_eq!(initial_rating(&s, false).source, RatingSource::Camera);
        assert_eq!(initial_rating(&s, false).stars, 2);
        s.camera_rating = Some(0);
        assert_eq!(initial_rating(&s, false).stars, 0);
        assert_eq!(initial_rating(&Settings::default(), false).stars, 5);
    }

    #[test]
    fn metadata_crop_extended_iso_and_camera_rating() {
        let parse = |v: Value| settings_from_tags(v.as_object().unwrap());
        let s = parse(
            serde_json::json!({"ExifIFD:ISO":65535,"ExifIFD:ISOSpeed":102400,"ExifIFD:FocalLength":35,"ExifIFD:FocalLengthIn35mmFormat":52,"Composite:ScaleFactor35efl":1,"Sony:APS-CSizeCapture":1,"XMP-xmp:Rating":0,"Sony:Rating":4}),
        );
        assert_eq!(
            (s.iso, s.crop_factor, s.camera_rating),
            (Some(102400), Some(1.5), Some(4))
        );
        let s = parse(
            serde_json::json!({"ExifIFD:ISO":65535,"ExifIFD:RecommendedExposureIndex":128000,"ExifIFD:FocalLength":50,"ExifIFD:FocalLengthIn35mmFormat":75,"XMP-xmp:Rating":-1}),
        );
        assert_eq!(
            (s.iso, s.crop_factor, s.camera_rating),
            (Some(128000), Some(1.5), None)
        );
        assert_eq!(parse(serde_json::json!({"ExifIFD:ISO":65535})).iso, None);
        assert_eq!(
            parse(serde_json::json!({"Composite:ScaleFactor35efl":0})).crop_factor,
            None
        );
        assert_eq!(
            parse(serde_json::json!({"ExifIFD:FocalLength":35,"Composite:FocalLength35efl":35}))
                .crop_factor,
            None
        );
    }

    #[test]
    fn real_sony_apsc_body_and_full_frame_crop_metadata() {
        // raw.pixls.us samples 1971 (ILCE-6000) and 6869 (ILCE-7CM2), ExifTool 13.59.
        let apsc = serde_json::json!({"ExifIFD:ISO":100,"ExifIFD:ExposureTime":0.00625,"ExifIFD:FocalLength":39,"ExifIFD:FocalLengthIn35mmFormat":58,"Composite:ScaleFactor35efl":1.48717948717949,"Composite:FocalLength35efl":58.0000000000001});
        let crop = serde_json::json!({"ExifIFD:ISO":100,"ExifIFD:ExposureTime":0.01,"ExifIFD:FocalLength":24,"ExifIFD:FocalLengthIn35mmFormat":36,"Composite:ScaleFactor35efl":1.5,"Composite:FocalLength35efl":36,"Sony:APS-CSizeCapture":1});
        for (tags, equivalent) in [(apsc, 58.0), (crop, 36.0)] {
            let settings = settings_from_tags(tags.as_object().unwrap());
            let rating = initial_rating(&settings, false);
            assert!((rating.safe_speed.unwrap() - 1.0 / equivalent).abs() < 1e-12);
            assert_eq!((rating.stars, rating.shutter_deduction), (5, 0));
        }
    }

    #[test]
    fn an_unreadable_raw_is_not_an_unrated_photo_with_missing_settings() {
        for kind in ["TXT", "JPEG", "DNG", "", "Unknown"] {
            let tags = serde_json::json!({"File:FileType":kind});
            assert!(!readable_raw(tags.as_object().unwrap()));
        }
        for kind in ["ARW", "NEF", "NRW", "TIFF"] {
            let tags = serde_json::json!({"File:FileType":kind});
            assert!(readable_raw(tags.as_object().unwrap()));
        }
    }

    #[test]
    fn protect_comes_from_the_source_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.ARW");
        fs::write(&path, b"original").unwrap();
        let original = fs::metadata(&path).unwrap().permissions();
        let mut protected = original.clone();
        protected.set_readonly(true);
        fs::set_permissions(&path, protected).unwrap();
        assert!(locked_on_card(&path).unwrap());
        fs::set_permissions(&path, original).unwrap();
        assert!(!locked_on_card(&path).unwrap());
    }
}
