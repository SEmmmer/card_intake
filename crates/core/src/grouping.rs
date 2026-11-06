//! Splits a card's files into shoots.

use crate::scan::{MediaFile, MediaKind};
use chrono::{NaiveDate, NaiveDateTime};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShootGroup {
    pub date: NaiveDate,
    pub first: NaiveDateTime,
    pub last: NaiveDateTime,
    pub photos: usize,
    pub photo_bytes: u64,
    pub videos: usize,
    pub video_bytes: u64,
}

/// One shoot per capture date, oldest first.
///
/// A shoot that runs past midnight is split in two. Grouping by time gaps
/// (say, a new shoot after 4 hours without a frame) can replace this later.
pub fn group_by_date(files: &[MediaFile]) -> Vec<ShootGroup> {
    let mut groups: BTreeMap<NaiveDate, ShootGroup> = BTreeMap::new();
    for file in files {
        let date = file.captured.date();
        let group = groups.entry(date).or_insert_with(|| ShootGroup {
            date,
            first: file.captured,
            last: file.captured,
            photos: 0,
            photo_bytes: 0,
            videos: 0,
            video_bytes: 0,
        });
        group.first = group.first.min(file.captured);
        group.last = group.last.max(file.captured);
        match file.kind {
            MediaKind::Photo => {
                group.photos += 1;
                group.photo_bytes += file.size;
            }
            MediaKind::Video => {
                group.videos += 1;
                group.video_bytes += file.size;
            }
        }
    }
    groups.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::TimeSource;
    use std::path::PathBuf;

    fn file(at: &str, kind: MediaKind, size: u64) -> MediaFile {
        MediaFile {
            path: PathBuf::from("x"),
            size,
            captured: NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M").unwrap(),
            time_source: TimeSource::Exif,
            kind,
        }
    }

    #[test]
    fn groups_by_day_with_counts_and_time_range() {
        let files = [
            file("2026-11-03 05:58", MediaKind::Photo, 30),
            file("2026-11-02 18:12", MediaKind::Photo, 20),
            file("2026-11-02 06:41", MediaKind::Photo, 10),
            file("2026-11-02 12:00", MediaKind::Video, 100),
        ];
        let groups = group_by_date(&files);

        assert_eq!(groups.len(), 2);
        let nov2 = &groups[0];
        assert_eq!(nov2.date.to_string(), "2026-11-02");
        assert_eq!(nov2.first.format("%H:%M").to_string(), "06:41");
        assert_eq!(nov2.last.format("%H:%M").to_string(), "18:12");
        assert_eq!((nov2.photos, nov2.photo_bytes), (2, 30));
        assert_eq!((nov2.videos, nov2.video_bytes), (1, 100));
        assert_eq!(groups[1].photos, 1);
    }

    #[test]
    fn no_files_no_groups() {
        assert!(group_by_date(&[]).is_empty());
    }
}
