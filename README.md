# Card Intake

Import camera cards to a NAS or to this computer on trips, one folder per shoot, and open each NAS shoot in its own Lightroom Classic catalog.

Built for this workflow: Sony ARW → NAS over 10 GbE → Lightroom Classic (+ DxO PureRAW, Pixel Cake) → finished JPEGs in Immich.

> Status: the desktop app imports real cards to a network share, groups shoots by capture date, and verifies copies with xxHash64. It copies four files at once, includes photo XMP and Sony clip XML sidecars, and shows progress with speed, time left, and files already on the NAS. Stopped imports can resume after an app restart. Windows supports a safe ejection attempt after verification. Each shoot (or trip) gets its own Lightroom catalog, which a plug-in Card Intake installs fills from the NAS, and **Finish** stores it on the NAS. **Deliver** shows the finished JPEGs that are new, and once you confirm, copies them with current Lightroom stars into a folder of finished photos (such as `MyPhoto\2026-11-06_夜景`) and can ask Immich to rescan. Card scanning, camera metadata, shared shutter history, CFexpress health and serial numbers, and NAS checks use real data. The NAS service remains a draft. Real NAS transfers, physical ejection, and the Lightroom plug-in still need end-to-end validation with configured hardware and Lightroom.

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
- **Card:** the SD class its size belongs to (SDSC up to 2 GB, SDHC 2–32 GB, SDXC 32 GB–2 TB, SDUC beyond), and a warning when its lock switch is on.
- **Format:** from the file system's own boot sector, a warning when it wasn't closed cleanly (exFAT's VolumeDirty, FAT's clean-shutdown bit) or when its last writer met bad sectors (exFAT's MediaFailure, FAT's hard-error bit).

The app also reads the card's exact size, partition table and file system details, but doesn't show them. A CFexpress card with nothing to warn about has no Card details section.

A card is taken for an SD card when it's in an SD host controller, or when a USB reader shows it as a removable medium behind no NVMe bridge. Its size must also be a whole number of 512 KiB steps, the unit SD cards count their capacity in; a CFexpress card's NVMe size isn't. For an SD card, **Card health** says it keeps no health log, instead of offering smartctl and an admin prompt.

What an SD card in a USB reader can't tell: its CID (maker, product name, serial number, manufacture date), its CSD, and its SD Status (speed class, UHS and video speed classes). A USB reader's mass-storage interface doesn't pass them on. The readers that can fetch them do it with undocumented vendor commands, which the app doesn't send, because an unknown command could as well rewrite the reader's firmware. Only an SD host controller, such as a laptop's built-in slot, shows the card itself. So an SD card in a USB reader is known by its volume ID, and an SD card formatted in the camera shows up as a new card.

