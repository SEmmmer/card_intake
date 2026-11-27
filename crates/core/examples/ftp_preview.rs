//! What a camera's FTP folder has of a card, without changing either:
//! `cargo run -p card_intake_core --example ftp_preview -- E:\ \\NAS\FTP`

use card_intake_core::ftp;
use std::path::Path;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(card), Some(root)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: ftp_preview <card> <ftp folder>");
        std::process::exit(2);
    };
    let started = Instant::now();
    match ftp::preview(Path::new(card), Path::new(root)) {
        Ok(preview) => {
            println!(
                "{} photos in {} ({:.1} s)",
                preview.photos,
                preview.root.display(),
                started.elapsed().as_secs_f64()
            );
            for d in preview.dates {
                println!(
                    "{}: {} on the card already in FTP ({:.1} GB), {} more from this camera in FTP",
                    d.date,
                    d.on_card,
                    d.on_card_bytes as f64 / 1e9,
                    d.ftp_only
                );
            }
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
