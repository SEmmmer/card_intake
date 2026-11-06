# Card Intake

Import camera cards straight to a NAS, one folder per shoot, and open each shoot in its own Lightroom Classic catalog.

Built for this workflow: Sony ARW → NAS over 10 GbE → Lightroom Classic (+ DxO PureRAW, Pixel Cake) → finished JPEGs in Immich.

> Status: the desktop app reads real cards: photos and clips, capture times (EXIF and Sony clip sidecars), shoots by date, card ID and size, and camera model, serial number, firmware, lens and shutter count. It reads CFexpress card health on Windows, checks the NAS folder before you use it, and remembers the cards and cameras it has seen. Importing is still a demo that copies nothing; copying and the NAS service are next.

## Card health

CFexpress cards are NVMe drives with their own health log: life used, spare blocks, data and commands written and read, power cycles, unsafe shutdowns, media errors, temperature. The app reads it with [smartctl](https://www.smartmontools.org).

| | Through a USB reader (e.g. Sony MRW-G3) | Card seen as an NVMe drive (Thunderbolt reader) |
|---|---|---|
| Windows | Yes. Install smartmontools for Windows; the app asks for admin rights each time. | Yes |
| macOS | No. macOS has no pass-through for USB readers, so no tool can read it (Sony's own Media Scan Utility doesn't support the MRW-G3 on Mac either). | Yes, with smartmontools installed |

SD cards keep no health log. Readings are saved with the card in this computer's card list; sharing that list between computers (through the NAS) comes later.

## NAS folder

Set the photo share's path in the app (NAS folder → Save). **Check** makes sure it's the right place before anything is copied there:

- the folder exists (is the share mounted?)
- it's on a network share, not this computer's own disk, so a missing mount doesn't fill the local disk
- a 64 MB test file writes, reads back identical, and is deleted
- there's enough free space
- it looks like the photo share (year folders such as `2026` with shoot folders inside)

## What it will do

- **Import a card** to `\\NAS\photos\2026\2026-11-02_Yosemite\`, grouped by capture date, with every file checked against the card (xxHash64) so you know it's safe to format.
- **Track cards and cameras**: camera serial number and shutter count from EXIF; CFexpress health (data written, life used, power cycles) from the card's own log; for SD cards, this app's own counts.
- **Open in Lightroom**: create a per-shoot catalog from your template, open it, and add every photo automatically (via the Lightroom plugin).
- **NAS service**: sort files from Pixel Cake into the right `_final` folder, ask Immich to rescan, and clean up `_work` when a shoot is finished.

## Folder layout on the NAS

```
\\NAS\photos\
  2026\
    2026-11-02_Yosemite\
      DSC03518.ARW, .xmp          originals + Lightroom edits
      DSC03518_DxO.dng            DxO PureRAW output, next to its ARW
      _work\                      files handed to Pixel Cake (temporary)
      _final\                     finished JPEGs (the only thing Immich shows)
      _catalog\                   the shoot's Lightroom catalog, once the shoot is finished
  _inbox\                         Pixel Cake exports here; the NAS service sorts it
```

Immich external library exclusion patterns: `**/*.ARW`, `**/*.dng`, `**/*.DNG`, `**/_work/**`, `**/_inbox/**`, `**/_catalog/**`.

Lightroom Classic can't open a catalog on a network drive, so a shoot's catalog stays on the local disk while you edit and moves to `_catalog` when you finish.

## Repo layout

| Path | What |
|---|---|
| `crates/core` | Shared Rust logic: card detection, scanning, grouping, folder names |
| `apps/desktop` | Desktop app for Windows and macOS (Tauri 2) |
| `apps/server` | NAS service (axum), runs in Docker on the Synology |
| `ui` | The app's screens (started from the prototype) |
| `lrplugin/CardIntake.lrplugin` | Lightroom Classic plugin that adds photos to a new catalog |
| `docker` | Dockerfile and compose file for Synology Container Manager |
| `prototype` | Clickable design mock-up |

## Develop

Needs Rust (stable) and, on macOS, the Xcode command line tools. On Windows, Tauri needs WebView2, which Windows 10 and 11 already have.

```sh
scripts/fetch-exiftool.sh           # once: ExifTool, needed for shutter counts
cargo test                          # tests
cargo run -p card_intake_desktop    # desktop app
cargo run -p card_intake_server     # NAS service on http://localhost:8787/health
```

Checks from the command line, without opening a window (nothing is written to the card):

```sh
cargo run -p card_intake_desktop -- --report /Volumes/Untitled     # what the app reads from a card
cargo run -p card_intake_desktop -- --health /Volumes/Untitled     # the card's health log
cargo run -p card_intake_desktop -- --check-nas /Volumes/photos    # the NAS folder check
cargo run -p card_intake_core --example scan_card -- /Volumes/Untitled
```

The NAS path is saved in `settings.json` in the app's config folder. Environment variables override it and the tool locations: `CARD_INTAKE_NAS`, `CARD_INTAKE_EXIFTOOL`, `CARD_INTAKE_SMARTCTL`. The list of cards, cameras and health readings is `ledger.json` in the app data folder (`~/Library/Application Support/io.github.semmmer.cardintake/` on macOS).

The Windows-specific code lives in `crates/core` and is compile-checked with `cargo check -p card_intake_core --target x86_64-pc-windows-msvc`; the desktop app itself has to be built on Windows.

Installers and app bundles: `cargo install tauri-cli --version "^2"`, then `cargo tauri build` inside `apps/desktop`.

### NAS service on Synology

In Container Manager, create a project from `docker/compose.yaml` and set the photo share path in it (`/volume1/photos` by default).

### Lightroom plugin

In Lightroom Classic: File → Plug-in Manager → Add, and pick `lrplugin/CardIntake.lrplugin`. Do this once on each computer. The plugin is an untested draft for now.
