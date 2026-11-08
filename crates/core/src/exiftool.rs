//! Runs ExifTool for what only it can decode, such as Sony's shutter count, which sits
//! in an enciphered maker-note block, and Nikon's, which its raws and JPEGs carry in a
//! maker note the camera writes.

use crate::cameras;
use chrono::NaiveDateTime;
use serde::Serialize;
use serde_json::{Map, Value};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct CameraDetails {
    pub make: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub internal_serial: Option<String>,
    pub firmware: Option<String>,
    pub lens: Option<String>,
    pub shutter_count: Option<u64>,
    pub electronic_count: Option<u64>,
}

const TAGS: &[&str] = &[
    "-Make",
    "-Model",
    "-SerialNumber",
    "-InternalSerialNumber",
    "-Software",
    "-LensModel",
    "-ShutterCount",
    "-ShutterCount2",
    "-ShutterCount3",
    "-MechanicalShutterCount",
    "-ElectronicShutterCount",
];

pub fn camera_details(exiftool: &Path, photo: &Path) -> io::Result<CameraDetails> {
    // -fast, not -fast2: -fast2 skips the maker notes, where Sony keeps the shutter count.
    let output = crate::process::command(exiftool)
        .args(["-json", "-n", "-fast"])
        .args(TAGS)
        .arg(photo)
        .output()?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!("ExifTool failed: {}", err.trim())));
    }
    parse_json(&output.stdout)
}

pub fn parse_json(bytes: &[u8]) -> io::Result<CameraDetails> {
    let value: Value = serde_json::from_slice(bytes).map_err(io::Error::other)?;
    let obj = value
        .get(0)
        .and_then(Value::as_object)
        .ok_or_else(|| io::Error::other("ExifTool returned no data"))?;
    Ok(details(obj))
}

fn details(obj: &Map<String, Value>) -> CameraDetails {
    let text = |key: &str| {
        obj.get(key)
            .map(|v| match v {
                Value::String(s) => s.trim().to_string(),
                other => other.to_string(),
            })
            .filter(|s| !s.is_empty())
    };
    let number = |key: &str| {
        obj.get(key)
            .and_then(|v| v.as_u64().or_else(|| v.as_str()?.trim().parse().ok()))
    };
    let model = text("Model");
    // The count a body keeps in ShutterCount: its mechanical actuations alone on bodies
    // that have a mechanical shutter to count, its whole (electronic) count on a body
    // that doesn't. ImageCount and ShutterCount2/3 are not independent counters.
    let total = number("ShutterCount")
        .or_else(|| number("ShutterCount3"))
        .or_else(|| number("ShutterCount2"));
    let mechanical = number("MechanicalShutterCount");
    // Nikon's ShutterCount "includes both mechanical and electronic shutter
    // activations" (ExifTool's Nikon tags), so on a Nikon that also counts its
    // mechanical shutter (the Zf) the difference is its electronic count.
    let nikon = text("Make")
        .or_else(|| model.clone())
        .is_some_and(|m| m.to_ascii_uppercase().starts_with("NIKON"));
    let nikon_electronic = match (nikon, total, mechanical) {
        (true, Some(total), Some(mechanical)) => total.checked_sub(mechanical),
        _ => None,
    };
    // A body without a mechanical shutter (the Nikon Z 9) writes 0 in
    // MechanicalShutterCount on every photo, while ShutterCount carries the real count:
    // there the 0 is not a reading and the total is. On a body with a mechanical shutter,
    // 0 actuations there is a valid reading, so it comes first.
    let (shutter_count, electronic_count) = match &model {
        Some(model) if !cameras::has_mechanical_shutter(model) => (total, None),
        _ => (
            mechanical.or(total),
            number("ElectronicShutterCount").or(nikon_electronic),
        ),
    };

    CameraDetails {
        make: text("Make"),
        model,
        serial: text("SerialNumber"),
        internal_serial: text("InternalSerialNumber"),
        firmware: text("Software"),
        lens: text("LensModel"),
        shutter_count,
        electronic_count,
    }
}

