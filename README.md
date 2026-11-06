# Card Intake

Import camera cards straight to a NAS, one folder per shoot, and open each shoot in its own Lightroom Classic catalog.

Built for this workflow: Sony ARW → NAS over 10 GbE → Lightroom Classic (+ DxO PureRAW, Pixel Cake) → finished JPEGs in Immich.

> Status: the desktop app imports real cards to a network share, groups shoots by capture date, and verifies copies with xxHash64. It copies four files at once, includes photo XMP and Sony clip XML sidecars, and shows progress with speed, time left, and files already on the NAS. Stopped imports can resume after an app restart. Windows supports a safe ejection attempt after verification and opening local Lightroom catalogs with a plug-in handoff. Card scanning, camera metadata, shared shutter history, CFexpress health and serial numbers, and NAS checks use real data. The NAS service remains a draft. Real NAS transfers, physical ejection, and the Lightroom handoff still need end-to-end validation with configured hardware and Lightroom.

## Card health and identity

CFexpress cards are NVMe drives with their own health log: life used, spare blocks, data written and read, power cycles, unsafe shutdowns, media errors, temperature. Command totals remain in the raw health data but are omitted from the UI. Cards also have their own serial number, which formatting never changes.

| | Card seen as an NVMe drive (USB4 or Thunderbolt reader, e.g. Sony MRW-G3 on a USB4 port) | Behind a USB bridge (e.g. MRW-G3 on a USB 10 Gbps port) |
|---|---|---|
| Windows | Read on insertion, directly from Windows (`IOCTL_STORAGE_QUERY_PROPERTY`): no smartctl, no admin rights. | With [smartctl](https://www.smartmontools.org) for Windows; the app asks for admin rights each time. |
| macOS | With smartmontools installed. | No. macOS has no pass-through for USB readers, so no tool can read it (Sony's own Media Scan Utility doesn't support the MRW-G3 on Mac either). |

Cards are recognised by their own serial number when the reader shows the card itself (NVMe, or an SD host controller). A card formatted in the camera then stays the same card, and the app counts the format. Behind a USB bridge only the volume ID is available, and formatting changes it. SD cards keep no health log, and USB SD readers don't pass the card's serial number (CID) through, so an SD card formatted in the camera shows up as a new card.

Readings are saved with the card in this computer's card list.

## Shutter counts

The shutter count of every photo on an inserted card is read with ExifTool (in parallel, about a second for 200 ARW files) and kept per camera, one count range per shooting day. **Read NAS photos** (under Cameras) reads every photo already on the share too, so the graph goes back to the first imported shoot; folders read before are skipped unless their photos change.

The history is saved in the photo share at `.card-intake/shutter-counts.json`, so every computer running Card Intake (Windows or Mac) reads and adds to the same history. A lock file keeps two computers from writing at the same moment, and merging is idempotent: the same photos read on two computers count once. A copy is kept in the app data folder for when the share isn't connected, and is merged in the next time it is.

The main display is **Mechanical shutter count**, with a smaller **Electronic** count beside it. The latter uses an explicit `ElectronicShutterCount` tag when available; absent metadata is shown as **Not reported**. Alternative `ShutterCount2/3` and `ImageCount` values are not treated as independent electronic counters. ExifTool identifies the α7R VI's `ShutterCount` as mechanical actuations in its [Sony tag definitions](https://github.com/exiftool/exiftool/blob/master/lib/Image/ExifTool/Sony.pm). Zero mechanical actuations is a valid reading.

Manufacturer ratings remain separate from unconfirmed reference values (`crates/core/src/cameras.rs`). The user-suggested **500,000** value for α7R VI is displayed as an **unconfirmed life expectancy reference**. It was not confirmed in [Sony's ILCE-7RM6 specifications](https://www.sony.com/electronics/support/e-mount-body-ilce-7-series/ilce-7rm6/specifications) or the Japanese feature pages checked on 2026-11-07. Schema version 2 preserves existing shutter history and re-reads old folder snapshots once to collect any electronic counters.

## NAS folder

Set the photo share's path in the app (NAS folder → Save). **Check** makes sure it's the right place before anything is copied there:

- the folder exists (is the share mounted?)
- it's on a network share, not this computer's own disk, so a missing mount doesn't fill the local disk
- a 64 MB test file writes, reads back identical, and is deleted
- there's enough free space
- it looks like the photo share (year folders such as `2026` with shoot folders inside)

## Import a card on Windows

1. Connect the NAS share in Explorer. Enter its UNC path (for example `\\NAS\photos`) or mapped drive path (`Z:\photos`) under **NAS folder**, then **Save**. Imports require an existing network folder; a local folder or an unrecognized mount is rejected.
2. Insert a camera card. The app detects `DCIM`, `M4ROOT/CLIP`, and `PRIVATE/M4ROOT/CLIP`, including video-only Sony cards. Select capture dates and name the shoots. The displayed paths show the destinations.
3. Leave **Check every file after copying** enabled. Choose whether to include videos and whether to attempt ejection, then click **Import to NAS**. Photos and their XMP files go in the shoot folder; clips and their XML/XMP sidecars go in its `video` folder.
4. Check the result and **Recent imports**. The result distinguishes copied, identical/skipped, and verified files. Originals remain on the card. Unselected shoots, excluded videos, proxies and unrelated files are not imported.

**Shoot folders that already exist.** When the card is read, the app looks up each capture date's folders on the NAS (`2026/2026-11-06` and `2026/2026-11-06_<title>`). A date that already has a folder gets that title in the shoot name, so another card from the same day goes into the same folder; the card shows how many of its photos and clips are already on the NAS (same name and size), and the summary counts only the new ones. Clear the name or type another one and a new folder is made, but files already in the date's other folders are not copied again: the import compares each one with the card (xxHash64) and skips it when identical, and copies it when it's a different photo with the same name. A photo's or clip's sidecar goes with it into the folder where it already is. The done panel lists every folder that holds the files.

Four files are copied at once: a CFexpress card reads about twice as fast with parallel reads (measured on a CEA-G240T in an MRW-G3 on USB4: 837 MB/s with one stream, 1,647 MB/s with two to four), and one file's read-back overlaps the next one's copy. The progress line shows the card's read speed and the time left, and each import's average speed is kept in its history.

Copies are staged in unique `.card-intake-*.part` files, flushed, and (when enabled) read back and compared with the source's xxHash64 checksum. Only complete copies are published under their final filenames. An existing file is skipped only after its size and checksum match; different contents stop the import without replacing it. Two card files targeting the same filename are rejected before any copying starts.

**Stop** waits for the files being copied, removes their partial copies when the share is reachable, and keeps completed files. The desk then shows how far the import got, and **Resume import** repeats the same selection: completed files are hashed and skipped, unfinished ones copied from the beginning. The selection and options are also saved before copying begins, so after an app restart the same card comes back with its shoot names and options filled in and **Resume import** ready. A new import replaces the saved one. Closing the window during an import requests cancellation and leaves the result visible; close it again after the worker finishes. A process crash or disconnected share can leave a `.part` file; retries ignore it and create a fresh staging file.

Automatic ejection requires verification. Windows must be able to lock the volume before dismounting and ejecting it. A busy reader, unsupported eject command or permission error is reported separately from the successful copy; use Explorer to eject in that case.

The import engine is shared with the macOS app, but the new desktop workflow has been built and tested on Windows. Physical reader ejection and a real NAS transfer still need validation with your hardware.

## Workflow and next steps

- **Import a card** to `\\NAS\photos\2026\2026-11-02_Yosemite\`, grouped by capture date, with optional xxHash64 read-back verification (enabled by default).
- **Track cards and cameras**: camera serial number and shutter count history from EXIF, shared through the NAS; CFexpress health (data written, life used, power cycles) and serial number from the card itself; for SD cards, this app's own counts.
- **Open in Lightroom**: create a local per-shoot or combined catalog from a closed template, open it, and add originals in place through the Lightroom plug-in. Existing working catalogs and edits are reused.
- **Planned — NAS service**: sort files from Pixel Cake into the right `_final` folder, ask Immich to rescan, and clean up `_work` when a shoot is finished.

## Folder layout on the NAS

```
\\NAS\photos\
  2026\
    2026-11-02_Yosemite\
      DSC03518.ARW, .xmp          originals + Lightroom edits
      DSC03518_DxO.dng            DxO PureRAW output, next to its ARW
      video\                     original clips + Sony XML sidecars
      _work\                      files handed to Pixel Cake (temporary)
      _final\                     finished JPEGs (the only thing Immich shows)
      _catalog\                   the shoot's Lightroom catalog, once the shoot is finished
  _inbox\                         Pixel Cake exports here; the NAS service sorts it
```

Immich external library exclusion patterns: `**/*.ARW`, `**/*.dng`, `**/*.DNG`, `**/_work/**`, `**/_inbox/**`, `**/_catalog/**`.

Lightroom Classic working catalogs stay on the local disk while you edit, at `<catalog folder>\2026\2026-11-06_Yosemite\2026-11-06_Yosemite.lrcat`. A catalog you keep in the shoot's `_catalog` folder on the NAS is copied back to this computer when the shoot is opened again (the history shows it as **Stored on NAS** with **Reopen**). Moving a finished catalog to `_catalog` is still done by hand.

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

Needs Rust (stable) and, on macOS, the Xcode command line tools. On Windows, use the MSVC Rust toolchain, Visual Studio C++ Build Tools with a Windows SDK, and WebView2.

Windows (PowerShell):

```powershell
cargo test --workspace
cargo run -p card_intake_desktop
# Standalone executable: target\release\card_intake_desktop.exe
cargo build --release -p card_intake_desktop
```

ExifTool is optional for importing, but needed for shutter counts. Install ExifTool for Windows with its companion support folder, then put `exiftool.exe` on `PATH` or set `$env:CARD_INTAKE_EXIFTOOL` to its full path. The shell fetch script below is for macOS/Linux.

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
cargo run -p card_intake_desktop -- --shutter /Volumes/photos/2026 # shutter counts of every photo in a folder
cargo run -p card_intake_core --example card_storage -- E:\         # Windows: the card's bus, serial and NVMe health log
cargo run -p card_intake_core --example scan_card -- /Volumes/Untitled
```

On Windows, use `E:\` instead of `/Volumes/Untitled` and `\\NAS\photos` instead of `/Volumes/photos`.

The import tests use temporary card/share fixtures and cover verified copies, sidecars, date/video selection, conflicts, corrupt read-backs, cancellation, parallel workers, retries, files already under another shoot title, changed sources, saved resume requests, history, and rejection of local destinations. Lightroom tests cover catalog creation, reuse and reopening from the NAS, and run the actual Lua plug-in in Fengari with mocked Adobe SDK services (duplicates, partial failures, retries, disconnected folders, and requests for another catalog). UI tests run the real page with a mocked Tauri bridge and require Node.js:

```sh
npm ci --prefix tests/ui
npm test --prefix tests/ui
cargo clippy --workspace --all-targets -- -D warnings
```

The NAS path is saved in `settings.json` in the app's config folder. Environment variables override it and the tool locations: `CARD_INTAKE_NAS`, `CARD_INTAKE_EXIFTOOL`, `CARD_INTAKE_SMARTCTL`. Cards, health readings and the most recent 100 import results are saved in `ledger.json`, and this computer's copy of the shutter count history in `shutter-counts.json`, in the app data folder (`%APPDATA%\io.github.semmmer.cardintake\` on Windows; `~/Library/Application Support/io.github.semmmer.cardintake/` on macOS). Older ledgers load with an empty import history.

The Windows-specific code lives in `crates/core`. `cargo test --workspace` builds and tests the native desktop backend on Windows; tests do not eject physical cards or write to configured NAS shares.

Installers and app bundles: `cargo install tauri-cli --version "^2"`, then `cargo tauri build` inside `apps/desktop`.

### NAS service on Synology

In Container Manager, create a project from `docker/compose.yaml` and set the photo share path in it (`/volume1/photos` by default).

### Lightroom plugin

1. In the Lightroom Classic version you use, create an **empty catalog** and close it. Keep that `.lrcat` as a template; opening it for editing can leave lock/WAL files, in which case Card Intake refuses to copy it.
2. In Card Intake, under **Lightroom**, open **Catalog template, folder and Lightroom**, enter the template path, and save. The Windows installation of `Lightroom.exe` is detected automatically. Catalogs default to `%LOCALAPPDATA%\io.github.semmmer.cardintake\Catalogs`; you can choose another local folder (for example `D:\LrC Catalogs`).
3. In Lightroom: **File → Plug-in Manager → Add**, choose the plug-in folder shown in Card Intake, and enable it. The app writes the bundled plug-in to its local data folder. After updating it, reload the plug-in or restart Lightroom. The repository copy is `lrplugin/CardIntake.lrplugin`.
4. After a completed import, use **Open in Lightroom** beside a shoot, or **Open all … shoots in one catalog** (it uses the first shoot's catalog). The **Lightroom catalog** column in Recent imports shows whether a shoot's catalog is on this PC or stored on the NAS, with **Open** or **Reopen**. A new catalog is a copy of the template; later opens reuse it without overwriting edits.

Requests are tied to the exact catalog filename. The plug-in polls while the catalog is open, adds supported photos directly from the shoot folders on the NAS, and skips photos already in the catalog. Videos and sidecars are not added as photos. Card Intake waits for the plug-in's acknowledgement before displaying successful addition counts. Failed requests are retained; click **Open in Lightroom** again to retry. If the plug-in is disabled or Lightroom is waiting at a dialog, the request stays queued. Catalogs created by another Lightroom version may require an upgrade; recreate the template in the installed version before retrying.

The browser-only `prototype/card-intake.html` remains a design demonstration. Real imports, persistent resume, and Lightroom launching run in the desktop app, whose UI is `ui/index.html`.
