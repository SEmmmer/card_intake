//! The user's settings, kept as `settings.json` in the app's config folder.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// The photo share, e.g. `/Volumes/photos` or `\\NAS\photos`.
    #[serde(default)]
    pub nas_root: Option<String>,
    #[serde(default)]
    pub lightroom: crate::lightroom::LightroomSettings,
    /// Where Deliver puts finished JPEGs, one folder per shoot, e.g.
    /// `\\NAS\Picture\MyPhoto` (the folder Immich's external library watches).
    #[serde(default)]
    pub deliver_to: Option<String>,
    #[serde(default)]
    pub immich: crate::immich::ImmichSettings,
    /// Cameras and cards shown folded, e.g. `camera:03506694` or `card:E2F0-8E6E`.
    #[serde(default)]
    pub folded: Vec<String>,
    #[serde(default)]
    pub offline_folder: Option<String>,
    #[serde(default)]
    pub backup_drives: Vec<card_intake_core::backup::Drive>,
    #[serde(default)]
    pub not_mine_cards: Vec<NotMineCard>,
    #[serde(default)]
    pub not_mine_cameras: Vec<NotMineCamera>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotMineCard {
    pub key: String,
    pub model: Option<String>,
    pub capacity_bytes: Option<u64>,
    pub sd_class: Option<String>,
    pub marked_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotMineCamera {
    pub model: String,
    pub serial: Option<String>,
    pub internal_serial: Option<String>,
    pub marked_at: chrono::DateTime<chrono::Utc>,
}

impl Settings {
    pub fn not_mine(&self) -> card_intake_core::shutter::MyCameras {
        let mut cameras = card_intake_core::shutter::MyCameras::default();
        for camera in &self.not_mine_cameras {
            cameras.add(
                &camera.model,
                camera.serial.as_deref(),
                camera.internal_serial.as_deref(),
            );
        }
        cameras
    }

    pub fn drop_not_mine(&self, readings: &mut card_intake_core::shutter::ShutterDb) {
        let ignored = self.not_mine();
        readings.cameras.retain(|c| !ignored.owns_camera(c));
    }
}

impl Settings {
    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_string_pretty(self).map_err(io::Error::other)?,
        )?;
        std::fs::rename(tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_loads() {
        let dir = std::env::temp_dir().join(format!("card-intake-settings-{}", std::process::id()));
        let path = dir.join("settings.json");
        assert_eq!(Settings::load(&path).unwrap(), Settings::default());
        let settings = Settings {
            nas_root: Some("/Volumes/photos".into()),
            ..Default::default()
        };
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), settings);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn old_settings_load_without_immich() {
        let s: Settings =
            serde_json::from_str(r#"{"nas_root":"//NAS/photos","lightroom":{}}"#).unwrap();
        assert_eq!(s.immich, crate::immich::ImmichSettings::default());
        assert!(s.not_mine_cards.is_empty() && s.not_mine_cameras.is_empty());
        assert!(s.offline_folder.is_none() && s.backup_drives.is_empty());
    }

    #[test]
    fn not_mine_and_offline_choices_round_trip_without_shared_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let s = Settings {
            not_mine_cards: vec![NotMineCard {
                key: "sn:borrowed".into(),
                model: Some("SD".into()),
                capacity_bytes: Some(256_000_000_000),
                sd_class: Some("SDXC".into()),
                marked_at: chrono::Utc::now(),
            }],
            not_mine_cameras: vec![NotMineCamera {
                model: "ILCE-7M4".into(),
                serial: Some("0451234".into()),
                internal_serial: Some("1 2 3".into()),
                marked_at: chrono::Utc::now(),
            }],
            offline_folder: Some(dir.path().join("Pictures/Offline").display().to_string()),
            backup_drives: vec![card_intake_core::backup::Drive {
                uuid: "a1c8d6e2-fb12-4a34-8b56-123456789abc".into(),
                label: "Trip".into(),
                added_at: chrono::Utc::now(),
            }],
            ..Default::default()
        };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), s);
        assert!(s.not_mine().owns("ILCE-7M4", None, Some("1 2 3")));
        assert!(!s.not_mine().owns("ILCE-7M4", Some("other"), None));
    }

    #[test]
    fn not_mine_readings_are_dropped_before_new_cameras_are_kept() {
        use card_intake_core::shutter::{CameraHistory, MyCameras, ShutterDb};
        let s = Settings {
            not_mine_cameras: vec![NotMineCamera {
                model: "ILCE-7M4".into(),
                serial: Some("borrowed".into()),
                internal_serial: Some("1 2 3".into()),
                marked_at: chrono::Utc::now(),
            }],
            ..Default::default()
        };
        let mut readings = ShutterDb {
            cameras: vec![
                CameraHistory {
                    model: "ILCE-7M4".into(),
                    serial: None,
                    internal_serial: Some("1 2 3".into()),
                    generation: None,
                    days: Vec::new(),
                },
                CameraHistory {
                    model: "ILCE-7M4".into(),
                    serial: Some("new".into()),
                    internal_serial: None,
                    generation: None,
                    days: Vec::new(),
                },
            ],
            ..Default::default()
        };
        s.drop_not_mine(&mut readings);
        let new_cameras = readings.split_off_new(&MyCameras::default());
        assert_eq!(new_cameras.cameras.len(), 1);
        assert_eq!(new_cameras.cameras[0].serial.as_deref(), Some("new"));
    }
}