/// "ILCE-7RM6 v1.01" and Nikon's "Ver.05.30" both become "1.01" and "05.30".
pub fn firmware_version(software: &str) -> String {
    let last = software.split_whitespace().last().unwrap_or(software);
    // Nikon writes the version as "Ver.05.30", Sony as "ILCE-7RM6 v1.01".
    let version = if last
        .get(..4)
        .is_some_and(|p| p.eq_ignore_ascii_case("ver."))
    {
        &last[4..]
    } else {
        last
    };
    version.trim_start_matches(['v', 'V']).to_string()
}

/// One photo's shutter count and when it was taken.
#[derive(Debug, Clone, PartialEq)]
pub struct ShutterSample {
    pub model: String,
    pub serial: Option<String>,
    /// Sony's internal serial number. Some photos lack the regular one but carry this.
    pub internal_serial: Option<String>,
    pub firmware: Option<String>,
    pub count: u64,
    pub electronic_count: Option<u64>,
    /// The camera's clock at capture (EXIF DateTimeOriginal).
    pub taken: NaiveDateTime,
}

/// Reads the shutter count of many photos with one ExifTool run.
///
/// File names go through a UTF-8 argument file: on Windows, ExifTool can't take
/// non-ASCII names (e.g. a shoot called 夜景) on its command line.
pub fn shutter_samples(exiftool: &Path, photos: &[PathBuf]) -> io::Result<Vec<ShutterSample>> {
    let mut tags = TAGS.to_vec();
    tags.push("-DateTimeOriginal");
    let bytes = read_batch(exiftool, photos, &tags)?;
    parse_shutter_json(&bytes)
}

/// Runs a read-only batch through a UTF-8 argument file, including partial results.
pub(crate) fn read_batch(
    exiftool: &Path,
    photos: &[PathBuf],
    tags: &[&str],
) -> io::Result<Vec<u8>> {
    if photos.is_empty() {
        return Ok(b"[]".to_vec());
    }
    let mut list = tempfile::Builder::new()
        .prefix("card-intake-exiftool-")
        .suffix(".txt")
        .tempfile()?;
    for photo in photos {
        let name = photo.to_string_lossy();
        if !name.contains(['\n', '\r']) && !name.starts_with(['-', '#']) {
            writeln!(list, "{name}")?;
        }
    }
    list.flush()?;
    let list = list.into_temp_path();
    let output = crate::process::command(exiftool)
        .args(["-json", "-n", "-fast", "-charset", "filename=utf8"])
        .args(tags)
        .arg("-@")
        .arg(&list)
        .output()?;
    // ExifTool exits with an error when any one file fails; the others are still read.
    if output.stdout.iter().all(u8::is_ascii_whitespace) {
        if output.status.success() {
            return Ok(b"[]".to_vec());
        }
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!("ExifTool failed: {}", err.trim())));
    }
    Ok(output.stdout)
}

/// The photos worth reading in a folder: one per shot, preferring the raw file when
/// the camera also saved a JPEG or HEIF.
pub fn one_per_shot(photos: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    const RANK: &[&str] = &["arw", "nef", "nrw", "hif", "heic", "jpg", "jpeg"];
    let rank = |p: &Path| {
        let ext = p.extension()?.to_str()?.to_ascii_lowercase();
        RANK.iter().position(|r| *r == ext)
    };
    let mut best: std::collections::BTreeMap<PathBuf, (usize, PathBuf)> = Default::default();
    for photo in photos {
        let Some(r) = rank(&photo) else { continue };
        let key = photo
            .with_extension("")
            .to_string_lossy()
            .to_lowercase()
            .into();
        match best.get(&key) {
            Some((existing, _)) if *existing <= r => {}
            _ => {
                best.insert(key, (r, photo));
            }
        }
    }
    best.into_values().map(|(_, p)| p).collect()
}

pub fn parse_shutter_json(bytes: &[u8]) -> io::Result<Vec<ShutterSample>> {
    let value: Value = serde_json::from_slice(bytes).map_err(io::Error::other)?;
    let Some(items) = value.as_array() else {
        return Err(io::Error::other("ExifTool returned no list"));
    };
    Ok(items
        .iter()
        .filter_map(|item| {
            let obj = item.as_object()?;
            let d = details(obj);
            let taken = obj
                .get("DateTimeOriginal")
                .and_then(Value::as_str)
                .and_then(parse_exif_time)?;
            Some(ShutterSample {
                model: d.model?,
                serial: d.serial,
                internal_serial: d.internal_serial,
                firmware: d.firmware.as_deref().map(firmware_version),
                count: d.shutter_count?,
                electronic_count: d.electronic_count,
                taken,
            })
        })
        .collect())
}

