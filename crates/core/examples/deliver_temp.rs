//! Local write validation: plans a delivery, confirms everything it would add, and
//! delivers. Refuses a shoot or finished photos folder outside system temp.
//!
//!     cargo run -p card_intake_core --example deliver_temp -- <temp-shoot> <temp-finished-photos> <exiftool> [catalog]
use card_intake_core::deliver;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() < 3 {
        return Err(
            "usage: deliver_temp <temp-shoot> <temp-finished-photos> <exiftool> [catalog]".into(),
        );
    }
    let temp = std::env::temp_dir().canonicalize()?;
    let shoot = PathBuf::from(&args[0]).canonicalize()?;
    let photos = PathBuf::from(&args[1]).canonicalize()?;
    for folder in [&shoot, &photos] {
        if !folder.starts_with(&temp) || *folder == temp {
            return Err("Write validation is restricted to folders under system temp.".into());
        }
    }
    let exiftool = PathBuf::from(&args[2]);
    let catalog = args.get(3).map(PathBuf::from);
    let target = photos.join(shoot.file_name().ok_or("No shoot folder name")?);
    let mut source_hashes = std::collections::BTreeMap::new();
    for e in walkdir::WalkDir::new(&shoot) {
        let e = e?;
        if e.file_type().is_file() && !e.file_name().to_string_lossy().starts_with(".card-intake") {
            source_hashes.insert(e.path().to_path_buf(), deliver::hash_file(e.path())?);
        }
    }
    let plan = deliver::plan(&shoot, &exiftool, catalog.as_deref(), &target)?;
    let result = deliver::deliver(
        &shoot,
        &exiftool,
        catalog.as_deref(),
        &target,
        &plan.confirmed(),
    )?;
    for (source, before) in &source_hashes {
        if deliver::hash_file(source)? != *before {
            return Err(format!("Source changed: {}", source.display()).into());
        }
    }
    let checks = result
        .photos
        .iter()
        .filter(|p| p.action == "delivered")
        .map(|p| -> Result<_, std::io::Error> {
            let source = deliver::hash_file(&shoot.join(&p.source))?;
            let copy = deliver::hash_file(&target.join(&p.name))?;
            Ok(serde_json::json!({
                "name": p.name,
                "source_xxhash64": format!("{source:016x}"),
                "copy_xxhash64": format!("{copy:016x}"),
                "identical": source == copy,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut json = serde_json::to_value(&result)?;
    json["validation"] = serde_json::json!({
        "sources_unchanged": true,
        "source_count": source_hashes.len(),
        "staging_left": shoot.join(deliver::STAGING).exists(),
        "checks": checks,
    });
    println!("{}", serde_json::to_string_pretty(&json)?);
    if !result.errors.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}
