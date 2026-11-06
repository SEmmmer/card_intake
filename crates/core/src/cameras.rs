//! Camera models: the name on the body, and the shutter life the maker rates it for.
//!
//! Only ratings the maker itself published are listed. Sony gives one for few bodies
//! (for most current models it publishes none), and a model without an official
//! rating shows no rated life rather than a guess.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ModelInfo {
    /// The model as EXIF reports it, e.g. "ILCE-1".
    pub model: &'static str,
    /// The name on the body, e.g. "α1".
    pub name: &'static str,
    /// Mechanical shutter cycles the maker rates the body for.
    pub rated_shutter: Option<u64>,
    /// Where the rating comes from.
    pub rating_source: Option<&'static str>,
    pub shutter_reference: Option<u64>,
}

const fn named(model: &'static str, name: &'static str) -> ModelInfo {
    ModelInfo {
        model,
        name,
        rated_shutter: None,
        rating_source: None,
        shutter_reference: None,
    }
}

const fn rated(
    model: &'static str,
    name: &'static str,
    cycles: u64,
    source: &'static str,
) -> ModelInfo {
    ModelInfo {
        model,
        name,
        rated_shutter: Some(cycles),
        rating_source: Some(source),
        shutter_reference: None,
    }
}

const MODELS: &[ModelInfo] = &[
    rated(
        "ILCE-1",
        "α1",
        500_000,
        "Sony: shutter durability of 500,000 cycles (α1 product information, 2021)",
    ),
    named("ILCE-1M2", "α1 II"),
    named("ILCE-9", "α9"),
    rated(
        "ILCE-9M2",
        "α9 II",
        500_000,
        "Sony: shutter tested for durability in excess of 500,000 cycles (α9 II press release, October 2019)",
    ),
    named("ILCE-9M3", "α9 III"),
    ModelInfo {
        shutter_reference: Some(500_000),
        // User-suggested reference. No 500,000-cycle rating found in Sony's
        // ILCE-7RM6 specifications/features (checked 2026-11-07).
        ..named("ILCE-7RM6", "α7R VI")
    },
    named("ILCE-7RM5", "α7R V"),
    named("ILCE-7RM4", "α7R IV"),
    named("ILCE-7RM4A", "α7R IVA"),
    named("ILCE-7RM3", "α7R III"),
    named("ILCE-7RM3A", "α7R IIIA"),
    named("ILCE-7M5", "α7 V"),
    named("ILCE-7M4", "α7 IV"),
    named("ILCE-7M3", "α7 III"),
    named("ILCE-7SM3", "α7S III"),
    named("ILCE-7CR", "α7CR"),
    named("ILCE-7CM2", "α7C II"),
    named("ILCE-7C", "α7C"),
    named("ILCE-6700", "α6700"),
    named("ILCE-6600", "α6600"),
    named("ILCE-6400", "α6400"),
];

pub fn model_info(model: &str) -> Option<&'static ModelInfo> {
    MODELS
        .iter()
        .find(|m| m.model.eq_ignore_ascii_case(model.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_up_names_and_official_ratings_only() {
        let a1 = model_info("ILCE-1").unwrap();
        assert_eq!((a1.name, a1.rated_shutter), ("α1", Some(500_000)));
        let a7r6 = model_info("ilce-7rm6 ").unwrap();
        assert_eq!((a7r6.name, a7r6.rated_shutter), ("α7R VI", None));
        assert_eq!(a7r6.shutter_reference, Some(500_000));
        assert!(model_info("NIKON Z 8").is_none());
        assert!(
            MODELS
                .iter()
                .all(|m| m.rated_shutter.is_some() == m.rating_source.is_some())
        );
    }
}
