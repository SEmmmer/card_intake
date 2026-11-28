# Card Intake

Import camera cards to a NAS, or to this computer on trips, one folder per shoot, and open each shoot in its own Lightroom Classic catalog.

Built for this workflow: Sony ARW → NAS over 10 GbE → Lightroom Classic (+ DxO PureRAW, Pixel Cake) → finished JPEGs in Immich.

> Status: built and tested on Windows; the import engine is shared with macOS. Real NAS transfers, physical ejection and the Lightroom plug-in still need end-to-end validation with configured hardware and Lightroom.

## What it does

- **Import a card** to `\\NAS\photos\2026\2026-11-02_Yosemite\`: one folder per capture date, four files at once, every copy read back and compared with the card (xxHash64). Photo XMP and Sony clip XML sidecars travel with their files, and a stopped import can resume.
- **Rate raws**: each proprietary raw gets an initial star rating in an XMP sidecar, from in-camera protection, the camera's own rating, or its ISO and shutter speed.
- **Track cards and cameras**: a shutter count history per camera, shared through the NAS; a CFexpress card's health log and serial number, read from the card itself.
- **Open in Lightroom**: one catalog per shoot or per trip, made from a template on this computer. A plug-in the app installs adds new raws and DxO DNGs in place from the NAS. **Finish** stores the catalog on the NAS; **Reopen** brings it back.
- **Deliver**: copies the finished JPEGs that are new, with their current Lightroom stars, into a folder of finished photos, and can ask Immich to rescan.
- **Trips**: without the NAS, import to this computer and to backup drives, and **Send to NAS** later.

## Folder layout on the NAS

```
\\NAS\photos\
  2026\
    2026-11-02_Yosemite\
      DSC03518.ARW, .xmp          originals + Lightroom edits
      DSC03518_DxO.dng            DxO PureRAW output, next to its ARW
      video\
        4k30\                     clips by spec, each with its Sony XML and proxy
        8k30\, 4k60\, 1080p60\    ...
        4k120-30\                 slow motion: shot at 120 fps, saved at 30
      topix\                     Lightroom JPEGs (choose any export folder names)
      final\                     PixCake JPEGs
      .card-intake-delivered.json the names Deliver gave each photo, and when
      _catalog\                   the shoot's Lightroom catalog once finished, or trip.txt naming its trip
    _trips\
      2026-11-03_Japan\           a finished trip catalog holding several shoots
  .card-intake\shutter.sqlite    the shared shutter count history

\\NAS\Picture\MyPhoto\              finished photos only: Immich's external library
  2026-11-02_Yosemite\             one folder per shoot, filled by Deliver
```

A clip's spec folder is its size and frame rate, from its Sony XML or ExifTool; a clip neither can read goes in `video` itself.

Lightroom Classic can't open catalogs on a network drive, so a catalog stays on this computer while you edit (`Pictures\Lightroom\Card Intake` by default). **Finish** copies it with its Smart Previews and AI mask data to `_catalog` (or `_trips`), reads every copied file back, and then removes the local copy.

## Import a card

1. Under **NAS folder**, enter the share's path (`\\NAS\photos` or a mapped drive) and **Save**. **Check** makes sure the folder exists, is on a network share, takes a 64 MB test file that reads back identical, has free space, and looks like the photo share.
2. Insert a card. The app finds `DCIM`, `M4ROOT/CLIP` and `PRIVATE/M4ROOT/CLIP`. Tick the capture dates and name the shoots; the paths shown are the destinations. A new card or camera asks for **Register** or **Not mine** first.
3. Leave **Check every file after copying** on and press **Import to NAS**.
4. Check the result and **Recent imports**. Originals stay on the card.

- **Folders that already exist.** A date that already has a shoot folder gets its name, so another card from the same day joins it. Files already in any of that date's folders are compared with the card and skipped when identical. A different photo with the same name is never replaced.
- **Through this computer.** With file checks on, a NAS import first reads the card into the offline folder, while other workers send the checked copies on to the NAS. Once every file is checked here, the card is done: it can be ejected, and Lightroom can open while the NAS catches up. If the upload stops, the complete laptop copy stays in Recent imports with **Send to NAS**. When the NAS has verified everything, the unchanged laptop copies are deleted. Without room on this computer, or with file checks off, the card goes straight to the NAS.
- **Stop and resume.** **Stop** keeps completed files and removes partial copies. **Resume import** carries on, also after the app restarts.
- **Eject.** **Eject the card when done** needs file checks on. **Eject** beside the card's name works at any time, and leaves a card alone if a different one is in the drive by then. On macOS it only asks you to eject the card yourself.
- Unselected shoots, excluded videos, clip thumbnails, `MEDIAPRO.XML`, LUTs and unrelated files are not imported.
- Only one import, delivery, backup or Send job runs at a time.

### A camera that sends photos over FTP

Under **Camera FTP folder**, save the folder the camera uploads to (`\\NAS\FTP`). Its subfolders are searched too; `#recycle`, `@eaDir` and other hidden folders are left out. When an import to the NAS selects shoots the FTP folder has photos of, two options appear:

