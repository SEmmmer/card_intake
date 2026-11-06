# Card Intake

Import camera cards straight to a NAS, one folder per shoot, and open each shoot in its own Lightroom Classic catalog.

Built for this workflow: Sony ARW → NAS over 10 GbE → Lightroom Classic (+ DxO PureRAW, Pixel Cake) → finished JPEGs in Immich.

> Status: the desktop app imports real cards to a network share, groups shoots by capture date, and verifies copies with xxHash64. It copies four files at once, includes photo XMP and Sony clip XML sidecars, and shows progress with speed, time left, and files already on the NAS. Stopped imports can resume after an app restart. Windows supports a safe ejection attempt after verification. Each shoot (or trip) gets its own Lightroom catalog, which a plug-in Card Intake installs fills from the NAS, and **Finish** stores it on the NAS. Card scanning, camera metadata, shared shutter history, CFexpress health and serial numbers, and NAS checks use real data. The NAS service remains a draft. Real NAS transfers, physical ejection, and the Lightroom plug-in still need end-to-end validation with configured hardware and Lightroom.

## Card health and identity

CFexpress cards are NVMe drives with their own health log: life used, spare blocks, data written and read, power cycles, unsafe shutdowns, media errors, temperature. Command totals remain in the raw health data but are omitted from the UI. Cards also have their own serial number, which formatting never changes. The model, serial number and health log come from the card itself, whatever the make — a Nikon Z 9's Type B card is read exactly like a Sony card.

