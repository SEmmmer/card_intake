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
    #[serde(default)]
    pub imports: Vec<crate::import::ImportResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardRecord {
    /// The record's key. For a card whose own serial number is known it's
    /// `sn:<serial>`, which survives formatting; otherwise the volume ID the card had
    /// when it was first seen.
    pub id: String,
    /// The volume ID the card has now. Formatting gives it a new one. Ledgers from
    /// before this field existed keep it in `id`.
    #[serde(default)]
    pub volume_id: Option<String>,
    /// A name the user gives the card, e.g. "A1-1". Not editable yet.
    pub name: Option<String>,
    /// The card's model, e.g. "CEA-G240T", when the reader shows the card itself.
    #[serde(default)]
    pub model: Option<String>,
    /// How the card was connected the last time: "NVMe", "USB", "SD"...
    #[serde(default)]
    pub bus: Option<String>,
    /// "SDHC", "SDXC"... for an SD card, from its size.
    #[serde(default)]
    pub sd_class: Option<String>,
    /// Formats noticed: the same card (by serial number) came back with a new volume ID.
    #[serde(default)]
    pub formats: u32,
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

impl CardRecord {
    pub fn current_volume_id(&self) -> &str {
        self.volume_id.as_deref().unwrap_or(&self.id)
    }
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
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CardSighting {
    /// The volume ID, which formatting changes.
    pub volume_id: Option<String>,
    /// The card's own serial number, which formatting doesn't change.
    pub hardware_serial: Option<String>,
    pub model: Option<String>,
    pub bus: Option<String>,
    pub sd_class: Option<String>,
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
    pub fn record_import(&mut self, result: crate::import::ImportResult) {
        self.imports.insert(0, result);
        self.imports.truncate(100);
    }

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

    /// The record a card belongs to: by its own serial number when known, otherwise
    /// by its current volume ID (unless that record belongs to a card with another serial).
    fn find_card(&self, seen: &CardSighting) -> Option<usize> {
        if let Some(serial) = &seen.hardware_serial
            && let Some(i) = self
                .cards
                .iter()
                .position(|c| c.hardware_serial.as_ref() == Some(serial))
        {
            return Some(i);
        }
        let volume = seen.volume_id.as_deref()?;
        self.cards.iter().position(|c| {
            c.current_volume_id() == volume
                && (c.hardware_serial.is_none() || seen.hardware_serial.is_none())
        })
    }

    /// Remembers a card and returns its record's key, or `None` when there's nothing
    /// to recognise it by (no serial number and no volume ID).
    pub fn record_card(&mut self, seen: CardSighting, now: NaiveDateTime) -> Option<String> {
        let key = match self.find_card(&seen) {
            Some(i) => {
                let card = &mut self.cards[i];
                if let Some(volume) = seen.volume_id {
                    // Only a card recognised by its serial can come back with a new
                    // volume ID: it was formatted.
                    if card.current_volume_id() != volume {
                        card.formats += 1;
                    }
                    card.volume_id = Some(volume);
                }
                card.last_seen = now;
                card.insertions += 1;
                card.hardware_serial = seen.hardware_serial.or(card.hardware_serial.take());
                card.model = seen.model.or(card.model.take());
                card.bus = seen.bus.or(card.bus.take());
                card.sd_class = seen.sd_class.or(card.sd_class.take());
                card.capacity_bytes = seen.capacity_bytes.or(card.capacity_bytes);
                card.filesystem = seen.filesystem.or(card.filesystem.take());
                card.reader = seen.reader.or(card.reader.take());
                card.id.clone()
            }
            None => {
                let id = match (&seen.hardware_serial, &seen.volume_id) {
                    (Some(serial), _) => format!("sn:{serial}"),
                    (None, Some(volume)) => volume.clone(),
                    (None, None) => return None,
                };
                self.cards.push(CardRecord {
                    id: id.clone(),
                    volume_id: seen.volume_id,
                    name: None,
                    model: seen.model,
                    bus: seen.bus,
                    sd_class: seen.sd_class,
                    formats: 0,
                    capacity_bytes: seen.capacity_bytes,
                    filesystem: seen.filesystem,
                    reader: seen.reader,
                    first_seen: now,
                    last_seen: now,
                    insertions: 1,
                    hardware_serial: seen.hardware_serial,
                    health: Vec::new(),
                });
                id
            }
        };
        self.cards.sort_by_key(|c| std::cmp::Reverse(c.last_seen));
        Some(key)
    }

    /// The key of the record this card belongs to, without recording anything.
    pub fn card_key(&self, seen: &CardSighting) -> Option<String> {
        self.find_card(seen).map(|i| self.cards[i].id.clone())
    }

    /// The key of the card that has this volume ID now.
    pub fn card_key_for_volume(&self, volume_id: &str) -> Option<String> {
        self.cards
            .iter()
            .find(|c| c.current_volume_id() == volume_id)
            .map(|c| c.id.clone())
    }

    /// Adds a health reading to a known card. Returns false if the card isn't known.
    pub fn record_health(
        &mut self,
        card_key: &str,
        health: CardHealth,
        now: NaiveDateTime,
    ) -> bool {
        let Some(card) = self.cards.iter_mut().find(|c| c.id == card_key) else {
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

    /// Adds a health reading to the card that has `volume_id` now, which learns the
    /// card's own serial number and model from it. Behind a USB bridge the serial isn't
    /// known on insertion, so a card formatted since it was last read comes in as a new
    /// record; when the ledger already knows its serial, that record folds into the
    /// card's own, and the format is counted. Returns the card's key, or `None` when the
    /// card isn't known.
    pub fn record_reading(
        &mut self,
        volume_id: Option<&str>,
        health: CardHealth,
        now: NaiveDateTime,
    ) -> Option<String> {
        let serial = health.serial.clone();
        let by_serial = serial.as_ref().and_then(|s| {
            self.cards
                .iter()
                .position(|c| c.hardware_serial.as_ref() == Some(s))
        });
        let by_volume = volume_id.and_then(|volume| {
            self.cards.iter().position(|c| {
                c.current_volume_id() == volume
                    && (c.hardware_serial.is_none() || c.hardware_serial == serial)
            })
        });
        let index = match (by_serial, by_volume) {
            (Some(card), Some(sighting)) if card != sighting => self.fold_into(sighting, card),
            (Some(i), _) | (None, Some(i)) => i,
            (None, None) => return None,
        };
        let card = &mut self.cards[index];
        card.model = health.model.clone().or(card.model.take());
        let key = card.id.clone();
        self.record_health(&key, health, now);
        self.cards.sort_by_key(|c| std::cmp::Reverse(c.last_seen));
        Some(key)
    }

    /// Folds the record at `from` (the card as inserted, known by its volume ID) into the
    /// record at `into` (the same card, known by its serial number). Returns the new
    /// index of `into`.
    fn fold_into(&mut self, from: usize, into: usize) -> usize {
        let seen = self.cards.remove(from);
        let into = if from < into { into - 1 } else { into };
        let card = &mut self.cards[into];
        if card.current_volume_id() != seen.current_volume_id() {
            card.formats += 1;
        }
        card.volume_id = Some(seen.current_volume_id().to_string());
        card.insertions += seen.insertions;
        card.first_seen = card.first_seen.min(seen.first_seen);
        card.last_seen = card.last_seen.max(seen.last_seen);
        card.name = card.name.take().or(seen.name);
        // The newer sighting's connection and file system win.
        card.bus = seen.bus.or(card.bus.take());
        card.sd_class = seen.sd_class.or(card.sd_class.take());
        card.capacity_bytes = seen.capacity_bytes.or(card.capacity_bytes);
        card.filesystem = seen.filesystem.or(card.filesystem.take());
        card.reader = seen.reader.or(card.reader.take());
        card.health.extend(seen.health);
        card.health.sort_by_key(|r| r.at);
        let excess = card.health.len().saturating_sub(MAX_HEALTH_READINGS);
        card.health.drain(..excess);
        // Imports from the card as inserted belong to the card.
        let key = card.id.clone();
        for import in &mut self.imports {
            if import.card_key.as_deref() == Some(seen.id.as_str()) {
                import.card_key = Some(key.clone());
            }
        }
        into
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
            volume_id: Some(id.into()),
            capacity_bytes: Some(240_000_000_000),
            filesystem: Some("ExFAT".into()),
            reader: Some("MRW-G3".into()),
            ..CardSighting::default()
        }
    }

    fn cfexpress(volume: &str, serial: &str) -> CardSighting {
        CardSighting {
            hardware_serial: Some(serial.into()),
            model: Some("CEA-G240T".into()),
            bus: Some("NVMe".into()),
            ..card(volume)
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
    fn a_card_known_by_its_serial_survives_formatting() {
        let mut ledger = Ledger::default();
        let key = ledger
            .record_card(cfexpress("E2F0-8E6E", "BF7F0763"), at("2026-11-01 10:00"))
            .unwrap();
        assert_eq!(key, "sn:BF7F0763");
        // Formatted in the camera: new volume ID, same card.
        let again = ledger
            .record_card(cfexpress("1234-ABCD", "BF7F0763"), at("2026-11-02 10:00"))
            .unwrap();
        assert_eq!(again, key);
        assert_eq!(ledger.cards.len(), 1);
        assert_eq!(ledger.cards[0].formats, 1);
        assert_eq!(ledger.cards[0].volume_id.as_deref(), Some("1234-ABCD"));
        assert_eq!(
            ledger.card_key_for_volume("1234-ABCD").as_deref(),
            Some(key.as_str())
        );
        assert_eq!(ledger.card_key_for_volume("E2F0-8E6E"), None);
        // A second card of the same model is a different record, even if a volume ID
        // happens to repeat.
        ledger.record_card(cfexpress("1234-ABCD", "OTHER"), at("2026-11-03 10:00"));
        assert_eq!(ledger.cards.len(), 2);
    }

    #[test]
    fn an_old_record_gains_the_serial_and_a_card_without_any_id_is_not_recorded() {
        let mut ledger = Ledger::default();
        let old = ledger
            .record_card(card("E2F0-8E6E"), at("2026-11-01 10:00"))
            .unwrap();
        assert_eq!(old, "E2F0-8E6E");
        let key = ledger
            .record_card(cfexpress("E2F0-8E6E", "BF7F0763"), at("2026-11-02 10:00"))
            .unwrap();
        assert_eq!(key, old);
        assert_eq!(ledger.cards[0].hardware_serial.as_deref(), Some("BF7F0763"));
        assert_eq!(ledger.cards[0].formats, 0);
        assert_eq!(
            ledger.record_card(CardSighting::default(), at("2026-11-02 10:00")),
            None
        );
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

    /// A card behind a USB bridge: only its volume ID is known on insertion.
    fn bridged(volume: &str) -> CardSighting {
        CardSighting {
            bus: Some("USB".into()),
            reader: None,
            ..card(volume)
        }
    }

    fn reading(serial: &str, used: u64) -> CardHealth {
        CardHealth {
            serial: Some(serial.into()),
            model: Some("CHIPFANCIER CFE Platinum".into()),
            percentage_used: Some(used),
            bridge: Some("JMicron JMS583".into()),
            ..CardHealth::default()
        }
    }

    fn import_from(card_key: &str) -> crate::import::ImportResult {
        serde_json::from_value(serde_json::json!({
            "started_at": "2026-11-01T10:00:00", "finished_at": "2026-11-01T10:10:00",
            "root": "E:\\", "nas_root": "\\\\NAS\\photos", "card_id": card_key,
            "card_key": card_key, "status": "complete", "total_files": 1, "total_bytes": 1,
            "copied_files": 1, "skipped_files": 0, "verified_files": 1, "completed_bytes": 1,
            "shoots": [], "error": null, "warnings": [], "ejected": false
        }))
        .unwrap()
    }

    #[test]
    fn a_reading_through_a_bridge_learns_the_cards_serial_and_model() {
        let mut ledger = Ledger::default();
        let key = ledger
            .record_card(bridged("1111-AAAA"), at("2026-11-01 10:00"))
            .unwrap();
        assert_eq!(
            ledger.record_reading(
                Some("1111-AAAA"),
                reading("P640", 0),
                at("2026-11-01 10:01")
            ),
            Some(key.clone())
        );
        let card = &ledger.cards[0];
        assert_eq!(card.hardware_serial.as_deref(), Some("P640"));
        assert_eq!(card.model.as_deref(), Some("CHIPFANCIER CFE Platinum"));
        assert_eq!(card.health.len(), 1);
        // An unknown card gets no reading.
        assert_eq!(
            ledger.record_reading(
                Some("2222-BBBB"),
                reading("OTHER", 0),
                at("2026-11-01 10:02")
            ),
            None
        );
    }

    #[test]
    fn a_formatted_card_read_through_a_bridge_folds_back_into_its_own_record() {
        let mut ledger = Ledger::default();
        let key = ledger
            .record_card(bridged("1111-AAAA"), at("2026-11-01 10:00"))
            .unwrap();
        ledger.record_reading(
            Some("1111-AAAA"),
            reading("P640", 0),
            at("2026-11-01 10:01"),
        );
        // Formatted in the camera: the bridge can't say whose card it is on insertion.
        let as_inserted = ledger
            .record_card(bridged("3333-CCCC"), at("2026-11-05 09:00"))
            .unwrap();
        assert_ne!(as_inserted, key);
        ledger.record_import(import_from(&as_inserted));
        assert_eq!(ledger.cards.len(), 2);

        // Reading its health finds the card's own record by serial.
        assert_eq!(
            ledger.record_reading(
                Some("3333-CCCC"),
                reading("P640", 1),
                at("2026-11-05 09:02")
            ),
            Some(key.clone())
        );
        assert_eq!(ledger.cards.len(), 1);
        let card = &ledger.cards[0];
        assert_eq!(card.formats, 1);
        assert_eq!(card.insertions, 2);
        assert_eq!(card.volume_id.as_deref(), Some("3333-CCCC"));
        assert_eq!(card.first_seen, at("2026-11-01 10:00"));
        assert_eq!(card.last_seen, at("2026-11-05 09:00"));
        let used: Vec<_> = card
            .health
            .iter()
            .map(|r| r.health.percentage_used)
            .collect();
        assert_eq!(used, [Some(0), Some(1)]);
        assert_eq!(ledger.imports[0].card_key.as_deref(), Some(key.as_str()));
        assert_eq!(
            ledger.card_key_for_volume("3333-CCCC").as_deref(),
            Some(key.as_str())
        );
    }

    #[test]
    fn remembers_an_sd_cards_class_and_old_ledgers_load_without_one() {
        let mut ledger = Ledger::default();
        let sd = CardSighting {
            sd_class: Some("SDXC".into()),
            ..card("0B35-ECD9")
        };
        ledger.record_card(sd, at("2026-11-07 05:00"));
        // A later sighting that couldn't tell keeps what was known.
        ledger.record_card(card("0B35-ECD9"), at("2026-11-07 06:00"));
        assert_eq!(ledger.cards[0].sd_class.as_deref(), Some("SDXC"));

        let old = r#"{"cards":[{"id":"A","name":null,"capacity_bytes":1,"filesystem":null,
            "reader":null,"first_seen":"2026-11-06T22:49:40","last_seen":"2026-11-06T22:49:40",
            "insertions":1}],"cameras":[]}"#;
        let ledger: Ledger = serde_json::from_str(old).unwrap();
        assert_eq!(ledger.cards[0].sd_class, None);
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