Tested with a 256 GB SDXC card from a DJI Osmo Pocket 3 in a Genesys Logic USB 3 reader (05E3:0764, two slots). The Osmo Pocket 3 writes its model number, `PP-101`, in EXIF; the camera list calls it Osmo Pocket 3 and knows it has no mechanical shutter. `cargo run -p card_intake_desktop -- --health F:\` prints the details with the health answer.

## Nikon Z 9 cards

A card formatted in a Nikon Z 9 is read like a Sony card, with two things that belong to that body:

- **Files.** Photos are NEF (NRW on older Nikon bodies) or JPEG, plus HEIF stills from the bodies that write them; clips are MOV, MP4 or N-RAW `NEV`, and Lightroom's XMP sidecars travel with their photos. Nikon keeps them in its own folder name inside `DCIM` (`100NCZ_9` on a Z 9 using the default `NCZ_9` folder name), and the whole `DCIM` tree is read. Clips carry no Sony XML sidecar, so a clip's capture date falls back to its file time.
- **Shutter count.** The Z 9's shutter is electronic only (Nikon's [Z 9 specifications](https://onlinemanual.nikonimglib.com/z9/en/specifications_348.html)). Its photos carry the count in `ShutterCount` and write 0 in `MechanicalShutterCount`, the tag a body with a mechanical shutter counts in. The app reads the Z 9's `ShutterCount` and ignores the mechanical 0, so its card adds the electronic count to the shutter history rather than a flat 0; the camera list labels the figure **Electronic shutter count**. Nikon publishes no shutter rating for a shutter it doesn't fit, so a Z 9 shows no life percentage. Other bodies keep taking their mechanical count first.

The Z 9 takes CFexpress Type B and XQD cards. A CFexpress card in a Thunderbolt or USB4 reader that shows the card as an NVMe drive reports its own model, serial number and health log on insertion, without smartctl or admin rights. In a USB reader with a JMicron JMS583 bridge the app reads the same things itself, with admin rights; behind other USB bridges it is read with smartctl, like any card. An XQD card is not an NVMe drive and keeps no NVMe health log, so it is known by its volume ID only. The card's type name comes from its own model where the maker encodes it (Sony's `CEA…` is Type A, `CEB…` Type B); other makes are listed as **CFexpress**. Reading a real Z 9 card has not been tested here: the file handling, the serial-number identity and the health log follow the paths above, and `cargo run -p card_intake_core --example card_storage -- E:\` prints what Windows reports for the inserted card.

## Nikon Zf

The Zf is read like the Z 9 (same NEF, JPEG, HEIF and MOV/MP4 files, the whole `DCIM` tree), with its own folder name and shutter:

- **Cards.** The Zf takes an SD card and a microSD card. Its default folder is `NCZ_F` (e.g. `DCIM/106NCZ_F`). SD cards in a USB reader show what [Card details](#card-details-any-card-sd-included) can read; they keep no health log.
- **Shutter count.** The Zf has a mechanical shutter, an electronic front curtain and a silent electronic shutter, and its NEFs carry two counters: `MechanicalShutterCount` for the mechanical shutter and `ShutterCount` for every release. ExifTool's [Nikon tags](https://exiftool.org/TagNames/Nikon.html) describe the latter as including "both mechanical and electronic shutter activations". The app charts the mechanical count, against the 200,000 cycles Nikon says the Zf's shutter "has been tested to" ([Nikon USA, Z f](https://www.nikonusa.com/p/z-f/1761/overview)). The difference between the two counters is shown as **Electronic**: 15,557 on a Zf photo with 25,602 releases and 10,045 mechanical ones.

  Checked against 16,539 Zf photos (firmware 3.01): `ShutterCount` is never below `MechanicalShutterCount`. Of 15,881 back-to-back releases, every one taken with the mechanical shutter, electronic front curtain or auto mode added 1 to the mechanical count. Every one taken with the electronic shutter (Nikon's `ShutterMode` 16; 390 of them) added nothing.

Zf photos read by an earlier Card Intake have no electronic count in the shutter history. Read their folders again to add it:

```sh
cargo run -p card_intake_core --example count_photos -- --again \\NAS\photos "\\NAS\old\Nikon Transfer 2026 5"
```

## Shutter counts

The shutter count of every photo on an inserted card is read with ExifTool (in parallel, about a second for 200 raw files) and kept per camera, one count range per shooting day. **Read NAS photos** (under My Cameras) reads every photo already on the share too, so the graph goes back to the first imported shoot; folders read before are skipped unless their photos change.

Only **My Cameras** keep a history. A camera whose photos are read for the first time (a camera borrowed or tried in a store, with your card in it) shows as **New Camera**, on the desk and at the top of the list, with **Register to My Cameras** below its serial number. Its counts are shown but not saved; registering it stores what was already read. A camera is known by its serial number, or by Sony's internal serial number when its photos don't carry the serial (the camera's **Write Serial Number** setting is off). Registration is shared through the NAS like the counts. A NAS folder holding photos from a camera that isn't registered is read again the next time, so those photos count once the camera is registered. Cards work the same way: a card the app hasn't seen is a **New Card** with **Register to My Cards**, kept while the app runs and saved in `ledger.json` once registered. Cards and cameras already in the history before registration existed are registered.

Before readings are stored, each camera's photos from one card or folder are checked against each other **and its stored history**, before they become day ranges. A count never goes down, and can't rise faster than the body shoots (its top frame rate plus 10%, from `crates/core/src/cameras.rs`: Zf 30, α7R VI 30, Z 9 120, α9 III 120, EOS R3 195 frames a second; a model without its own rate gets the fastest listed). Whole-second EXIF times get one extra second's frame budget for bursts; there is no count allowance for clock changes. A clock adjustment can therefore leave out genuine readings, which is safer than permanently widening a day with a bad count.

ExifTool guesses the maker-note layout for the α7R VI, α1 II and α7 V when it thinks the mechanical shutter was used. On the α7R VI, that guess has produced 4,194,304 and 1,024 on a day whose real counts were 2 to 30. Those models also need support from a different, nearby count in the read (within one second's frame budget: 33 for the α7R VI), or from a consistent stored range or nearby stored count. An isolated last photo cannot extend the history by hundreds simply because the time gap permits it, and repeated copies of one bogus constant don't support each other. A sparse read may identify a new camera without contributing any counts. These checks cannot identify a false number that fits all the available evidence; there is no reliable shutter-type tag for this model.

Stored days constrain earlier and later readings using their low/high counts and first/last shot times. A stored high that contradicts the next day's low is not used as a checking bound, so a previously corrupted high doesn't make good new readings look bad. The graph still shows the stored ranges: day summaries alone cannot reliably distinguish corruption from a shutter-count reset after replacement. Existing bad ranges need a separate repair from the photos; this filter does not rewrite them or hide genuine resets.

**Remove** appears on registered cameras and cards. Click it again within five seconds to confirm. Removing a camera deletes its shutter history and shares the removal through the NAS, including with computers that reconnect later. If seen again it is a **New Camera**. Registering it again starts a fresh history from readings collected after removal; old databases, pending batches and legacy JSON cannot bring its previous history back. Removing a card forgets its record on this computer only, including its old health readings and insertion totals; imports that mention it stay in **Recent imports**. A card still in the reader is seen afresh as a **New Card**.

The history is a SQLite database in the photo share, `.card-intake/shutter.sqlite`, so every computer running Card Intake (Windows or Mac) reads and adds to the same history. Each computer keeps its own copy in the app data folder (`shutter.sqlite`) for when the share isn't connected. Every row records the write that last changed it, so a sync sends only this computer's rows changed since its last sync and fetches only the rows other computers changed since then; a history of many years syncs as fast as a day's. Merging is idempotent: the same photos read on two computers count once.

The shared database never uses SQLite's WAL mode, which needs shared memory a network share doesn't provide, and writes and schema migrations also hold a lock file (`.card-intake/shutter.lock`), because SQLite's own locks aren't reliable on every network share. A damaged database, or one written by a newer Card Intake, is left untouched with a message. The JSON file older builds kept (`shutter-counts.json`, in the share and in app data) is merged in automatically, and again whenever an older build changes it, but only into a camera's original registration.

SQLite schema **2** upgrades schema 1 while preserving camera IDs, days, database IDs and write numbers. It retains a small removal row for each removed camera. Removing or re-registering advances that camera's generation; rows from older generations are ignored, while rows within one generation still merge by lowest/highest count. Concurrent generations have a stable tie-break, with removal winning equal generation numbers. **Update every computer sharing the database.** Schema 1 builds refuse schema 2 and cannot sync until updated; keeping version 1 would let them ignore removals and restore deleted history. Serial-less bodies of the same model are kept separate by their internal serial numbers.

Photos kept outside the photo share — an old archive, for instance — can be added to the same history without moving or copying anything:

```sh
cargo run -p card_intake_core --example count_photos -- \\NAS\photos "\\NAS\old\Nikon Transfer 2026 5"
```

The script reads each folder once with the app's own ExifTool pass, merges the counts into the share's `.card-intake/shutter.sqlite` the way the app syncs, and writes nothing to the photo folders. Folders whose photos haven't changed are skipped on later runs (`--again` reads them anyway, for counts a newer Card Intake reads), and progress is saved every 20 seconds so an interrupted run keeps what it read. A folder outside the share is remembered in the same database under an `extra:` key, which the app itself never looks up.

The main display is **Mechanical shutter count**, with a smaller **Electronic** count beside it (**Electronic shutter count** as the only figure on a body that has no mechanical shutter). The smaller figure appears when a file carries an explicit electronic counter, or, on a Nikon that writes both `ShutterCount` and `MechanicalShutterCount`, their difference ([the Zf](#nikon-zf)). ExifTool 13.59 defines no `ElectronicShutterCount` tag, so it stays **Not reported** for the Sony bodies checked here. Alternative `ShutterCount2/3` and `ImageCount` values are not treated as independent electronic counters. ExifTool identifies the α7R VI's `ShutterCount` as mechanical actuations in its [Sony tag definitions](https://github.com/exiftool/exiftool/blob/master/lib/Image/ExifTool/Sony.pm). Zero mechanical actuations is a valid reading. A body that has no mechanical shutter at all, such as the Nikon Z 9, writes 0 in that tag on every photo and keeps its real count in `ShutterCount`, so the app charts that instead ([see above](#nikon-z-9-cards)).

Manufacturer ratings remain separate from unconfirmed reference values (`crates/core/src/cameras.rs`). The user-suggested **500,000** value for α7R VI is displayed as an **unconfirmed life expectancy reference**. It was not confirmed in [Sony's ILCE-7RM6 specifications](https://www.sony.com/electronics/support/e-mount-body-ilce-7-series/ilce-7rm6/specifications) or the Japanese feature pages checked on 2026-11-07. Legacy JSON format version 2 preserves existing shutter history and re-reads old folder snapshots once to collect any electronic counters; it is separate from the SQLite schema version above.

## NAS folder

Set the photo share's path in the app (NAS folder → Save). **Check** makes sure it's the right place before anything is copied there:

- the folder exists (is the share mounted?)
- it's on a network share, not this computer's own disk, so a missing mount doesn't fill the local disk
- a 64 MB test file writes, reads back identical, and is deleted
- there's enough free space
- it looks like the photo share (year folders such as `2026` with shoot folders inside)

## Import a card on Windows

1. For a NAS import, connect the share in Explorer. Enter its UNC path (for example `\\NAS\photos`) or mapped drive path (`Z:\photos`) under **NAS folder**, then **Save**. NAS imports require an existing, confirmed network folder. Without the NAS, use **Copy to this computer** instead (see below).
2. Insert a camera card. The app detects `DCIM`, `M4ROOT/CLIP`, and `PRIVATE/M4ROOT/CLIP`, including video-only Sony cards. Select capture dates and name the shoots. The displayed paths show the destinations.
3. Leave **Check every file after copying** enabled. Choose whether to include videos and whether to attempt ejection, then click **Import to NAS** (or **Import to this computer**). Before importing, choose **Register** or **Not mine** for any new card or camera. Photos and their XMP files go in the shoot folder; clips and their XML/XMP sidecars go in its `video` folder.
4. Check the result and **Recent imports**. The result distinguishes copied, identical/skipped, and verified files. Originals remain on the card. Unselected shoots, excluded videos, proxies and unrelated files are not imported.

**Shoot folders that already exist.** When the card is read, the app looks up each capture date's folders on the NAS (`2026/2026-11-06` and `2026/2026-11-06_<title>`). A date that already has a folder gets that title in the shoot name, so another card from the same day goes into the same folder; the card shows how many of its photos and clips are already on the NAS (same name and size), and the summary counts only the new ones. Clear the name or type another one and a new folder is made, but files already in the date's other folders are not copied again: the import compares each one with the card (xxHash64) and skips it when identical, and copies it when it's a different photo with the same name. A photo's or clip's sidecar goes with it into the folder where it already is. The done panel lists every folder that holds the files.

Four files are copied at once: a CFexpress card reads about twice as fast with parallel reads (measured on a CEA-G240T in an MRW-G3 on USB4: 837 MB/s with one stream, 1,647 MB/s with two to four), and one file's read-back overlaps the next one's copy. The progress line shows the card's read speed and the time left, and each import's average speed is kept in its history.

**Through this computer.** With **Check every file after copying** on, an import to the NAS first reads the card into this computer's offline folder (see [Trips without the NAS](#trips-without-the-nas)). Four workers read and check the files, and write each raw's initial rating beside it as its card sidecars finish. Separate workers send those checked copies to the NAS at the same time. Four write files while eight others finish them: set the time, flush once, read back without this computer's file cache on Windows and macOS, and compare with the card's checksum before publishing. A bounded backlog keeps the number of unfinished files small when the NAS is slow to commit. A new raw becomes visible only after its rated XMP has arrived too. Existing NAS sidecars keep their edits and non-zero ratings.

When every selected file is checked on this computer and the ratings are written, the card is done. Its local file list and an offline history entry are saved first, so an interrupted upload can be finished with **Send to NAS** even after a power loss. **Eject the card when done** now attempts ejection then; otherwise **Eject** starts working. Nothing reads the card again. The card's resume request is removed because finishing from here uses the laptop copy.

The same import keeps sending to the NAS. Its progress shows that the card is read and checked, whether it was ejected, and how far the NAS has to catch up, with recent speed and time left. You can **Open in Lightroom** beside each shoot, or open several shoots as a trip, as soon as the card is done. Lightroom uses the real NAS paths and fills in as photos arrive; a listed shoot folder can be absent until its first file lands. Only one card can be imported at a time, so a newly inserted card waits until this upload ends.

The NAS keeps the usual rules: an identical photo in the date's other folders stays there and its sidecars go next to it; a different photo with the same name is never replaced. If the NAS fails, or you press **Stop upload**, the complete, rated laptop copy stays in Recent imports with **Send to NAS**. The card may already have been ejected. When the NAS has verified everything, the unchanged laptop copies are deleted and the same history entry becomes the final NAS import. Files changed meanwhile stay on this computer and are reported, with **Delete laptop copy** available later.

Without room on this computer for the card, or without a usable offline folder, the card goes straight to the NAS as before, with a note in the result. With file checks off, imports also go straight to the NAS. Offline imports and direct NAS imports finish their card work at the end of the import.

Every result saves timings in history: time and bytes reading the card, upload time and bytes, and the workers' time writing, flushing, reading back, publishing, working on NAS sidecars and waiting for backlog room, plus ExifTool's time. The done panel shows one small speed line. The history's average speed measures the card read up to card done; it no longer includes the rest of a background upload.

Copies are staged in unique `.card-intake-*.part` files, flushed, and (when enabled) read back and compared with the source's xxHash64 checksum. Only complete copies are published under their final filenames. An existing file is skipped only after its size and checksum match; different contents stop the import without replacing it. Two card files targeting the same filename are rejected before any copying starts.

**Stop** before card done cancels the card import, removes its partial copies when the share is reachable, and keeps completed files. After card done, **Stop upload** keeps the complete laptop import and **Send to NAS** finishes it later. For a stop before card done, the desk shows how far the import got, and **Resume import** repeats the same selection: completed files are hashed and skipped, unfinished ones copied from the beginning. The selection and options are also saved before copying begins, so after an app restart the same card comes back with its shoot names and options filled in and **Resume import** ready. A new import replaces the saved one. Closing the window during an import requests cancellation and leaves the result visible; close it again after the worker finishes. A process crash or disconnected share can leave a `.part` file; retries ignore it and create a fresh staging file.

Automatic ejection requires verification. Windows must be able to lock the volume before dismounting and ejecting it. A busy reader, unsupported eject command or permission error is reported separately from the successful copy; use Explorer to eject in that case.

**Eject** beside the card's name ejects it the same safe way at any time: before an import, after one, or without importing. It waits until the card is done, or while the app reads the card's shutter counts or health, and it leaves a card alone if a different one has been put in that drive since the screen was drawn. On macOS it only asks you to eject the card yourself, for now.

The import engine is shared with the macOS app, but the new desktop workflow has been built and tested on Windows. Physical reader ejection and a real NAS transfer still need validation with your hardware.

## Trips without the NAS

Under **Offline copies**, choose the **Offline folder on this computer**. It defaults to `Pictures\Card Intake Offline` (`~/Pictures/Card Intake Offline` on a Mac). **Check** shows whether the folder exists or can be created, whether it is on this computer's own disk, and its free space. **Save** checks it again and remembers the path. Network shares, camera cards and volumes carrying a Card Intake backup marker are refused. The folder is created when an import needs it; its volume is checked again while copying.

On the desk, tick **Copy to this computer** and press **Import to this computer**. The app ticks it when a card is read without a confirmed NAS connection, and leaves it ticked when the NAS comes back. It can only be unticked with the NAS connected. Photos go to the offline folder instead of the NAS, with the same date folders, `video` folders, sidecars, verification and initial raw ratings. Existing shoots and files are found there too, so a second card joins the same local shoot. **Resume import** keeps the saved folder, shoot names and backup choices. Each raw is rated as its copy and card sidecars finish, with no rating pass at the end. Shutter counts stay in this computer's database and sync with the photo share later.

**Backup drives.** Connect a separate local volume, choose it under **Add a backup drive** (or enter a folder on it), and press **Check / Init**. Card Intake refuses the system volume, the offline folder's volume, camera cards and network shares. It checks free space against files in the offline folder that aren't on the drive, and writes, reads back and deletes a 64 MB test file. FAT32 gets a warning about files over 4 GB. A failed check never initializes or remembers the drive.

A successful check creates one folder at the volume root, `.card-intake-<uuid>`, with a `drive.json` identifying the drive. Photos go inside that folder, with the same year and shoot layout. The folder gets no Windows hidden attribute. A drive already initialized elsewhere is adopted. Card Intake finds remembered drives by their marker when their drive letter changes. **Forget** only removes the remembered drive; its marker and photos stay. **Eject** waits until no copy job is using the drive.

**Back up now** copies every shoot in the offline folder to that drive. Its progress bar appears under the drive. Plugging in a drive adds it to the list of volumes to pick within a few seconds. Completed copies are kept if you press **Stop**; pressing **Back up now** again resumes. Files with the same size and time (within two seconds) are skipped; with the same size but a different time, their checksums must match. A different file under the same name is an error, and nothing on a backup drive is replaced or removed. New files are staged beside their final names, flushed, read back with xxHash64, given the source time and published without replacing anything. Free space and FAT32's file size limit are checked before copying. OS junk, Card Intake temporary files, `.part` files and empty folders are left out.

For a card import, **Also copy to backup drives** requires **Copy to this computer**, **Check every file after copying**, and a connected remembered drive. It uses every remembered drive connected when the import starts. Space on this computer and every drive, and FAT32's file size limit, are checked before copying originals. Once the local files and ratings are complete, the drives are filled from those verified copies, checked against the card checksums kept during import; the card isn't read again for backup. Each drive's result is shown. If one fails, the local copy still counts, the card isn't ejected, and **Resume import** retries the same drives (reconnect them first).

**Send to NAS.** Offline shoots have **Send to NAS** in Recent imports, available with a confirmed network share. It sends the whole shoot folders, merges into existing folders with the same names, checks every copy and never replaces an existing NAS file. Identical files in other folders for the same date are skipped and their folders reported. Different photos or clips stop the job; a different XMP/XML sidecar keeps the NAS's version and is reported. Its progress bar appears above Recent imports. **Stop**, then **Send to NAS** again, checks and skips completed files. The laptop's shoot folder is used when present; otherwise the first connected remembered backup drive holding that shoot is used, and the result names the source. A local file list saved after import makes Send stop if an original or sidecar has gone missing from the chosen source. Restore a partly deleted laptop shoot from backup before sending it; an absent shoot can use the backup directly. Only one import, delivery, backup or Send job can run at a time.

Once Send has checked every file, the row has the usual Lightroom and Deliver controls and can merge with other imports of the same shoot or trip. Unsent offline imports are kept even when history exceeds its usual 100 records. The newest result is always kept too, so a new NAS import still has its Lightroom controls when the protected offline records already fill that limit.

**Delete laptop copy** appears only after Send has finished. Click again within five seconds to confirm. It uses the verified file list saved in this computer's app data folder, and only deletes files whose size and exact modification time still match. Changed files, extra files and sidecars whose different NAS version was kept stay on this computer and are reported. Empty folders are removed up to the shoot folder; the year stays. Backup drives and the NAS are never touched. When nothing is left, the row says **Laptop copy deleted**.

## Not mine

Before **Import** or **Resume import**, a new card or the desk's new camera asks for **Register** or **Not mine**. There is no default choice; every shown row needs a choice. **Cancel** changes nothing. The choices are applied before importing, and an error stops the import from starting.

**Not mine** is remembered on this computer only. The desk shows a muted **Not mine** chip, and the card or camera stays out of the New lists. Photos still import normally. A Not-mine camera's shutter readings are dropped, including readings already held while the app was open. Its NAS photo folders are still read again next time, so registering it later can count them.

At the bottom of **My Cards** and **My Cameras**, unfold **Not mine (n)** to undo a choice. **Forget** makes it new again, so it asks on the next import. **Register** removes the choice and uses the usual registration. A card can only be registered if this app has seen it in the current session; otherwise the app says **Insert the card to register it.** These folded rows are remembered like the other fold buttons.

## Initial raw ratings

Every import gives proprietary raw photos an initial Lightroom Classic star rating in a same-stem `.xmp` sidecar beside the destination raw. This is always on for photos. The scanner currently recognises ARW, NEF and NRW as proprietary raws; any proprietary photo format added to the scanner also uses this rule. DNG (including the DJI Osmo Pocket 3), JPEG, HEIF/HIF and videos are skipped. RAW+JPEG pairs are rated through the raw. The source raw is only read, and neither originals nor sidecars on the card are changed.

The rules run in this order:

1. **Locked in camera:** Sony/Nikon Protect is the source file's read-only attribute on Windows, write permission bits on Unix, or the immutable/Locked flags on macOS. It gives **5 stars** and a Card Intake **Pick request**, ahead of any camera rating. Destination permissions and EXIF protection tags are not used.
2. **Camera rating:** an embedded XMP or EXIF/maker-note `Rating` of 1–5 is used. Zero, reject (-1) and missing ratings fall through.
3. **Computed:** start at 5, subtract the ISO and shutter deductions, and clamp to 0–5.

| Recorded ISO | Deduction |
|---|---:|
| ≤800 | 0 |
| 801–3200 | 1 |
| 3201–6400 | 2 |
| 6401–12800 | 3 |
| 12801–25600 | 4 |
| ≥25601 | 5 |

ISO comes from EXIF ISO/PhotographicSensitivity, with ISOSpeed or RecommendedExposureIndex for the 65535 overflow marker and higher sensitivities. No usable ISO means no ISO deduction and an `ISO not recorded` keyword.

Safe shutter time is `1 / (focal length × crop factor)` seconds. Crop factor comes from ExifTool's `Composite:ScaleFactor35efl` and the recorded 35 mm equivalent focal length; it is never a user setting. Sony's `APS-CSizeCapture=On` supplies a 1.5× minimum when the composite misses crop capture. Full-frame α7R VI raws on a real card report 1.0×; real RAW samples from an α6000 and an α7C II in APS-C mode report about 1.487× (58/39 mm) and 1.5× (36/24 mm). Crop factors are not rounded from the metadata.

An exposure equal to or faster than the safe time loses nothing. Slower exposures lose `floor(log2(exposure / safe time))` points, with a small floating-point tolerance at exact stops. Exposures of **1 second or longer** have no shutter deduction; ISO still counts. Missing or zero focal length, crop factor or exposure time skips this part and records why, with a `Shutter not checked` keyword.

Metadata is read from source raws in copy order, in batches of 16 through one persistent ExifTool process with UTF-8 arguments. Each batch becomes available independently. A worker writes the rating as soon as the raw and its card sidecars are copied and checked; there is no serial rating pass at the end. The card's read-only attribute is captured while the card is being read. For an import through this computer, both the laptop copy and the NAS sidecar are rated, and the NAS sidecar is ready before a new raw is published. Identical raws already at the destination or in another shoot folder for the same date are rated there too. No second folder or second raw copy is needed. Missing ExifTool, unreadable metadata or an unsafe sidecar produces a warning and never fails the file import; importing again retries the rating step. Protection can still be honoured without ExifTool.

An existing nonzero `xmp:Rating`, including a reject, is kept. A missing rating or zero is filled in. Existing develop settings, keywords, other namespaces, comments and packet padding are preserved. Card Intake adds keyword leaves under **Card Intake** using `dc:subject` (flat leaves plus the parent) and `lr:hierarchicalSubject` (`Card Intake|…`), without adding duplicates: `Locked in camera`, `Camera rating`, `Computed rating`, `High ISO -2`, `Slow shutter -1`, `Shutter not checked`, and `ISO not recorded`, as applicable. Deduction names use an ASCII hyphen-minus.

The stable Card Intake XMP namespace is **`https://github.com/semmmer/card_intake/ns/1.0/`**, conventionally `ci`. Its properties are `RatingSource`, `SuggestedRating`, `ExistingRatingKept`, `Pick`, `Version` and `WrittenAt` (UTC); computed ratings also record `ISO`, `ISODeduction`, `ISOSkipped`, `ExposureTime` (seconds), `FocalLength` (mm), `CropFactor`, `SafeShutterSpeed` (seconds), `ShutterDeduction` and `ShutterSkipped` when applicable. The namespace URI identifies the schema; it need not be fetched. Sidecars use an XMP packet wrapper, `x:xmpmeta` and RDF. XML is parsed with DTDs disabled and only the relevant properties are patched. Unparseable or ambiguous packets are left untouched with a warning; updates are flushed to a temporary file in the same folder and renamed into place.

