//! Patch only our properties; unrelated XML, packet padding and edits stay byte-for-byte.

use crate::rating::{InitialRating, NAMESPACE, RatingSource};
use roxmltree::{Document, Node};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::ops::Range;
use std::path::Path;

const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const XMP: &str = "http://ns.adobe.com/xap/1.0/";
const DC: &str = "http://purl.org/dc/elements/1.1/";
const LR: &str = "http://ns.adobe.com/lightroom/1.0/";
const FIELDS: &[&str] = &[
    "RatingSource",
    "SuggestedRating",
    "ExistingRatingKept",
    "Pick",
    "Version",
    "WrittenAt",
    "ISO",
    "ISODeduction",
    "ISOSkipped",
    "ExposureTime",
    "FocalLength",
    "CropFactor",
    "SafeShutterSpeed",
    "ShutterDeduction",
    "ShutterSkipped",
];
const EMPTY: &str = "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n</rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>\n";

#[derive(Debug)]
pub(crate) struct Written {
    pub rating: i32,
    pub kept: bool,
}

type Edit = (Range<usize>, String);

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn element<'a, 'input>(node: Node<'a, 'input>, ns: &str, name: &str) -> bool {
    node.has_tag_name((ns, name))
}

fn insert_inside(text: &str, node: Node<'_, '_>, addition: String) -> Result<Edit, String> {
    let range = node.range();
    let xml = &text[range.clone()];
    if xml.ends_with("/>") {
        let name = xml[1..]
            .split([' ', '\t', '\n', '\r', '/', '>'])
            .next()
            .ok_or("Missing XML name")?;
        Ok((range.end - 2..range.end, format!(">{addition}</{name}>")))
    } else {
        let end = range.start + xml.rfind("</").ok_or("Missing XML closing tag")?;
        Ok((end..end, addition))
    }
}

fn add_keywords(
    text: &str,
    descriptions: &[Node<'_, '_>],
    ns: &str,
    name: &str,
    wanted: &[String],
    edits: &mut Vec<Edit>,
) -> Result<String, String> {
    let properties: Vec<_> = descriptions
        .iter()
        .flat_map(|d| d.children())
        .filter(|n| element(*n, ns, name))
        .collect();
    if properties.len() > 1
        || descriptions
            .iter()
            .any(|d| d.attribute((ns, name)).is_some())
    {
        return Err(format!("Ambiguous {name} keywords"));
    }
    let mut seen = BTreeSet::new();
    let bag = if let Some(property) = properties.first() {
        let children: Vec<_> = property.children().filter(Node::is_element).collect();
        if children.len() != 1
            || !element(children[0], RDF, "Bag")
            || property.attributes().len() > 0
        {
            return Err(format!("Unsupported {name} keywords"));
        }
        for item in children[0].children().filter(Node::is_element) {
            if !element(item, RDF, "li")
                || item.attributes().len() > 0
                || item.children().any(|n| n.is_element())
            {
                return Err(format!("Unsupported {name} keyword item"));
            }
            seen.insert(item.text().unwrap_or("").to_string());
        }
        Some(children[0])
    } else {
        None
    };
    let mut items = String::new();
    for keyword in wanted {
        if seen.insert(keyword.clone()) {
            items.push_str(&format!(
                "<rdf:li xmlns:rdf=\"{RDF}\">{}</rdf:li>",
                escape(keyword)
            ));
        }
    }
    match bag {
        Some(bag) if !items.is_empty() => {
            edits.push(insert_inside(text, bag, items)?);
            Ok(String::new())
        }
        Some(_) => Ok(String::new()),
        None => Ok(format!(
            "<k:{name} xmlns:k=\"{ns}\"><rdf:Bag>{items}</rdf:Bag></k:{name}>"
        )),
    }
}

