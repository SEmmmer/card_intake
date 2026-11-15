//! Clips are sorted by spec inside a shoot's `video` folder: `video/8k30`, `video/4k60`,
//! `video/1080p60`. A slow-motion clip, shot at one rate and saved at another, names
//! both: `video/4k120-30`. Each clip's Sony XML and proxy go into the same folder.
//!
//! The spec comes from Sony's `<clip>M01.XML`, otherwise from ExifTool. A clip
//! neither can read goes into `video` itself, as every clip did before spec folders.

use crate::{deliver, exiftool, scan};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spec {
    pub width: u32,
    pub height: u32,
    /// Frames per second the camera shot.
    pub capture_fps: f64,
    /// Frames per second the file plays at.
    pub format_fps: f64,
}

impl Spec {
    /// `8k30`, `4k60`, `1080p60`; `4k120-30` for a clip shot at 120 and saved at 30.
    pub fn folder(&self) -> Option<String> {
        let size = size_label(self.width, self.height)?;
        let capture = rate(self.capture_fps)?;
        let format = rate(self.format_fps)?;
        Some(if capture == format {
            format!("{size}{capture}")
        } else {
            format!("{size}{capture}-{format}")
        })
    }
}

/// Whole frames per second: 29.97 is 30, 59.94 is 60, 23.976 is 24, 119.88 is 120.
fn rate(fps: f64) -> Option<u32> {
    (fps.is_finite() && (0.5..10_000.0).contains(&fps)).then(|| fps.round() as u32)
}

/// From 4K up, the long side in thousands (3840 and 4096 are 4k, 7680 is 8k); below
/// that, the short side in lines (1080p, 720p). A portrait clip counts as landscape.
fn size_label(width: u32, height: u32) -> Option<String> {
    let (long, short) = (width.max(height), width.min(height));
    if short == 0 {
        None
    } else if long >= 3840 {
        Some(format!("{}k", (long + 500) / 1000))
    } else {
        Some(format!("{short}p"))
    }
}

/// The spec in a Sony clip's `<clip>M01.XML`.
pub fn sony_spec(clip: &Path) -> Option<Spec> {
    sony_spec_in(&fs::read_to_string(scan::sidecar_path(clip)?).ok()?)
}

/// `<VideoFrame captureFps="119.88p" formatFps="29.97p"/>` and
/// `<VideoLayout pixel="3840" numOfVerticalLine="2160"/>`.
fn sony_spec_in(xml: &str) -> Option<Spec> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let element = |name: &str| doc.descendants().find(|n| n.tag_name().name() == name);
    let frame = element("VideoFrame")?;
    let layout = element("VideoLayout")?;
    let fps = |attr: &str| {
        frame
            .attribute(attr)?
            .trim_end_matches(['p', 'P', 'i', 'I'])
            .parse::<f64>()
            .ok()
    };
    let format_fps = fps("formatFps")?;
    Some(Spec {
        width: layout.attribute("pixel")?.parse().ok()?,
        height: layout.attribute("numOfVerticalLine")?.parse().ok()?,
        capture_fps: fps("captureFps").unwrap_or(format_fps),
        format_fps,
    })
}

/// Specs ExifTool reads from the clips' own headers, by [`deliver::normal_path`].
/// ExifTool gives the rate the file plays at, so it can't tell slow motion apart.
fn exiftool_specs(tool: &Path, clips: &[PathBuf]) -> HashMap<String, Spec> {
    let mut found = HashMap::new();
    for chunk in clips.chunks(64) {
        let Ok(bytes) = exiftool::read_batch(
            tool,
            chunk,
            &["-ImageWidth", "-ImageHeight", "-VideoFrameRate"],
        ) else {
            continue;
        };
        found.extend(parse_exiftool(&bytes));
    }
    found
}

fn parse_exiftool(bytes: &[u8]) -> HashMap<String, Spec> {
    let Ok(Value::Array(rows)) = serde_json::from_slice(bytes) else {
        return HashMap::new();
    };
    let number = |row: &Value, key: &str| {
        let value = row.get(key)?;
        value
            .as_f64()
            .or_else(|| value.as_str()?.trim().parse().ok())
    };
    rows.iter()
        .filter_map(|row| {
            let fps = number(row, "VideoFrameRate")?;
            let spec = Spec {
                width: number(row, "ImageWidth")? as u32,
                height: number(row, "ImageHeight")? as u32,
                capture_fps: fps,
                format_fps: fps,
            };
            spec.folder()?;
            Some((deliver::normal_path(row.get("SourceFile")?.as_str()?), spec))
        })
        .collect()
}

