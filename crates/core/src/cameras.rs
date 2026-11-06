//! Camera models: the name on the body, and the shutter life the maker rates it for.
//!
//! Only ratings the maker itself published are listed. Sony gives one for few bodies
//! (for most current models it publishes none), and a model without an official
//! rating shows no rated life rather than a guess. Other makers' bodies are listed when
//! the app has to know something specific about them, such as Nikon's Z 9, which has no
//! mechanical shutter at all.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ModelInfo {
    /// The model as EXIF reports it, e.g. "ILCE-1" or "NIKON Z 9".
    pub model: &'static str,
    /// The name on the body, e.g. "α1" or "Z 9".
    pub name: &'static str,
    /// Mechanical shutter cycles the maker rates the body for.
    pub rated_shutter: Option<u64>,
    /// Where the rating comes from.
    pub rating_source: Option<&'static str>,
    pub shutter_reference: Option<u64>,
    /// False for a body with no mechanical shutter (the Nikon Z 9). It writes 0 in
    /// `MechanicalShutterCount` on every photo; the count it does keep is in
    /// `ShutterCount`.
    pub has_mechanical_shutter: bool,
}

const fn named(model: &'static str, name: &'static str) -> ModelInfo {
    ModelInfo {
        model,
        name,
        rated_shutter: None,
        rating_source: None,
        shutter_reference: None,
        has_mechanical_shutter: true,
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
        has_mechanical_shutter: true,
    }
}

/// A body whose shutter is only electronic: the maker fits no mechanical shutter, so
/// there is nothing to rate and no mechanical count to prefer over the total count.
const fn electronic_only(model: &'static str, name: &'static str) -> ModelInfo {
    ModelInfo {
        has_mechanical_shutter: false,
        ..named(model, name)
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
    // Nikon: a Z 9 card is read like any other CFexpress card. The body itself has no
    // mechanical shutter, so the count it writes is the electronic one.
    electronic_only("NIKON Z 9", "Z 9"),
    // DJI writes the model number, PP-101, in EXIF ("Model PP-101" in DJI's Osmo
    // Pocket 3 safety guidelines). A gimbal camera with a 1-inch sensor and no
    // mechanical shutter; its photos carry no shutter count.
    electronic_only("PP-101", "Osmo Pocket 3"),
];

pub fn model_info(model: &str) -> Option<&'static ModelInfo> {
    MODELS
        .iter()
        .find(|m| m.model.eq_ignore_ascii_case(model.trim()))
}

/// Whether the body has a mechanical shutter. An unknown model is assumed to have one:
/// only bodies known to lack it are listed above.
pub fn has_mechanical_shutter(model: &str) -> bool {
    model_info(model).is_none_or(|m| m.has_mechanical_shutter)
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
        assert!(model_info("CANON EOS R5").is_none());
        assert!(
            MODELS
                .iter()
                .all(|m| m.rated_shutter.is_some() == m.rating_source.is_some())
        );
    }

    #[test]
    fn the_z_9_is_known_to_have_no_mechanical_shutter() {
        let z9 = model_info("NIKON Z 9").unwrap();
        assert_eq!(z9.name, "Z 9");
        // Nikon publishes no shutter rating for a shutter it doesn't fit.
        assert_eq!((z9.rated_shutter, z9.rating_source), (None, None));
        assert!(!z9.has_mechanical_shutter);
        assert!(!has_mechanical_shutter("nikon z 9"));
        // Bodies the app knows nothing about keep the usual assumption.
        assert!(has_mechanical_shutter("NIKON Z 8"));
        assert!(has_mechanical_shutter("ILCE-7RM6"));
    }

    #[test]
    fn a_dji_osmo_pocket_3_is_named_and_has_no_mechanical_shutter() {
        let pocket = model_info("PP-101").unwrap();
        assert_eq!(pocket.name, "Osmo Pocket 3");
        assert!(!pocket.has_mechanical_shutter);
    }
}