fn merge(text: &str, initial: &InitialRating) -> Result<(String, Written), String> {
    // DTDs and external entities are disabled by the parser.
    let doc = Document::parse(text).map_err(|e| format!("Invalid XMP: {e}"))?;
    let roots: Vec<_> = doc
        .descendants()
        .filter(|n| element(*n, RDF, "RDF"))
        .collect();
    if roots.len() != 1 {
        return Err("Expected one RDF packet".into());
    }
    let rdf = roots[0];
    let descriptions: Vec<_> = rdf.children().filter(Node::is_element).collect();
    if descriptions.iter().any(|d| {
        !element(*d, RDF, "Description")
            || d.attribute((RDF, "about")).is_some_and(|a| !a.is_empty())
    }) {
        return Err("Unsupported RDF resource".into());
    }
    let mut edits = Vec::new();
    let mut ratings = Vec::new();
    for description in &descriptions {
        // Replace a prior metadata-only description without leaving an empty one.
        if description.attribute((NAMESPACE, "RatingSource")).is_some()
            && description.attributes().all(|a| {
                (a.namespace() == Some(RDF) && a.name() == "about")
                    || (a.namespace() == Some(NAMESPACE) && FIELDS.contains(&a.name()))
            })
            && description
                .children()
                .all(|n| n.is_text() && n.text().is_none_or(|t| t.trim().is_empty()))
        {
            edits.push((description.range(), String::new()));
            continue;
        }
        for attr in description.attributes() {
            if attr.namespace() == Some(XMP) && attr.name() == "Rating" {
                ratings.push((
                    attr.value()
                        .trim()
                        .parse::<i32>()
                        .map_err(|_| "Invalid existing rating")?,
                    attr.range_value(),
                    true,
                ));
            }
            if attr.namespace() == Some(NAMESPACE) && FIELDS.contains(&attr.name()) {
                edits.push((attr.range(), String::new()));
            }
        }
        for child in description.children().filter(Node::is_element) {
            if element(child, XMP, "Rating") {
                if child.children().any(|n| n.is_element()) || child.attributes().len() > 0 {
                    return Err("Unsupported existing rating".into());
                }
                ratings.push((
                    child
                        .text()
                        .unwrap_or("")
                        .trim()
                        .parse::<i32>()
                        .map_err(|_| "Invalid existing rating")?,
                    child.range(),
                    false,
                ));
            }
            if child.tag_name().namespace() == Some(NAMESPACE)
                && FIELDS.contains(&child.tag_name().name())
            {
                edits.push((child.range(), String::new()));
            }
        }
    }
    if ratings.len() > 1 {
        return Err("Ambiguous existing rating".into());
    }
    let kept = ratings.first().is_some_and(|(n, ..)| *n != 0);
    let rating = if kept {
        ratings[0].0
    } else {
        i32::from(initial.stars)
    };
    let mut rating_attribute = String::new();
    if !kept {
        match ratings.first() {
            Some((_, range, true)) => edits.push((range.clone(), rating.to_string())),
            Some((_, range, false)) => {
                edits.push((range.clone(), String::new()));
                rating_attribute = format!(" xmp:Rating=\"{rating}\"");
            }
            None => rating_attribute = format!(" xmp:Rating=\"{rating}\""),
        }
    }
    let labels = initial.keywords();
    let flat: Vec<_> = std::iter::once("Card Intake".to_string())
        .chain(labels.iter().cloned())
        .collect();
    let hierarchy: Vec<_> = labels.iter().map(|s| format!("Card Intake|{s}")).collect();
    let flat_xml = add_keywords(text, &descriptions, DC, "subject", &flat, &mut edits)?;
    let hierarchy_xml = add_keywords(
        text,
        &descriptions,
        LR,
        "hierarchicalSubject",
        &hierarchy,
        &mut edits,
    )?;
    let mut fields = vec![
        ("RatingSource", initial.source.name().to_string()),
        ("SuggestedRating", initial.stars.to_string()),
        (
            "ExistingRatingKept",
            if kept { "True" } else { "False" }.into(),
        ),
        ("Pick", if initial.pick { "True" } else { "False" }.into()),
        ("Version", env!("CARGO_PKG_VERSION").into()),
        ("WrittenAt", chrono::Utc::now().to_rfc3339()),
    ];
    if initial.source == RatingSource::Computed {
        for (key, value) in [
            ("ISO", initial.settings.iso.map(|n| n.to_string())),
            ("ISODeduction", Some(initial.iso_deduction.to_string())),
            ("ISOSkipped", initial.iso_skipped.map(str::to_string)),
            (
                "ExposureTime",
                initial.settings.exposure.map(|n| n.to_string()),
            ),
            (
                "FocalLength",
                initial.settings.focal_length.map(|n| n.to_string()),
            ),
            (
                "CropFactor",
                initial.settings.crop_factor.map(|n| n.to_string()),
            ),
            (
                "SafeShutterSpeed",
                initial.safe_speed.map(|n| n.to_string()),
            ),
            (
                "ShutterDeduction",
                Some(initial.shutter_deduction.to_string()),
            ),
            (
                "ShutterSkipped",
                initial.shutter_skipped.map(str::to_string),
            ),
        ] {
            if let Some(value) = value {
                fields.push((key, value));
            }
        }
    }
    let fields: String = fields
        .iter()
        .map(|(key, value)| format!(" ci:{key}=\"{}\"", escape(value)))
        .collect();
    let addition = format!(
        "\n<rdf:Description xmlns:rdf=\"{RDF}\" rdf:about=\"\" xmlns:xmp=\"{XMP}\" xmlns:ci=\"{NAMESPACE}\"{rating_attribute}{fields}>{flat_xml}{hierarchy_xml}</rdf:Description>\n"
    );
    edits.push(insert_inside(text, rdf, addition)?);
    edits.sort_by_key(|(range, _)| (range.start, range.end));
    if edits.windows(2).any(|pair| pair[0].0.end > pair[1].0.start) {
        return Err("Overlapping XMP properties".into());
    }
    let mut output = text.to_string();
    for (range, replacement) in edits.into_iter().rev() {
        output.replace_range(range, &replacement);
    }
    Document::parse(&output).map_err(|e| format!("Couldn't validate merged XMP: {e}"))?;
    Ok((output, Written { rating, kept }))
}

