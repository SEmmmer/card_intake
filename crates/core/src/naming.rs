//! Shoot folder names: `2026/2026-11-02_Yosemite`.

use chrono::{Datelike, NaiveDate};
use std::path::PathBuf;

/// Characters that Windows or macOS don't allow in folder names.
const FORBIDDEN: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

/// Turns what the user typed into a safe folder-name part.
///
/// `"Mono Lake  birds"` becomes `"Mono-Lake-birds"`. Leading and trailing dots are
/// dropped: Windows rejects a trailing dot, and a leading dot hides the folder on macOS.
pub fn clean_shoot_name(raw: &str) -> String {
    let kept: String = raw
        .chars()
        .filter(|c| !FORBIDDEN.contains(c) && !c.is_control())
        .collect();
    kept.split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
        .trim_matches('.')
        .to_string()
}

/// `"2026-11-02_Yosemite"`, or just `"2026-11-02"` when the name is empty.
pub fn shoot_folder_name(date: NaiveDate, name: &str) -> String {
    let name = clean_shoot_name(name);
    let date = date.format("%Y-%m-%d");
    if name.is_empty() {
        date.to_string()
    } else {
        format!("{date}_{name}")
    }
}

/// A shoot's folder relative to the photo share, e.g. `2026/2026-11-02_Yosemite`.
///
/// Records store this relative path so the same history works on Windows
/// (`\\NAS\photos`) and macOS (`/Volumes/photos`).
pub fn shoot_dir(date: NaiveDate, name: &str) -> PathBuf {
    PathBuf::from(date.year().to_string()).join(shoot_folder_name(date, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nov2() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 2).unwrap()
    }

    #[test]
    fn cleans_spaces_and_forbidden_characters() {
        assert_eq!(clean_shoot_name("Mono Lake  birds"), "Mono-Lake-birds");
        assert_eq!(clean_shoot_name("  a/b:c*d  "), "abcd");
        assert_eq!(clean_shoot_name("Point Reyes."), "Point-Reyes");
        assert_eq!(clean_shoot_name(".hidden"), "hidden");
        assert_eq!(clean_shoot_name("   "), "");
    }

    #[test]
    fn folder_name_with_and_without_a_name() {
        assert_eq!(shoot_folder_name(nov2(), "Yosemite"), "2026-11-02_Yosemite");
        assert_eq!(shoot_folder_name(nov2(), ""), "2026-11-02");
        assert_eq!(shoot_folder_name(nov2(), "???"), "2026-11-02");
    }

    #[test]
    fn shoot_dir_is_year_then_folder() {
        assert_eq!(
            shoot_dir(nov2(), "Yosemite"),
            PathBuf::from("2026").join("2026-11-02_Yosemite")
        );
    }
}