Lightroom Classic reads the sidecar's rating and keywords for proprietary raws ([Adobe metadata documentation](https://helpx.adobe.com/lightroom-classic/desktop/organize-photos-in-lightroom-classic/metadata-basics-actions.html)). Pick flags need the plug-in: when it **adds a new photo**, it reads `ci:Pick="True"` by namespace and sets `pickStatus=1` only if the photo is unflagged. It preserves existing Picks, rejects and photos already in the catalog. A DxO DNG stacked later inherits its raw's Pick through the existing plug-in behavior. Metadata presets chosen in Lightroom can affect the imported metadata, so keep rating/keyword fields out of an import preset if you want the sidecar values.

The import result and done panel report the locked/camera/computed counts, ratings kept and any warnings. For a local validation import without app settings, history or ejection, create an empty folder under the system temporary directory and run:

```powershell
$env:CARGO_TARGET_DIR = 'target\codex'
cargo run -p card_intake_core --example rate_card -- E:\ <existing-temp-folder> <path-to-exiftool.exe>
```

The example refuses destinations outside the system temporary directory and verifies all copied raws. It leaves the card read-only and prints the result, including the 0–5 star distribution, as JSON.

## Workflow and next steps

- **Import a card** to `\\NAS\photos\2026\2026-11-02_Yosemite\`, grouped by capture date, with optional xxHash64 read-back verification (enabled by default).
- **Track cards and cameras**: camera serial number and shutter count history from EXIF, shared through the NAS; CFexpress health (data written, life used, power cycles) and serial number from the card itself; for SD cards, this app's own counts.
- **Open in Lightroom**: one catalog per shoot, or a trip catalog for several shoots, made from a closed template on this computer. The Card Intake plug-in adds new raw photos and DxO DNGs in place from the NAS whenever the catalog opens, with your presets, and builds Smart Previews. **Finish** stores a closed catalog on the NAS; opening the shoot again brings it back.
- **Deliver**: export JPEGs from Lightroom Classic into any folder inside the shoot; retouch portraits in PixCake (Pixel Cake) if needed. Press **Deliver** in Recent imports: it shows thumbnails of the finished JPEGs that are new, and after you confirm, copies them with their current Lightroom stars into the shoot's folder among your finished photos, e.g. `\\NAS\Picture\MyPhoto\2026-11-06_夜景`, then asks Immich to rescan. A trip's button covers every shoot in that trip's saved list, one folder per shoot. Deliver works before a catalog is created, while it is open, and after Finish; it is separate from Finish.
- **Recent imports** shows imports whose only catalog is the same shoot or trip (a second card into the same shoot, say) as one row with their totals.

## Folder layout on the NAS

```
\\NAS\photos\
  2026\
    2026-11-02_Yosemite\
      DSC03518.ARW, .xmp          originals + Lightroom edits
      DSC03518_DxO.dng            DxO PureRAW output, next to its ARW
      video\                     original clips + Sony XML sidecars
      topix\                     Lightroom JPEGs (choose any export folder names)
      final\                     PixCake JPEGs (FINAL or any other name works too)
      .card-intake-delivered.json the names Deliver gave each photo, and when
      _catalog\                   the shoot's Lightroom catalog once finished, or trip.txt naming its trip
    _trips\
      2026-11-03_Japan\           a finished trip catalog holding several shoots

