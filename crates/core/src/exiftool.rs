//! Runs ExifTool for what only it can decode, such as Sony's shutter count,
//! which sits in an enciphered maker-note block.

use serde::Serialize;
use serde_json::Value;
use std::io;
use std::path::Path;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct CameraDetails {
    pub make: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub internal_serial: Option<String>,
    pub firmware: Option<String>,
    pub lens: Option<String>,
    pub shutter_count: Option<u64>,
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
        .ok_or_else(|| io::Error::other("ExifTool returned no data"))?;

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

    Ok(CameraDetails {
        make: text("Make"),
        model: text("Model"),
        serial: text("SerialNumber"),
        internal_serial: text("InternalSerialNumber"),
        firmware: text("Software"),
        lens: text("LensModel"),
        shutter_count: number("ShutterCount")
            .or_else(|| number("ShutterCount3"))
            .or_else(|| number("ShutterCount2")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn empty_output_is_an_error() {
        assert!(parse_json(b"[]").is_err());
    }
}