- **Skip photos already in FTP** (on by default) leaves on the card each photo the FTP folder has a file of with the same name and size. `EM_04408_1.JPG`, the FTP server's second `EM_04408.JPG`, counts as the same name.
- **Merge FTP photos into this import** (off each time, and only with Skip on) moves the FTP folder's photos from the same camera (by EXIF serial number) taken on the selected days into the shoots, sorted by capture date like the card's, under the camera's own names. Before relying on an FTP copy, it is hashed and compared with the card's: one that differs isn't the card's photo, so the card's is imported and the FTP file stays. Each photo is copied (between two shares of one server, the server copies it), its copy read back, and only then is the FTP file deleted, with any identical duplicates. A photo the shoot already has is just deleted from FTP; one that differs from the shoot's is left with a warning. Raws are rated as from the card, and folders left empty in the FTP folder are removed.

Copy to this computer ignores the FTP folder: a trip's laptop copy holds every photo on the card.

## Trips without the NAS

Under **Offline copies**, choose the offline folder (`Pictures\Card Intake Offline` by default). Tick **Copy to this computer**, which the app does itself when a card is read without a confirmed NAS, and press **Import to this computer**. Photos get the same folders, sidecars, checks and ratings as on the NAS.

- **Backup drives.** **Check / Init** tests a separate local volume and remembers it; photos go in its `.card-intake-<uuid>` folder. **Back up now** copies every offline shoot there, and **Also copy to backup drives** does it as part of an import. Nothing on a backup drive is replaced or removed.
- **Send to NAS** in Recent imports sends an offline shoot to the share with the usual checks, from the laptop copy or, when that's gone, from a backup drive.
- **Delete laptop copy** appears once Send has finished; click it twice. It deletes only files unchanged since the import. Backup drives and the NAS are never touched.

## Not mine

A borrowed card or camera can be marked **Not mine** instead of registered. The choice is remembered on this computer only. Its photos import normally, but a Not-mine camera's shutter readings are dropped. Undo it under **Not mine (n)** at the bottom of My Cards and My Cameras.

## Initial raw ratings

Every import gives proprietary raws (ARW, NEF, NRW, 3FR) a star rating in a same-stem `.xmp` beside the destination raw. Nothing on the card is changed. The rules run in this order:

1. **Locked in camera** (Protect): 5 stars and a Pick request.
2. **Camera rating** of 1–5.
3. **Computed**: 5, minus the ISO deduction below, minus one point for each full stop the exposure is slower than `1 / (focal length × crop factor)` seconds. Exposures of 1 second or longer lose nothing for the shutter.

| Recorded ISO | Deduction |
|---|---:|
| ≤800 | 0 |
| 801–3200 | 1 |
| 3201–6400 | 2 |
| 6401–12800 | 3 |
| 12801–25600 | 4 |
| ≥25601 | 5 |

An existing non-zero rating, including a reject, is kept, and so are develop settings and keywords. Card Intake adds keywords under **Card Intake** (`Computed rating`, `High ISO -2`, `Slow shutter -1` and so on) and its own properties in the namespace `https://github.com/semmmer/card_intake/ns/1.0/`. The plug-in turns a Pick request into a Pick flag when it adds a new photo. Camera and computed ratings need ExifTool; without it the import still succeeds, with a warning.

## Lightroom

1. In Lightroom Classic, create an **empty catalog**, set its catalog settings the way you want every shoot's, and close it.
2. In Card Intake, under **Lightroom → Catalog template, folders and Lightroom**, enter the template's path and save. To have your develop presets and profiles in every catalog, enter their folder under **Presets and profiles to install**.
3. Card Intake installs its plug-in in Lightroom's Modules folder (`%APPDATA%\Adobe\Lightroom\Modules\CardIntake.lrplugin`): restart Lightroom once. In **File → Plug-in Manager → Card Intake**, choose the develop presets for raws and for DxO DNGs, and the metadata preset.
4. After an import, use **Open in Lightroom** beside a shoot, or name a trip and use **Open as a trip**. Only shoots without a catalog can join a trip.