/// Each clip's spec folder (`4k30`). A clip missing from the map goes into `video`.
pub fn spec_folders(clips: &[PathBuf], exiftool: Option<&Path>) -> HashMap<PathBuf, String> {
    let mut folders = HashMap::new();
    let mut unread = Vec::new();
    for clip in clips {
        match sony_spec(clip).and_then(|s| s.folder()) {
            Some(folder) => {
                folders.insert(clip.clone(), folder);
            }
            None => unread.push(clip.clone()),
        }
    }
    if let (Some(tool), false) = (exiftool, unread.is_empty()) {
        let read = exiftool_specs(tool, &unread);
        for clip in unread {
            let key = deliver::normal_path(&clip.to_string_lossy());
            if let Some(folder) = read.get(&key).and_then(Spec::folder) {
                folders.insert(clip, folder);
            }
        }
    }
    folders
}

/// The clip's stem when `stem` is a Sony proxy's, `<clip>S03`: an `S` and two digits
/// after a clip name that ends in a digit (`C0001`, `20261114_EM_0006`).
fn proxy_of(stem: &str) -> Option<&str> {
    let b = stem.as_bytes();
    let n = b.len();
    (n >= 4
        && b[n - 3].eq_ignore_ascii_case(&b'S')
        && b[n - 2].is_ascii_digit()
        && b[n - 1].is_ascii_digit()
        && b[n - 4].is_ascii_digit())
    .then(|| &stem[..n - 3])
}

/// A Sony proxy (`C0001S03.MP4`), copied next to its clip as a companion.
pub fn is_proxy(path: &Path) -> bool {
    proxy_clip_stem(path).is_some()
}

/// The stem of the clip a proxy belongs to: `C0001` for `C0001S03.MP4`.
pub fn proxy_clip_stem(path: &Path) -> Option<&str> {
    if !path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
    {
        return None;
    }
    proxy_of(path.file_stem()?.to_str()?)
}

/// Whether the file `name` is a proxy of the clip whose stem is `clip_stem`.
pub fn is_proxy_of(name: &str, clip_stem: &str) -> bool {
    proxy_clip_stem(Path::new(name)).is_some_and(|s| s.eq_ignore_ascii_case(clip_stem))
}