\\NAS\Picture\MyPhoto\              finished photos only: Immich's external library
  2026-11-02_Yosemite\             one folder per shoot, filled by Deliver
```

Deliver looks through the whole shoot, including export folders named freely. It skips `_catalog`, `video`, hidden/system folders, `@eaDir`, `#recycle` and `.lrdata` folders. It recognises Lightroom and PixCake from each JPEG's metadata. PixCake replaces its Lightroom twin (DocumentID first, then the stem with PixCake's `_(N)` removed); the newest version wins. Camera JPEGs and unrecognised JPEGs are skipped, and the confirmation says why. Raws are never delivered. Names lose PixCake's `_(N)` and DxO's `-DxO_…` suffix, use `.jpg`, and get `-2`, `-3` only when two photos would share a name. The shoot's `.card-intake-delivered.json` keeps each photo's name across deliveries.

Nothing in the finished photos folder is ever replaced or removed, whoever put it there. A name that's already there is skipped, so pressing Deliver again only offers what's new, and says so when nothing is. A photo you deleted from the finished photos folder is offered again under its old name. The confirmation lists exactly what will be copied; if the shoot changed meanwhile, Deliver asks you to check again instead of copying something else.

Copies are made in a hidden `.card-intake-delivering` folder inside the shoot, read back with xxHash64 and given their stars. A new folder is then moved into the finished photos folder in one rename, so Immich never sees half of it; into a folder that's already there (such as one you filled by hand), each file is moved on its own. Moves never replace anything: if a file with the same name appears meanwhile, the copy is dropped. The shoot and the finished photos folder must be on the same share for these moves (as `\\NAS\Picture\Sony Transfer` and `\\NAS\Picture\MyPhoto` are); otherwise each file is copied beside its final name under a hidden name, checked and renamed. Current stars come from a temporary copy of the Lightroom catalog and its WAL; the original catalog is never opened. Without a usable catalog match, the raw's XMP sidecar comes next, then the DxO DNG's embedded rating, then the exported JPEG's own stars. Only the delivered copy is edited. Last delivery shows each photo's source, stars and rating source, skipped JPEGs, warnings and errors.

