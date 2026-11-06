//! The cards and cameras this app has seen, kept between runs as one JSON file.

use crate::health::CardHealth;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    pub cards: Vec<CardRecord>,
    pub cameras: Vec<CameraRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardRecord {
    /// The volume's ID. Formatting the card gives it a new one.
    pub id: String,
    /// A name the user gives the card, e.g. "A1-1". Not editable yet.
    pub name: Option<String>,
    pub capacity_bytes: Option<u64>,
    pub filesystem: Option<String>,
    pub reader: Option<String>,
    pub first_seen: NaiveDateTime,
    pub last_seen: NaiveDateTime,
    /// How many times the card was inserted while the app was running.
    pub insertions: u32,
    /// The card's own serial number, from its health log. Survives formatting.
    #[serde(default)]
    pub hardware_serial: Option<String>,
    /// Health log readings, oldest first.
    #[serde(default)]
    pub health: Vec<HealthReading>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthReading {
    pub at: NaiveDateTime,
    pub health: CardHealth,
}

/// Health readings kept per card; older ones are dropped.
const MAX_HEALTH_READINGS: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraRecord {
    pub model: String,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub first_seen: NaiveDateTime,
    pub last_seen: NaiveDateTime,
    /// Shutter count readings, oldest first. A reading is added only when the count changes.
    pub shutter_counts: Vec<ShutterReading>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShutterReading {
    pub at: NaiveDateTime,
    pub count: u64,
}

/// What the app learned about a card when it was inserted.
#[derive(Debug, Clone, PartialEq)]
pub struct CardSighting {
    pub id: String,
    pub capacity_bytes: Option<u64>,
    pub filesystem: Option<String>,
    pub reader: Option<String>,
}

/// What the app learned about a camera from the newest photo on a card.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraSighting {
    pub model: String,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub shutter_count: Option<u64>,
}

impl Ledger {
    /// Loads the ledger, or starts an empty one if the file doesn't exist yet.
    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Saves by writing a temporary file and renaming it, so a crash never leaves half a file.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, path)
    }

    pub fn record_card(&mut self, seen: CardSighting, now: NaiveDateTime) {
        match self.cards.iter_mut().find(|c| c.id == seen.id) {
            Some(card) => {
                card.last_seen = now;
                card.insertions += 1;
                card.capacity_bytes = seen.capacity_bytes.or(card.capacity_bytes);
                card.filesystem = seen.filesystem.or(card.filesystem.take());
                card.reader = seen.reader.or(card.reader.take());
            }
            None => self.cards.push(CardRecord {
                id: seen.id,
                name: None,
                capacity_bytes: seen.capacity_bytes,
                filesystem: seen.filesystem,
                reader: seen.reader,
                first_seen: now,
                last_seen: now,
                insertions: 1,
                hardware_serial: None,
                health: Vec::new(),
            }),
        }
        self.cards.sort_by_key(|c| std::cmp::Reverse(c.last_seen));
    }

    /// Adds a health reading to a known card. Returns false if the card isn't known.
    pub fn record_health(&mut self, card_id: &str, health: CardHealth, now: NaiveDateTime) -> bool {
        let Some(card) = self.cards.iter_mut().find(|c| c.id == card_id) else {
            return false;
        };
        if health.serial.is_some() {
            card.hardware_serial = health.serial.clone();
        }
        card.health.push(HealthReading { at: now, health });
        if card.health.len() > MAX_HEALTH_READINGS {
            card.health.remove(0);
        }
        true
    }

    pub fn record_camera(&mut self, seen: CameraSighting, now: NaiveDateTime) {
        let existing = self
            .cameras
            .iter_mut()
            .find(|c| c.model == seen.model && c.serial == seen.serial);
        let camera = match existing {
            Some(camera) => {
                camera.last_seen = now;
                camera.firmware = seen.firmware.or(camera.firmware.take());
                camera
            }
            None => {
                self.cameras.push(CameraRecord {
                    model: seen.model,
                    serial: seen.serial,
                    firmware: seen.firmware,
                    first_seen: now,
                    last_seen: now,
                    shutter_counts: Vec::new(),
                });
                self.cameras.last_mut().unwrap()
            }
        };
        if let Some(count) = seen.shutter_count
            && camera.shutter_counts.last().map(|r| r.count) != Some(count)
        {
            camera
                .shutter_counts
                .push(ShutterReading { at: now, count });
        }
        self.cameras.sort_by_key(|c| std::cmp::Reverse(c.last_seen));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    fn card(id: &str) -> CardSighting {
        CardSighting {
            id: id.into(),
            capacity_bytes: Some(240_000_000_000),
            filesystem: Some("ExFAT".into()),
            reader: Some("MRW-G3".into()),
        }
    }

    fn camera(count: Option<u64>) -> CameraSighting {
        CameraSighting {
            model: "ILCE-7RM6".into(),
            serial: Some("03506694".into()),
            firmware: Some("1.00".into()),
            shutter_count: count,
        }
    }

    #[test]
    fn counts_insertions_and_keeps_newest_card_first() {
        let mut ledger = Ledger::default();
        ledger.record_card(card("A"), at("2026-11-01 10:00"));
        ledger.record_card(card("B"), at("2026-11-02 10:00"));
        ledger.record_card(card("A"), at("2026-11-03 10:00"));

        assert_eq!(ledger.cards.len(), 2);
        assert_eq!(ledger.cards[0].id, "A");
        assert_eq!(ledger.cards[0].insertions, 2);
        assert_eq!(ledger.cards[0].first_seen, at("2026-11-01 10:00"));
    }

    #[test]
    fn adds_shutter_readings_only_when_the_count_changes() {
        let mut ledger = Ledger::default();
        ledger.record_camera(camera(Some(100)), at("2026-11-01 10:00"));
        ledger.record_camera(camera(Some(100)), at("2026-11-02 10:00"));
        ledger.record_camera(camera(None), at("2026-11-03 10:00"));
        ledger.record_camera(camera(Some(250)), at("2026-11-04 10:00"));

        assert_eq!(ledger.cameras.len(), 1);
        let counts: Vec<_> = ledger.cameras[0]
            .shutter_counts
            .iter()
            .map(|r| r.count)
            .collect();
        assert_eq!(counts, [100, 250]);
        assert_eq!(ledger.cameras[0].last_seen, at("2026-11-04 10:00"));
    }

    #[test]
    fn saves_and_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/ledger.json");
        assert_eq!(Ledger::load(&path).unwrap(), Ledger::default());

        let mut ledger = Ledger::default();
        ledger.record_card(card("A"), at("2026-11-01 10:00"));
        ledger.record_camera(camera(Some(100)), at("2026-11-01 10:00"));
        ledger.save(&path).unwrap();

        assert_eq!(Ledger::load(&path).unwrap(), ledger);
    }

    #[test]
    fn records_health_and_the_cards_own_serial() {
        let mut ledger = Ledger::default();
        let health = CardHealth {
            serial: Some("ABC123".into()),
            percentage_used: Some(2),
            ..CardHealth::default()
        };
        assert!(!ledger.record_health("A", health.clone(), at("2026-11-01 10:00")));

        ledger.record_card(card("A"), at("2026-11-01 10:00"));
        assert!(ledger.record_health("A", health, at("2026-11-01 10:05")));
        assert_eq!(ledger.cards[0].hardware_serial.as_deref(), Some("ABC123"));
        assert_eq!(ledger.cards[0].health.len(), 1);
    }

    #[test]
    fn loads_a_ledger_saved_before_health_existed() {
        let old = r#"{"cards":[{"id":"A","name":null,"capacity_bytes":1,"filesystem":null,
            "reader":null,"first_seen":"2026-11-06T22:49:40","last_seen":"2026-11-06T22:49:40",
            "insertions":1}],"cameras":[]}"#;
        let ledger: Ledger = serde_json::from_str(old).unwrap();
        assert!(ledger.cards[0].health.is_empty());
        assert_eq!(ledger.cards[0].hardware_serial, None);
    }
}