| | Card seen as an NVMe drive (USB4 or Thunderbolt reader, e.g. Sony MRW-G3 on a USB4 port) | Behind a JMicron JMS583 USB bridge (USB ID `152D:0583`, e.g. CHIPFANCIER CFCR-II Type B reader) | Behind another USB bridge (e.g. MRW-G3 on a USB 10 Gbps port) |
|---|---|---|---|
| Windows | Read on insertion, directly from Windows (`IOCTL_STORAGE_QUERY_PROPERTY`): no smartctl, no admin rights. | Read by the app itself through the bridge, the way smartctl and CrystalDiskInfo do it. Windows only allows that with admin rights, so **Read card health** asks each time. When the app already runs as admin, the card is read on insertion. | With [smartctl](https://www.smartmontools.org) for Windows; the app asks for admin rights each time. |
| macOS | With smartmontools installed. | No. macOS has no pass-through for USB readers. | No. macOS has no pass-through for USB readers, so no tool can read it (Sony's own Media Scan Utility doesn't support the MRW-G3 on Mac either). |

Through the JMicron bridge the app reads the same things as on the NVMe bus: the card's model, serial number, firmware and NVMe version, and its whole health log (also controller busy time and time spent above the warning and critical temperatures). It sends only two NVMe commands, both read-only: Identify Controller and Get Log Page (SMART / Health). It sends them only to a reader whose USB ID is a JMS583, because other devices would read those SCSI commands as something else. Tested with a CHIPFANCIER CFE Platinum 640 GB card in a CHIPFANCIER CFCR-II reader.

When the app isn't running as admin, it asks for admin rights by starting a hidden copy of itself with them (`--read-card-health-as-admin <drive> <answer file>`). Windows shows its usual prompt. The copy reads the card, writes the answer to a new file in the temp folder, and exits. It never overwrites an existing file.

Cards are recognised by their own serial number when the app reaches the card itself (NVMe, an SD host controller, or a JMicron bridge with admin rights). A card formatted in the camera then stays the same card, and the app counts the format. Behind a JMicron bridge without admin rights, a formatted card first shows up as a new card; reading its health finds its serial and puts it back with the card's own record, counting the format and keeping its imports. Behind other USB bridges only the volume ID is available, and formatting changes it. SD cards keep no health log, and USB SD readers don't pass the card's serial number (CID) through, so an SD card formatted in the camera shows up as a new card (see [Card details](#card-details-any-card-sd-included)).

Readings are saved with the card in this computer's card list.

## Card details (any card, SD included)

Under **Card details** the desk shows what every card and its reader reveal on insertion. A normal Windows user can read all of it, so the app never asks for admin rights here (`crates/core/src/card_details.rs`):

- **Reader:** the name it gives itself over USB and its USB ID (e.g. `USB Storage (05E3:0764)`, a Genesys Logic chip). Also how it talks to Windows: UAS, or the older and slower bulk-only (BOT). Then its link speed (USB 2.0, 5 Gbps, 10 Gbps or faster), with a warning when it could run faster in another port or with another cable. In a reader with several slots, the slot the card is in.
- **Card:** its exact size in bytes; the SD class that size belongs to (SDSC up to 2 GB, SDHC 2–32 GB, SDXC 32 GB–2 TB, SDUC beyond); and its lock switch, with a warning when it's on.
- **Format:** the partition table (MBR, GPT, or none), where the partition starts and its type. The file system with its revision and cluster size. From the file system's own boot sector, whether it was closed cleanly (exFAT's VolumeDirty, FAT's clean-shutdown bit). Also whether its last writer met bad sectors (exFAT's MediaFailure, FAT's hard-error bit). A card that wasn't closed cleanly, or that flags bad sectors, gets a warning.

A card is taken for an SD card when it's in an SD host controller, or when a USB reader shows it as a removable medium behind no NVMe bridge. Its size must also be a whole number of 512 KiB steps, the unit SD cards count their capacity in; a CFexpress card's NVMe size isn't. For an SD card, **Card health** says it keeps no health log, instead of offering smartctl and an admin prompt.

What an SD card in a USB reader can't tell: its CID (maker, product name, serial number, manufacture date), its CSD, and its SD Status (speed class, UHS and video speed classes). A USB reader's mass-storage interface doesn't pass them on. The readers that can fetch them do it with undocumented vendor commands, which the app doesn't send, because an unknown command could as well rewrite the reader's firmware. Only an SD host controller, such as a laptop's built-in slot, shows the card itself. So an SD card in a USB reader is known by its volume ID, and an SD card formatted in the camera shows up as a new card.

Tested with a 256 GB SDXC card from a DJI Osmo Pocket 3 in a Genesys Logic USB 3 reader (05E3:0764, two slots). The Osmo Pocket 3 writes its model number, `PP-101`, in EXIF; the camera list calls it Osmo Pocket 3 and knows it has no mechanical shutter. `cargo run -p card_intake_desktop -- --health F:\` prints the details with the health answer.

## Nikon Z 9 cards

A card formatted in a Nikon Z 9 is read like a Sony card, with two things that belong to that body:

- **Files.** Photos are NEF (NRW on older Nikon bodies) or JPEG, plus HEIF stills from the bodies that write them; clips are MOV, MP4 or N-RAW `NEV`, and Lightroom's XMP sidecars travel with their photos. Nikon keeps them in its own folder name inside `DCIM` (`100NCZ_9` on a Z 9 using the default `NCZ_9` folder name), and the whole `DCIM` tree is read. Clips carry no Sony XML sidecar, so a clip's capture date falls back to its file time.
- **Shutter count.** The Z 9's shutter is electronic only (Nikon's [Z 9 specifications](https://onlinemanual.nikonimglib.com/z9/en/specifications_348.html)). Its photos carry the count in `ShutterCount` and write 0 in `MechanicalShutterCount`, the tag a body with a mechanical shutter counts in. The app reads the Z 9's `ShutterCount` and ignores the mechanical 0, so its card adds the electronic count to the shutter history rather than a flat 0; the camera list labels the figure **Electronic shutter count**. Nikon publishes no shutter rating for a shutter it doesn't fit, so a Z 9 shows no life percentage. Other bodies keep taking their mechanical count first.

The Z 9 takes CFexpress Type B and XQD cards. A CFexpress card in a Thunderbolt or USB4 reader that shows the card as an NVMe drive reports its own model, serial number and health log on insertion, without smartctl or admin rights. In a USB reader with a JMicron JMS583 bridge the app reads the same things itself, with admin rights; behind other USB bridges it is read with smartctl, like any card. An XQD card is not an NVMe drive and keeps no NVMe health log, so it is known by its volume ID only. The card's type name comes from its own model where the maker encodes it (Sony's `CEA…` is Type A, `CEB…` Type B); other makes are listed as **CFexpress**. Reading a real Z 9 card has not been tested here: the file handling, the serial-number identity and the health log follow the paths above, and `cargo run -p card_intake_core --example card_storage -- E:\` prints what Windows reports for the inserted card.

## Shutter counts

The shutter count of every photo on an inserted card is read with ExifTool (in parallel, about a second for 200 raw files) and kept per camera, one count range per shooting day. **Read NAS photos** (under Cameras) reads every photo already on the share too, so the graph goes back to the first imported shoot; folders read before are skipped unless their photos change.

The history is a SQLite database in the photo share, `.card-intake/shutter.sqlite`, so every computer running Card Intake (Windows or Mac) reads and adds to the same history. Each computer keeps its own copy in the app data folder (`shutter.sqlite`) for when the share isn't connected. Every row records the write that last changed it, so a sync sends only this computer's rows changed since its last sync and fetches only the rows other computers changed since then; a history of many years syncs as fast as a day's. Merging is idempotent: the same photos read on two computers count once.

The shared database never uses SQLite's WAL mode, which needs shared memory a network share doesn't provide, and writes to it also hold a lock file (`.card-intake/shutter.lock`), because SQLite's own locks aren't reliable on every network share. A damaged database, or one written by a newer Card Intake, is left untouched with a message. The JSON file older builds kept (`shutter-counts.json`, in the share and in app data) is merged in automatically, and again whenever an older build changes it.

Photos kept outside the photo share — an old archive, for instance — can be added to the same history without moving or copying anything:

```sh
cargo run -p card_intake_core --example count_photos -- \\NAS\photos "\\NAS\old\Nikon Transfer 2026 5"
```

The script reads each folder once with the app's own ExifTool pass, merges the counts into the share's `.card-intake/shutter.sqlite` the way the app syncs, and writes nothing to the photo folders. Folders whose photos haven't changed are skipped on later runs, and progress is saved every 20 seconds so an interrupted run keeps what it read. A folder outside the share is remembered in the same database under an `extra:` key, which the app itself never looks up.

The main display is **Mechanical shutter count**, with a smaller **Electronic** count beside it (**Electronic shutter count** as the only figure on a body that has no mechanical shutter). The smaller figure appears when a file carries an explicit electronic counter; ExifTool 13.59 defines no `ElectronicShutterCount` tag, so it stays **Not reported** for the bodies checked here. Alternative `ShutterCount2/3` and `ImageCount` values are not treated as independent electronic counters. ExifTool identifies the α7R VI's `ShutterCount` as mechanical actuations in its [Sony tag definitions](https://github.com/exiftool/exiftool/blob/master/lib/Image/ExifTool/Sony.pm). Zero mechanical actuations is a valid reading. A body that has no mechanical shutter at all, such as the Nikon Z 9, writes 0 in that tag on every photo and keeps its real count in `ShutterCount`, so the app charts that instead ([see above](#nikon-z-9-cards)).

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
- **Open in Lightroom**: one catalog per shoot, or a trip catalog for several shoots, made from a closed template on this computer. The Card Intake plug-in adds new raw photos and DxO DNGs in place from the NAS whenever the catalog opens, with your presets, and builds Smart Previews. **Finish** stores a closed catalog on the NAS; opening the shoot again brings it back.
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
      _catalog\                   the shoot's Lightroom catalog once finished, or trip.txt naming its trip
    _trips\
      2026-11-03_Japan\           a finished trip catalog holding several shoots
  _inbox\                         Pixel Cake exports here; the NAS service sorts it
```

Immich external library exclusion patterns: `**/*.ARW`, `**/*.dng`, `**/*.DNG`, `**/_work/**`, `**/_inbox/**`, `**/_catalog/**`, `**/_trips/**`.

Lightroom Classic can't open catalogs on a network drive, so catalogs stay on this computer while you edit: `<catalog folder>\2026\2026-11-06_Yosemite\2026-11-06_Yosemite.lrcat`, or `<catalog folder>\2026\_trips\2026-11-03_Japan\` for a trip. **Finish** (in Recent imports, once Lightroom has closed the catalog) copies the catalog, its Smart Previews, its AI mask data (`.lrcat-data`) and its list of shoot folders to `_catalog` (or `_trips`), reads every copied file back to check it, and then removes the local copy; Lightroom's standard previews are rebuilt when needed, and its backups stay on this computer. **Reopen** copies the catalog back.

## Repo layout

| Path | What |
|---|---|
| `crates/core` | Shared Rust logic: card detection, scanning, grouping, folder names |
| `apps/desktop` | Desktop app for Windows and macOS (Tauri 2) |
| `apps/server` | NAS service (axum), runs in Docker on the Synology |
| `ui` | The app's screens (started from the prototype) |
| `lrplugin/CardIntake.lrplugin` | Lightroom Classic plug-in that adds a catalog's new photos from the NAS |
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
cargo run -p card_intake_desktop -- --health /Volumes/Untitled     # the card's details and health log (Windows may ask for admin rights)
cargo run -p card_intake_desktop -- --check-nas /Volumes/photos    # the NAS folder check
cargo run -p card_intake_desktop -- --shutter /Volumes/photos/2026 # shutter counts of every photo in a folder
cargo run -p card_intake_core --example card_storage -- E:\         # Windows: the card's bus, reader, details, serial and NVMe health log (through a JMicron bridge only when run as admin)
cargo run -p card_intake_core --example scan_card -- /Volumes/Untitled
cargo run -p card_intake_core --example count_photos -- /Volumes/photos /Volumes/old-Nikon  # counts photos outside the share
```

On Windows, use `E:\` instead of `/Volumes/Untitled` and `\\NAS\photos` instead of `/Volumes/photos`.

The import tests use temporary card/share fixtures and cover verified copies, sidecars, date/video selection, conflicts, corrupt read-backs, cancellation, parallel workers, retries, files already under another shoot title, changed sources, saved resume requests, history, and rejection of local destinations. Lightroom tests cover shoot and trip catalogs, the rule that only shoots without a catalog join a trip, Finish and Reopen, and installing the plug-in and presets; they also run the actual Lua plug-in in Fengari with a mocked Lightroom SDK (presets, skipped and refused files, DxO DNG stacking, disconnected folders, a busy catalog, and other catalogs). The shutter database tests cover syncing between computers, the JSON import, and damaged or newer databases. UI tests run the real page with a mocked Tauri bridge and require Node.js:

```sh
npm ci --prefix tests/ui
npm test --prefix tests/ui
cargo clippy --workspace --all-targets -- -D warnings
```

The NAS path is saved in `settings.json` in the app's config folder. Environment variables override it and the tool locations: `CARD_INTAKE_NAS`, `CARD_INTAKE_EXIFTOOL`, `CARD_INTAKE_SMARTCTL`. Cards, health readings and the most recent 100 import results are saved in `ledger.json`, and this computer's copy of the shutter count history in `shutter.sqlite`, in the app data folder (`%APPDATA%\io.github.semmmer.cardintake\` on Windows; `~/Library/Application Support/io.github.semmmer.cardintake/` on macOS). Older ledgers load with an empty import history.

The Windows-specific code lives in `crates/core`. `cargo test --workspace` builds and tests the native desktop backend on Windows; tests do not eject physical cards or write to configured NAS shares.

Installers and app bundles: `cargo install tauri-cli --version "^2"`, then `cargo tauri build` inside `apps/desktop`.

### NAS service on Synology

In Container Manager, create a project from `docker/compose.yaml` and set the photo share path in it (`/volume1/photos` by default).

### Lightroom

1. In the Lightroom Classic version you use, create an **empty catalog**, set its catalog settings the way you want every shoot's (for example **Automatically write changes into XMP**), and close it. Card Intake refuses a template Lightroom still has open.
2. In Card Intake, under **Lightroom**, open **Catalog template, folders and Lightroom**, enter the template path, and save. Lightroom Classic is found in its usual install folder. Catalogs default to `Pictures\Lightroom\Card Intake`; any local folder works. To have your develop presets and profiles in every catalog, enter their folder under **Presets and profiles to install**: Card Intake copies every `.xmp` in it to Lightroom's own preset folder (`%APPDATA%\Adobe\CameraRaw\Settings\Card Intake`), and updates changed ones.
3. Card Intake installs its plug-in in Lightroom's Modules folder (`%APPDATA%\Adobe\Lightroom\Modules\CardIntake.lrplugin`), which Lightroom loads by itself: restart Lightroom once. In **File → Plug-in Manager → Card Intake**, choose the develop preset for raw photos, the one for DxO DNGs, and the metadata preset. A copy older builds asked you to add by hand is removed.
4. After an import, use **Open in Lightroom** beside a shoot, or name a trip and use **Open as a trip** for all of the import's shoots (`2026-11-03_Japan`, after the first day). Only shoots without a catalog can join a trip; once in one, opening any of its shoots opens the trip. During an import you can press **Open in Lightroom when done** (and pick the first shoot or a trip), and Lightroom opens as soon as the import is verified. Recent imports shows each catalog: **On this PC** with **Open** and **Finish**, **Open in Lightroom**, or **Finished · on NAS** with **Reopen**.

When a catalog Card Intake made opens, the plug-in reads `<catalog>.card-intake.txt` next to it, which lists its shoot folders, and adds every raw photo and DxO PureRAW DNG in them that isn't in the catalog yet, in place, with the chosen presets; JPEG, HEIF and video stay out. It then builds Smart Previews for them. A DNG named after its raw (`DSC03518_DxO.dng`, `DSC03518_DxO_DeepPRIMEXD2s.dng`) is stacked on top of the raw and takes its rating, flag and colour label. Lightroom's plug-in API can't remove a photo from a catalog, so the raw is tagged `CardIntakeReplacedByDxO` (a keyword that is never exported) and the catalog's **To edit** smart collection leaves tagged photos out: edit from there and each shot appears once. Lightroom restarts when it switches catalogs, so this runs for every catalog you open; for one already open, when a second card or DxO's DNGs arrive, use **Library → Plug-in Extras → Add new files from NAS**. Nothing runs in the background. A share that isn't connected, or a file Lightroom refuses, is reported in a Lightroom dialog.

The browser-only `prototype/card-intake.html` remains a design demonstration. Real imports, persistent resume, and Lightroom launching run in the desktop app, whose UI is `ui/index.html`.