Lightroom Classic can't open catalogs on a network drive, so catalogs stay on this computer while you edit: `<catalog folder>\2026\2026-11-06_Yosemite\2026-11-06_Yosemite.lrcat`, or `<catalog folder>\2026\_trips\2026-11-03_Japan\` for a trip. **Finish** (in Recent imports, once Lightroom has closed the catalog) copies the catalog, its Smart Previews, its AI mask data (`.lrcat-data`) and its list of shoot folders to `_catalog` (or `_trips`), reads every copied file back to check it, and then removes the local copy; Lightroom's standard previews are rebuilt when needed, and its backups stay on this computer. **Reopen** copies the catalog back.

### Deliver and Immich

Under **Deliver → Finished photos folder and Immich**, choose the finished photos folder (for example `\\NAS\Picture\MyPhoto`, the folder Immich's external library watches). Optionally enter the Immich URL and an **admin API key** with **`library.read` + `library.update`** permissions (Immich's library endpoints are admin-only), then choose the external library to rescan. A password field accepts a new key; empty keeps the saved key, which is never sent back to the page. Clearing the URL and saving disconnects Immich. The key is kept only in this computer's `settings.json`.

After a delivery that copied something, the chosen library is asked to rescan once, including for a trip. A rescan failure is reported without failing the delivery. Card Intake never changes the library's settings. Without Immich, Deliver still copies the JPEGs.

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

