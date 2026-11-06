# Card Intake

Import camera cards straight to a NAS, one folder per shoot, and open each shoot in its own Lightroom Classic catalog.

Built for this workflow: Sony ARW → NAS over 10 GbE → Lightroom Classic (+ DxO PureRAW, Pixel Cake) → finished JPEGs in Immich.

> Status: early scaffold. The shared logic (card detection, scanning, grouping by date, folder naming) works and is tested. The desktop app shows the design prototype; copying, checking and the NAS service are next.

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
cargo test                          # shared logic tests
cargo run -p card_intake_desktop    # desktop app
cargo run -p card_intake_server     # NAS service on http://localhost:8787/health
```

Installers and app bundles: `cargo install tauri-cli --version "^2"`, then `cargo tauri build` inside `apps/desktop`.

### NAS service on Synology

In Container Manager, create a project from `docker/compose.yaml` and set the photo share path in it (`/volume1/photos` by default).

### Lightroom plugin

In Lightroom Classic: File → Plug-in Manager → Add, and pick `lrplugin/CardIntake.lrplugin`. Do this once on each computer. The plugin is an untested draft for now.