/// "2026:11:06 17:21:17", optionally followed by sub-seconds or a time zone.
fn parse_exif_time(text: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(text.get(..19)?, "%Y:%m:%d %H:%M:%S").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn electronic_count_is_explicit_and_zero_mechanical_is_valid() {
        let details = parse_json(br#"[{"ShutterCount":0,"ElectronicShutterCount":321,"ImageCount":900,"ShutterCount2":22}]"#).unwrap();
        assert_eq!(details.shutter_count, Some(0));
        assert_eq!(details.electronic_count, Some(321));
        let alternative =
            parse_json(br#"[{"ShutterCount":50,"ShutterCount2":100,"ImageCount":200}]"#).unwrap();
        assert_eq!(alternative.electronic_count, None);
    }

    #[test]
    fn parses_exiftool_json() {
        let json = br#"[{
            "SourceFile": "/Volumes/Untitled/DCIM/10061106/EM_01411.ARW",
            "Make": "SONY",
            "Model": "ILCE-7RM6",
            "SerialNumber": "03506694",
            "Software": "ILCE-7RM6 v1.00",
            "LensModel": "FE 35mm F1.4 GM",
            "ShutterCount": 4120
        }]"#;
        let got = parse_json(json).unwrap();
        assert_eq!(got.model.as_deref(), Some("ILCE-7RM6"));
        assert_eq!(got.serial.as_deref(), Some("03506694"));
        assert_eq!(got.shutter_count, Some(4120));
        assert_eq!(got.internal_serial, None);
    }

    #[test]
    fn falls_back_to_other_shutter_counts_and_numeric_serials() {
        let json = br#"[{"SerialNumber": 3506694, "ShutterCount3": "812"}]"#;
        let got = parse_json(json).unwrap();
        assert_eq!(got.serial.as_deref(), Some("3506694"));
        assert_eq!(got.shutter_count, Some(812));
    }

    #[test]
    fn parses_a_batch_and_skips_photos_without_a_count_or_date() {
        let json = br#"[
            {"SourceFile": "a.ARW", "Model": "ILCE-7RM6", "SerialNumber": "03506694",
             "Software": "ILCE-7RM6 v1.01", "ShutterCount": 699, "DateTimeOriginal": "2026:11:06 17:21:17"},
            {"SourceFile": "b.ARW", "Model": "ILCE-7RM6", "ShutterCount": 700},
            {"SourceFile": "c.JPG", "Model": "ILCE-7RM6", "DateTimeOriginal": "2026:11:06 17:22:00"},
            {"SourceFile": "d.HIF", "Model": "ILCE-1", "ShutterCount3": "12", "DateTimeOriginal": "2026:11:07 08:00:00.123+09:00"}
        ]"#;
        let samples = parse_shutter_json(json).unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].count, 699);
        assert_eq!(samples[0].firmware.as_deref(), Some("1.01"));
        assert_eq!(samples[0].taken.to_string(), "2026-11-06 17:21:17");
        assert_eq!(samples[1].model, "ILCE-1");
        assert_eq!(samples[1].count, 12);
    }

    #[test]
    fn reads_one_photo_per_shot_preferring_raw() {
        let picked = one_per_shot(
            [
                "a/DSC1.JPG",
                "a/DSC1.ARW",
                "a/DSC2.HIF",
                "a/DSC2.jpg",
                "a/DSC3.JPG",
                "a/DSC4.dng",
                "a/DSC_0005.JPG",
                "a/DSC_0005.NEF",
                "a/x.xmp",
            ]
            .map(PathBuf::from),
        );
        assert_eq!(
            picked,
            ["a/DSC1.ARW", "a/DSC2.HIF", "a/DSC3.JPG", "a/DSC_0005.NEF"].map(PathBuf::from)
        );
    }

    #[test]
    fn a_nikon_z_9_count_is_shutter_count_not_its_zero_mechanical_one() {
        // Cut from a real Z 9 NEF (ExifTool 13.59): the body has no mechanical shutter,
        // so MechanicalShutterCount stays 0 and ShutterCount carries the count.
        let json = br#"[{"SourceFile":"DSC_0020.NEF","Make":"NIKON CORPORATION",
            "Model":"NIKON Z 9","SerialNumber":3005326,"Software":"Ver.01.00",
            "LensModel":"NIKKOR Z 14-30mm f/4 S","ShutterCount":25,
            "MechanicalShutterCount":0,"DateTimeOriginal":"2022:02:05 13:48:11"}]"#;
        let z9 = parse_json(json).unwrap();
        assert_eq!(z9.model.as_deref(), Some("NIKON Z 9"));
        assert_eq!(z9.serial.as_deref(), Some("3005326"));
        assert_eq!(z9.shutter_count, Some(25));
        assert_eq!(z9.electronic_count, None);

        let samples = parse_shutter_json(json).unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].model, "NIKON Z 9");
        assert_eq!(samples[0].count, 25);
        assert_eq!(samples[0].firmware.as_deref(), Some("01.00"));
        assert_eq!(samples[0].taken.to_string(), "2022-02-05 13:48:11");

        // Only a 0 mechanical count and no total: nothing was measured.
        let nothing = parse_json(br#"[{"Model":"NIKON Z 9","MechanicalShutterCount":0}]"#).unwrap();
        assert_eq!(nothing.shutter_count, None);

        // For a body that has a mechanical shutter, its own count comes first and 0 is a
        // valid reading; so does Sony's single count.
        let z8 =
            parse_json(br#"[{"Model":"NIKON Z 8","ShutterCount":900,"MechanicalShutterCount":0}]"#)
                .unwrap();
        assert_eq!(z8.shutter_count, Some(0));
        let sony = parse_json(br#"[{"Model":"ILCE-7RM6","ShutterCount":0}]"#).unwrap();
        assert_eq!(sony.shutter_count, Some(0));
    }

    #[test]
    fn a_nikon_zf_counts_its_mechanical_shutter_and_the_rest_is_electronic() {
        // Cut from a real Zf NEF (ExifTool 13.59): ShutterCount counts every release,
        // MechanicalShutterCount the mechanical shutter's alone.
        let json = br#"[{"SourceFile":"DSC_5546.NEF","Make":"NIKON CORPORATION",
            "Model":"NIKON Z f","SerialNumber":8043077,"Software":"Ver.03.01",
            "LensModel":"NIKKOR Z 24-120mm f/4 S","ShutterCount":25602,
            "MechanicalShutterCount":10045,"DateTimeOriginal":"2026:06:02 10:12:31"}]"#;
        let zf = parse_json(json).unwrap();
        assert_eq!(zf.model.as_deref(), Some("NIKON Z f"));
        assert_eq!(zf.shutter_count, Some(10_045));
        assert_eq!(zf.electronic_count, Some(25_602 - 10_045));
        let samples = parse_shutter_json(json).unwrap();
        assert_eq!(
            (samples[0].count, samples[0].electronic_count),
            (10_045, Some(15_557))
        );
        assert_eq!(samples[0].firmware.as_deref(), Some("03.01"));

        // A mechanical count above the total isn't a pair of counters to subtract.
        let odd = parse_json(
            br#"[{"Make":"NIKON CORPORATION","Model":"NIKON Z f","ShutterCount":5,"MechanicalShutterCount":9}]"#,
        )
        .unwrap();
        assert_eq!((odd.shutter_count, odd.electronic_count), (Some(9), None));
        // Only Nikon's ShutterCount is documented as counting every release.
        let other = parse_json(
            br#"[{"Make":"OTHER","Model":"X1","ShutterCount":50,"MechanicalShutterCount":20}]"#,
        )
        .unwrap();
        assert_eq!(other.electronic_count, None);
    }

    #[test]
    fn firmware_version_drops_the_model_name() {
        assert_eq!(firmware_version("ILCE-7RM6 v1.01"), "1.01");
        assert_eq!(firmware_version("2.00"), "2.00");
        // Nikon writes "Ver.05.30" where Sony writes "v1.01".
        assert_eq!(firmware_version("Ver.05.30"), "05.30");
        assert_eq!(firmware_version("NIKON Z 9 Ver.05.30"), "05.30");
    }

    #[test]
    fn empty_output_is_an_error() {
        assert!(parse_json(b"[]").is_err());
    }
}