ExifTool is optional for importing copies, but needed for Deliver, shutter counts and shooting-settings ratings. Install ExifTool for Windows with its companion support folder, then put `exiftool.exe` on `PATH` or set `$env:CARD_INTAKE_EXIFTOOL` to its full path. The shell fetch script below is for macOS/Linux.

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
cargo run -p card_intake_desktop -- --deliver-plan /Volumes/photos/2026/2026-11-06_夜景 /Volumes/Picture/MyPhoto [catalog.lrcat] # what Deliver would add, as JSON; reads nothing but settings, which supply the folder and catalog if omitted
cargo run -p card_intake_core --example deliver_temp -- <temp-shoot> <temp-finished-photos> <exiftool> [catalog.lrcat] # a real delivery, restricted to system temp
cargo run -p card_intake_core --example card_storage -- E:\         # Windows: the card's bus, reader, details, serial and NVMe health log (through a JMicron bridge only when run as admin)
cargo run -p card_intake_core --example scan_card -- /Volumes/Untitled
cargo run -p card_intake_core --example count_photos -- /Volumes/photos /Volumes/old-Nikon  # counts photos outside the share
```

On Windows, use `E:\` instead of `/Volumes/Untitled` and `\\NAS\photos` instead of `/Volumes/photos`.

The import tests use temporary card/share fixtures and cover verified copies, sidecars, date/video selection, conflicts, corrupt read-backs, cancellation, parallel workers, retries, files already under another shoot title, changed sources, saved resume requests, history, and rejection of local destinations. Lightroom tests cover shoot and trip catalogs, the rule that only shoots without a catalog join a trip, Finish and Reopen, and installing the plug-in and presets; they also run the actual Lua plug-in in Fengari with a mocked Lightroom SDK (presets, skipped and refused files, DxO DNG stacking, disconnected folders, a busy catalog, and other catalogs). Shutter tests cover real α7R VI ranges, sparse and newest bad readings, stored-history checks, poisoned highs, visible resets, serial-less bodies, schema migration, synced removal and re-registration with stale computers, pending batches and JSON, and damaged or newer databases. UI tests run the real page with a mocked Tauri bridge, including removal confirmation, expiry, errors and preserved imports, and require Node.js:

```sh
npm ci --prefix tests/ui
npm test --prefix tests/ui
cargo clippy --workspace --all-targets -- -D warnings
```

The NAS path is saved in `settings.json` in the app's config folder. Environment variables override it and the tool locations: `CARD_INTAKE_NAS`, `CARD_INTAKE_EXIFTOOL`, `CARD_INTAKE_SMARTCTL`. Registered cards (My Cards), their health readings and the most recent 100 import results (plus every unsent offline import) are saved in `ledger.json`, and this computer's copy of the shutter count history in `shutter.sqlite`, in the app data folder (`%APPDATA%\io.github.semmmer.cardintake\` on Windows; `~/Library/Application Support/io.github.semmmer.cardintake/` on macOS). Offline folder, remembered backup drives and Not-mine choices are local settings. Offline import file lists live in `offline-manifests` and Send verification manifests in `sent-manifests`, beside the ledger, keeping file lists out of `ledger.json`. Older settings and ledgers load with defaults for these fields; old imports are NAS imports. Older ledgers load with an empty import history.

The Windows-specific code lives in `crates/core`. `cargo test --workspace` builds and tests the native desktop backend on Windows; tests do not eject physical cards or write to configured NAS shares.

Installers and app bundles: `cargo install tauri-cli --version "^2"`, then `cargo tauri build` inside `apps/desktop`.

### NAS service on Synology

In Container Manager, create a project from `docker/compose.yaml` and set the photo share path in it (`/volume1/photos` by default).

### Lightroom

1. In the Lightroom Classic version you use, create an **empty catalog**, set its catalog settings the way you want every shoot's (for example **Automatically write changes into XMP**), and close it. Card Intake refuses a template Lightroom still has open.
2. In Card Intake, under **Lightroom**, open **Catalog template, folders and Lightroom**, enter the template path, and save. Lightroom Classic is found in its usual install folder. Catalogs default to `Pictures\Lightroom\Card Intake`; any local folder works. To have your develop presets and profiles in every catalog, enter their folder under **Presets and profiles to install**: Card Intake copies every `.xmp` in it to Lightroom's own preset folder (`%APPDATA%\Adobe\CameraRaw\Settings\Card Intake`), and updates changed ones.
3. Card Intake installs its plug-in in Lightroom's Modules folder (`%APPDATA%\Adobe\Lightroom\Modules\CardIntake.lrplugin`), which Lightroom loads by itself: restart Lightroom once. Lightroom Classic 15.6 only starts a plug-in at launch when it offers an export service (with `LrForceInitPlugin` alone it waited until a plug-in feature was used, such as Plug-in Manager), so the plug-in offers one that exports nothing: **Card Intake (start-up helper)** in the Export dialog's **Export To** list is that, and choosing it only explains itself. The plug-in logs what it does to `%LOCALAPPDATA%\Adobe\Lightroom\Logs\LrClassicLogs\CardIntake.log`. In **File → Plug-in Manager → Card Intake**, choose the develop preset for raw photos, the one for DxO DNGs, and the metadata preset. A copy older builds asked you to add by hand is removed.
4. After an import, use **Open in Lightroom** beside a shoot, or name a trip and use **Open as a trip** for all of the import's shoots (`2026-11-03_Japan`, after the first day). Only shoots without a catalog can join a trip; once in one, opening any of its shoots opens the trip. During an import you can press **Open in Lightroom when done** (and pick the first shoot or a trip). For an import through this computer, it opens when the card is read and checked, while the NAS is still catching up; direct imports open when the import finishes. Recent imports shows each catalog: **On this PC** with **Open** and **Finish**, **Open in Lightroom**, or **Finished · on NAS** with **Reopen**.

When a catalog Card Intake made opens, the plug-in reads `<catalog>.card-intake.txt` next to it, which lists its shoot folders, and adds every raw photo and DxO PureRAW DNG in them that isn't in the catalog yet, in place, with the chosen presets; JPEG, HEIF and video stay out. It then builds Smart Previews for them. A DNG named after its raw (`DSC03518_DxO.dng`, `DSC03518_DxO_DeepPRIMEXD2s.dng`) is stacked on top of the raw and takes its rating, flag and colour label. The plug-in makes no collections or keywords. Lightroom restarts when it switches catalogs, so this runs for every catalog you open; while Card Intake is uploading, the plug-in re-reads the folder list and adds arriving photos every few seconds, skipping folders that do not exist yet. When **Open in Lightroom** finds the catalog already open, Card Intake asks the plug-in to add what's new (it opens `lightroom://io.github.semmmer.cardintake/add`). For DxO's DNGs, or any other time, use **Library → Plug-in Extras → Add new files from NAS**. While one of Lightroom's own prompts is open (such as **Enable address lookup?**, which a new catalog asks until its template has answered it), Lightroom holds the plug-in back; photos are added once the prompt is answered. A share that isn't connected, or a file Lightroom refuses, is reported in a Lightroom dialog.

Next to an open upload's catalog, `<name>.card-intake.uploading` contains a Unix-seconds heartbeat. Card Intake writes it before starting Lightroom, replaces it atomically about every five seconds, and removes it when uploading ends, stops or fails, and on normal exit. The plug-in only treats a heartbeat at most 45 seconds old as active. If Lightroom already has that catalog open, **Open in Lightroom** asks the plug-in to start adding this upload's arriving files. When the import ends, Card Intake updates the folder list to the NAS folders that actually hold the files, including earlier folders for the same date. The history remembers the catalog opened during upload, so Open, Finish, Reopen and Deliver keep using its edits even when the photos are found under another shoot title. **Finish** waits for the upload to end and never stores the heartbeat on the NAS.

`prototype/card-intake.html` is a demo that opens in any browser: the app's own page (`ui/index.html`) with a stand-in backend (`prototype/demo-backend.js`) and sample cards, cameras, imports and catalogs. Nothing is read or written, and reloading starts over. After changing either file, run `node prototype/build-demo.cjs`; a UI test fails while the demo is out of date.