/// Where a Sony card keeps a clip's proxies: `M4ROOT/SUB` beside `M4ROOT/CLIP`.
pub fn card_proxy_folder(clip: &Path) -> Option<PathBuf> {
    let folder = clip.parent()?;
    folder
        .file_name()?
        .eq_ignore_ascii_case("CLIP")
        .then(|| folder.parent().map(|m4root| m4root.join("SUB")))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(width: u32, height: u32, capture: f64, format: f64) -> Option<String> {
        Spec {
            width,
            height,
            capture_fps: capture,
            format_fps: format,
        }
        .folder()
    }

    #[test]
    fn names_folders_by_size_and_rate() {
        assert_eq!(spec(7680, 4320, 29.97, 29.97).as_deref(), Some("8k30"));
        assert_eq!(spec(3840, 2160, 29.97, 29.97).as_deref(), Some("4k30"));
        assert_eq!(spec(3840, 2160, 59.94, 59.94).as_deref(), Some("4k60"));
        assert_eq!(spec(3840, 2160, 119.88, 119.88).as_deref(), Some("4k120"));
        assert_eq!(spec(4096, 2160, 23.976, 23.976).as_deref(), Some("4k24"));
        assert_eq!(spec(5760, 3240, 25.0, 25.0).as_deref(), Some("6k25"));
        assert_eq!(spec(1920, 1080, 59.94, 59.94).as_deref(), Some("1080p60"));
        assert_eq!(spec(1280, 720, 50.0, 50.0).as_deref(), Some("720p50"));
        // A phone's portrait clip.
        assert_eq!(spec(1080, 1920, 30.0, 30.0).as_deref(), Some("1080p30"));
        assert_eq!(spec(2160, 3840, 30.0, 30.0).as_deref(), Some("4k30"));
        // Slow motion: shot at 120, plays at 30.
        assert_eq!(spec(3840, 2160, 119.88, 29.97).as_deref(), Some("4k120-30"));
        assert_eq!(
            spec(1920, 1080, 239.76, 23.98).as_deref(),
            Some("1080p240-24")
        );
        assert_eq!(spec(0, 2160, 30.0, 30.0), None);
        assert_eq!(spec(3840, 2160, 0.0, 0.0), None);
        assert_eq!(spec(3840, 2160, f64::NAN, 30.0), None);
    }

    /// Cut from 20261114_EM_0015M01.XML (α7R VI, 8K 29.97p).
    const EIGHT_K: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<NonRealTimeMeta xmlns="urn:schemas-professionalDisc:nonRealTimeMeta:ver.2.20" lastUpdate="2026-11-14T14:15:42+08:00">
	<CreationDate value="2026-11-14T14:14:08+08:00"/>
	<VideoFormat>
		<VideoRecPort port="DIRECT"/>
		<VideoFrame videoCodec="HEVC_7680_4320_M42210P@L61HT" captureFps="29.97p" formatFps="29.97p"/>
		<VideoLayout pixel="7680" numOfVerticalLine="4320" aspectRatio="16:9"/>
	</VideoFormat>
	<SubStream codec="AVC_Proxy_1280_720_MP@L4"/>
	<Device manufacturer="Sony" modelName="ILCE-7RM6" serialNo="03506694"/>
</NonRealTimeMeta>"#;

    #[test]
    fn reads_sony_xml() {
        let got = sony_spec_in(EIGHT_K).unwrap();
        assert_eq!((got.width, got.height), (7680, 4320));
        assert_eq!(got.folder().as_deref(), Some("8k30"));
        let slow = EIGHT_K
            .replace("7680", "3840")
            .replace("4320", "2160")
            .replace(r#"captureFps="29.97p""#, r#"captureFps="119.88p""#);
        assert_eq!(
            sony_spec_in(&slow).unwrap().folder().as_deref(),
            Some("4k120-30")
        );
        // No VideoFormat (an older or cut-down sidecar), or not XML at all.
        assert_eq!(
            sony_spec_in(r#"<NonRealTimeMeta><CreationDate value="x"/></NonRealTimeMeta>"#),
            None
        );
        assert_eq!(sony_spec_in("not xml"), None);
    }

    #[test]
    fn reads_exiftool_rows_and_skips_unusable_ones() {
        let json = br#"[
            {"SourceFile":"E:/DCIM/100NCZ_9/DSC_0100.MOV","ImageWidth":3840,"ImageHeight":2160,"VideoFrameRate":59.9400599400599},
            {"SourceFile":"E:/DCIM/100APPLE/IMG_0001.MOV","ImageWidth":"1920","ImageHeight":"1080","VideoFrameRate":"29.97"},
            {"SourceFile":"E:/DCIM/x/broken.mp4","ImageWidth":3840},
            {"SourceFile":"E:/DCIM/x/zero.mp4","ImageWidth":0,"ImageHeight":0,"VideoFrameRate":30}
        ]"#;
        let found = parse_exiftool(json);
        assert_eq!(found.len(), 2);
        assert_eq!(
            found[&deliver::normal_path(r"E:\DCIM\100NCZ_9\DSC_0100.MOV")]
                .folder()
                .as_deref(),
            Some("4k60")
        );
        assert_eq!(
            found["e:/dcim/100apple/img_0001.mov"].folder().as_deref(),
            Some("1080p30")
        );
        assert!(parse_exiftool(b"not json").is_empty());
    }

    #[test]
    fn spec_folders_use_the_xml_and_leave_unreadable_clips_out() {
        let card = tempfile::tempdir().unwrap();
        let clip = card.path().join("M4ROOT/CLIP/20261114_EM_0015.MP4");
        fs::create_dir_all(clip.parent().unwrap()).unwrap();
        fs::write(&clip, b"clip").unwrap();
        fs::write(clip.with_file_name("20261114_EM_0015M01.XML"), EIGHT_K).unwrap();
        let other = card.path().join("DCIM/100NCZ_9/DSC_0100.MOV");
        fs::create_dir_all(other.parent().unwrap()).unwrap();
        fs::write(&other, b"clip").unwrap();
        let folders = spec_folders(&[clip.clone(), other.clone()], None);
        assert_eq!(folders.get(&clip).map(String::as_str), Some("8k30"));
        assert!(!folders.contains_key(&other));
        // A missing ExifTool changes nothing.
        let folders = spec_folders(
            std::slice::from_ref(&other),
            Some(Path::new("missing-exiftool")),
        );
        assert!(folders.is_empty());
    }

    #[test]
    fn recognises_sony_proxies() {
        assert!(is_proxy(Path::new("SUB/20261114_EM_0006S03.MP4")));
        assert!(is_proxy(Path::new("c0001s03.mp4")));
        assert_eq!(
            proxy_clip_stem(Path::new("20261114_EM_0006S03.MP4")),
            Some("20261114_EM_0006")
        );
        assert!(is_proxy_of("C0001S03.MP4", "c0001"));
        assert!(!is_proxy_of("C0001S03.MP4", "C0002"));
        assert!(!is_proxy_of("C0001M01.XML", "C0001"));
        // A clip, its sidecar and look-alikes are not proxies.
        for name in [
            "C0001.MP4",
            "C0001S03.XML",
            "C0001S3.MP4",
            "BUS03.MP4",
            "S03.MP4",
            "C0001T01.JPG",
        ] {
            assert!(!is_proxy(Path::new(name)), "{name}");
        }
        assert_eq!(
            card_proxy_folder(Path::new("E:/M4ROOT/CLIP/C0001.MP4")),
            Some(PathBuf::from("E:/M4ROOT/SUB"))
        );
        assert_eq!(
            card_proxy_folder(Path::new("E:/PRIVATE/M4ROOT/CLIP/C0001.MP4")),
            Some(PathBuf::from("E:/PRIVATE/M4ROOT/SUB"))
        );
        assert_eq!(
            card_proxy_folder(Path::new("E:/DCIM/100NCZ_9/DSC_0100.MOV")),
            None
        );
    }
}