When a catalog Card Intake made opens, the plug-in adds every raw and DxO PureRAW DNG in its shoot folders that isn't in the catalog yet, in place, with the chosen presets, and builds Smart Previews. A photo you remove from the catalog stays out: `<catalog>.card-intake-added.txt` remembers every file the catalog has had (it travels with Finish and Reopen), and only files it has never had are added. To bring a removed photo back, use **Library → Synchronize Folder**. JPEG, HEIF and video stay out. A DNG named after its raw is stacked on it and takes its rating, flag and colour label. While Card Intake is uploading, arriving photos are added every few seconds. At any other time, use **Library → Plug-in Extras → Add new files from NAS**.

**Card Intake (start-up helper)** in the Export dialog exports nothing: it is what makes Lightroom start the plug-in at launch. The plug-in logs to `%LOCALAPPDATA%\Adobe\Lightroom\Logs\LrClassicLogs\CardIntake.log`.

## Deliver and Immich

Export JPEGs from Lightroom Classic into any folder inside the shoot, retouch them in PixCake if needed, then press **Deliver** in Recent imports. It shows the finished JPEGs that are new and, once you confirm, copies them with their current Lightroom stars into the shoot's folder among your finished photos, such as `\\NAS\Picture\MyPhoto\2026-11-06_夜景`. A trip's button covers every shoot in the trip.

- Lightroom and PixCake JPEGs are recognised from their metadata, and a PixCake version replaces its Lightroom twin. Camera JPEGs, unrecognised JPEGs and raws are never delivered.
- Nothing in the finished photos folder is ever replaced or removed, so pressing Deliver again only offers what's new.
- Copies are read back with xxHash64, and a new folder is moved in with one rename, so Immich never sees half of it.

Under **Deliver → Finished photos folder and Immich**, choose the folder Immich's external library watches. Optionally enter the Immich URL and an **admin API key** with `library.read` and `library.update` permissions, then choose the library to rescan after each delivery. The key is kept only in this computer's `settings.json`, and a failed rescan doesn't fail the delivery.

## Cards and cameras

**Card health.** CFexpress cards are NVMe drives with their own health log (life used, spare blocks, data written and read, power cycles, unsafe shutdowns, media errors, temperature) and serial number. How the app reads them depends on the reader:

| | Seen as an NVMe drive (USB4 or Thunderbolt reader) | Behind a JMicron JMS583 USB bridge | Behind another USB bridge |
|---|---|---|---|
| Windows | Read on insertion, without admin rights | Read by the app, which asks for admin rights each time | With [smartctl](https://www.smartmontools.org), with admin rights |
| macOS | With smartmontools installed | No | No |

A card the app can reach itself is known by its serial number, so formatting it in the camera keeps its record. Behind other USB bridges, and for SD cards in USB readers, only the volume ID is known, and a formatted card shows up as new (a Sony MRW-G3 on a plain USB port, rather than USB4, is such a bridge). The next time the app reads the card's serial number, on insertion or with **Read card health**, that record folds back into the card's own, with its imports. SD cards keep no health log. **Card details** shows what any card and its reader reveal without admin rights: the reader, its link speed, the SD size class and lock switch, and file system warnings.

**Shutter counts.** ExifTool reads the count of every photo on an inserted card, and the app keeps one count range per camera and shooting day. **Read NAS photos** reads every photo already on the share, so the graph goes back to the first shoot.

- Only **My Cameras** keep a history. A camera or card seen for the first time is a **New Camera** or **New Card** until you **Register** it. **Remove** (click twice) deletes a camera's history on every computer, or forgets a card on this one.
- Readings are checked against each other and the stored history: a count never goes down, and can't rise faster than the body shoots. Readings that don't fit are left out.
- The history is a SQLite database in the photo share, `.card-intake/shutter.sqlite`, shared by every computer running Card Intake; each keeps its own copy for when the share isn't connected. Update every computer together: an older build refuses a newer database.
- Photos outside the share can be counted where they are: `cargo run -p card_intake_core --example count_photos -- \\NAS\photos "\\NAS\old\Nikon Transfer"`.

**Camera notes.**

- **Sony**: `ShutterCount` is the mechanical count. ExifTool defines no electronic counter, so that stays **Not reported**. The α7R VI's 500,000 is an unconfirmed life expectancy reference.
- **Nikon Z 9**: its shutter is electronic only, so the app charts `ShutterCount` as **Electronic shutter count** and shows no life percentage. Reading a real Z 9 card hasn't been tested.
- **Nikon Zf**: the mechanical count is charted against Nikon's 200,000 cycles, and the rest of `ShutterCount` is shown as **Electronic**.
- **Hasselblad X2D II 100C**: `.3FR` raws; ExifTool 13.59 finds no shutter count in them.
- **DJI Osmo Pocket 3**: no mechanical shutter. Its DNGs get no rating.

## Develop

Needs Rust (stable). On Windows: the MSVC toolchain, Visual Studio C++ Build Tools with a Windows SDK, and WebView2. On macOS: the Xcode command line tools.

```sh
cargo run -p card_intake_desktop                        # the app
cargo test --workspace                                  # Rust tests
npm ci --prefix tests/ui                                # once
npm test --prefix tests/ui                              # the page and the Lightroom plug-in (Node.js)
cargo clippy --workspace --all-targets -- -D warnings
```

Tests use temporary folders: they don't eject cards or write to configured shares.

**ExifTool** is needed for Deliver, shutter counts and computed ratings. On Windows, put `exiftool.exe` on `PATH` or set `CARD_INTAKE_EXIFTOOL` to its full path. On macOS and Linux, run `scripts/fetch-exiftool.sh` once.

**Installing on Windows.** `cargo install tauri-cli --version "^2" --locked` once, then `pwsh scripts/install-windows.ps1` builds the installer and installs it for this user without prompts, in `%LOCALAPPDATA%\Card Intake`. When the installed app is open, the script closes it, installs and opens it again; while it has a job running (an import, backup, Send, delivery or Finish, which the app notes in `%LOCALAPPDATA%\io.github.semmmer.cardintake\busy.json`), it waits for the job to end first. `-NoBuild` installs the installer built last. The installed app opens once: starting it again shows the window that's already open. `cargo run` builds start regardless. On other systems, `cargo tauri build` inside `apps/desktop` makes the bundles.

**Checks without a window.** Each prints JSON and exits; nothing is written to the card:

```sh
cargo run -p card_intake_desktop -- --report E:\              # what the app reads from a card
cargo run -p card_intake_desktop -- --health E:\              # the card's details and health log
cargo run -p card_intake_desktop -- --check-nas \\NAS\photos  # the NAS folder check
cargo run -p card_intake_desktop -- --shutter <folder>      # shutter counts of every photo in a folder
cargo run -p card_intake_desktop -- --deliver-plan <shoot> <finished photos> [catalog.lrcat]
cargo run -p card_intake_core --example card_storage -- E:\   # Windows: bus, reader, serial, health log
cargo run -p card_intake_core --example scan_card -- E:\
cargo run -p card_intake_core --example ftp_preview -- E:\ \\NAS\FTP  # what the camera's FTP folder has of a card
cargo run -p card_intake_core --example rate_card -- E:\ <temp folder> <exiftool>       # a rated import, into system temp only
cargo run -p card_intake_core --example deliver_temp -- <shoot> <finished photos> <exiftool>  # a real delivery, in system temp only
```

**The demo.** `prototype/card-intake.html` is the app's page with a demo backend, so it opens in any browser with sample data. After changing `ui/index.html`, run `node prototype/build-demo.cjs`; a test fails when the demo is out of date.

**Where things are kept.** `settings.json`, `ledger.json` (registered cards, their health readings and the last 100 imports) and this computer's `shutter.sqlite` are in `%APPDATA%\io.github.semmmer.cardintake\` (`~/Library/Application Support/io.github.semmmer.cardintake/` on macOS). `CARD_INTAKE_NAS`, `CARD_INTAKE_EXIFTOOL` and `CARD_INTAKE_SMARTCTL` override the NAS path and the tools' locations.

**The log.** The installed app has no console, so what it couldn't read, a start that failed and any panic go to `%LOCALAPPDATA%\io.github.semmmer.cardintake\logs\Card Intake.log` (`~/Library/Logs/io.github.semmmer.cardintake/` on macOS). A log over 1 MB starts again, and the one before it is kept as `Card Intake.old.log`. When the app can't start, because `settings.json` or `ledger.json` can't be read for instance, a message box says why, and the file is left as it is.

| Path | What |
|---|---|
| `crates/core` | Card detection, scanning, importing, ratings, shutter history, delivery |
| `apps/desktop` | The desktop app for Windows and macOS (Tauri 2) |
| `ui` | The app's page |
| `lrplugin/CardIntake.lrplugin` | The Lightroom Classic plug-in |
| `prototype` | The demo, built from the app's page |
| `tests/ui` | Tests of the page and the plug-in |