pub(crate) fn write(path: &Path, initial: &InitialRating) -> Result<Written, String> {
    let mut permissions = None;
    let original = match fs::symlink_metadata(path) {
        Ok(meta) => {
            #[cfg(windows)]
            let linked = {
                use std::os::windows::fs::MetadataExt;
                meta.file_attributes() & 0x400 != 0
            };
            #[cfg(not(windows))]
            let linked = meta.file_type().is_symlink();
            if !meta.is_file() || linked {
                return Err("Sidecar isn't a regular file".into());
            }
            if meta.permissions().readonly() {
                return Err("Sidecar is read-only".into());
            }
            permissions = Some(meta.permissions());
            Some(fs::read(path).map_err(|e| e.to_string())?)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.to_string()),
    };
    let text = match &original {
        Some(bytes) => std::str::from_utf8(bytes).map_err(|e| e.to_string())?,
        None => EMPTY,
    };
    let (output, written) = merge(text, initial)?;
    let parent = path.parent().ok_or("Missing sidecar folder")?;
    let mut builder = tempfile::Builder::new();
    builder.prefix(".card-intake-").suffix(".part");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // New sidecars follow the process umask, rather than tempfile's private 0600.
        builder.permissions(fs::Permissions::from_mode(0o666));
    }
    let mut temp = builder.tempfile_in(parent).map_err(|e| e.to_string())?;
    temp.write_all(output.as_bytes())
        .map_err(|e| e.to_string())?;
    if let Some(permissions) = permissions {
        temp.as_file()
            .set_permissions(permissions)
            .map_err(|e| e.to_string())?;
    }
    temp.as_file().sync_all().map_err(|e| e.to_string())?;
    if let Some(original) = original {
        // Do not replace edits saved by Lightroom while the packet was being prepared.
        if fs::read(path).map_err(|e| e.to_string())? != original {
            return Err("Sidecar changed while rating".into());
        }
        temp.persist(path).map_err(|e| e.to_string())?;
    } else {
        temp.persist_noclobber(path).map_err(|e| e.to_string())?;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rating::{Settings, initial_rating};

    fn computed() -> InitialRating {
        initial_rating(
            &Settings {
                iso: Some(6400),
                exposure: Some(0.04),
                focal_length: Some(50.0),
                crop_factor: Some(1.0),
                camera_rating: None,
            },
            false,
        )
    }

    #[test]
    fn creates_a_lightroom_packet_with_all_fields_and_keywords() {
        let (xml, written) = merge(EMPTY, &computed()).unwrap();
        assert_eq!((written.rating, written.kept), (2, false));
        let doc = Document::parse(&xml).unwrap();
        let description = doc
            .descendants()
            .find(|n| n.attribute((XMP, "Rating")).is_some())
            .unwrap();
        assert_eq!(description.attribute((XMP, "Rating")), Some("2"));
        for (field, value) in [
            ("RatingSource", "computed"),
            ("ISO", "6400"),
            ("ISODeduction", "2"),
            ("ShutterDeduction", "1"),
            ("SafeShutterSpeed", "0.02"),
        ] {
            assert_eq!(description.attribute((NAMESPACE, field)), Some(value));
        }
        assert!(xml.contains("Card Intake|High ISO -2"));
        assert!(xml.contains("Card Intake|Slow shutter -1"));
        assert!(xml.starts_with("<?xpacket") && xml.contains("<x:xmpmeta"));
    }

    #[test]
    fn merges_both_rating_forms_and_preserves_unrelated_bytes() {
        for property in ["x:Rating='0'", ""] {
            let body = if property.is_empty() {
                "<x:Rating>0</x:Rating>"
            } else {
                ""
            };
            let xml = format!(
                "<x:xmpmeta xmlns:x='adobe:ns:meta/'><r:RDF xmlns:r='{RDF}'><r:Description xmlns:x='{XMP}' xmlns:crs='http://ns.adobe.com/camera-raw-settings/1.0/' xmlns:custom='urn:other' {property} crs:Exposure2012='+0.35' custom:keep='A &amp; B'>{body}<!-- keep this --><custom:Tree a='b'><custom:Leaf/></custom:Tree></r:Description></r:RDF></x:xmpmeta>"
            );
            let (merged, result) = merge(&xml, &computed()).unwrap();
            assert!(!result.kept);
            assert!(merged.contains("crs:Exposure2012='+0.35' custom:keep='A &amp; B'"));
            assert!(
                merged
                    .contains("<!-- keep this --><custom:Tree a='b'><custom:Leaf/></custom:Tree>")
            );
        }
    }

    #[test]
    fn keeps_nonzero_ratings_including_reject_and_dedupes_added_keywords() {
        for stars in [-1, 1, 3, 5] {
            let xml = format!(
                "<rdf:RDF xmlns:rdf='{RDF}'><rdf:Description xmlns:z='{XMP}' z:Rating='{stars}'/></rdf:RDF>"
            );
            let (once, kept) = merge(&xml, &computed()).unwrap();
            assert_eq!((kept.rating, kept.kept), (stars, true));
            let (twice, _) = merge(&once, &computed()).unwrap();
            assert_eq!(twice.matches("Card Intake|High ISO -2").count(), 1);
            assert_eq!(twice.matches(">High ISO -2<").count(), 1);
            let doc = Document::parse(&twice).unwrap();
            assert_eq!(
                doc.descendants()
                    .filter(|n| n.attribute((NAMESPACE, "RatingSource")).is_some())
                    .count(),
                1
            );
            assert!(twice.contains("ExistingRatingKept=\"True\""));
            let descriptions = doc
                .descendants()
                .filter(|n| n.has_tag_name((RDF, "Description")))
                .count();
            let (third, _) = merge(&twice, &computed()).unwrap();
            let third = Document::parse(&third).unwrap();
            assert_eq!(
                third
                    .descendants()
                    .filter(|n| n.has_tag_name((RDF, "Description")))
                    .count(),
                descriptions
            );
        }
    }

    #[test]
    fn existing_keywords_empty_bags_and_namespace_aliases() {
        let xml = format!(
            "<r:RDF xmlns:r='{RDF}'><r:Description xmlns:d='{DC}' xmlns:l='{LR}'><d:subject><r:Bag><r:li>旅行 &amp; family</r:li><r:li>Card Intake</r:li></r:Bag></d:subject><l:hierarchicalSubject><r:Bag/></l:hierarchicalSubject></r:Description></r:RDF>"
        );
        let (merged, _) = merge(&xml, &computed()).unwrap();
        assert!(merged.contains("<r:li>旅行 &amp; family</r:li>"));
        assert_eq!(merged.matches(">Card Intake<").count(), 1);
        assert!(merged.contains("Card Intake|Computed rating"));
    }

    #[test]
    fn unsafe_packets_are_left_untouched_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.xmp");
        for bytes in [
            "<broken",
            "<!DOCTYPE x [<!ENTITY a 'external'>]><x/>",
            "<x/>",
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(write(&path, &computed()).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn writes_atomically_and_records_a_pick_request_for_locked_raws() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.xmp");
        let rating = initial_rating(&Settings::default(), true);
        assert_eq!(write(&path, &rating).unwrap().rating, 5);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("ci:Pick=\"True\"")
        );
        assert!(write(&path, &rating).unwrap().kept);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
