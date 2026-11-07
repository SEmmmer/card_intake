/* Card Intake demo backend.
 *
 * Stands in for the desktop app, so the app's own page (ui/index.html) runs in any
 * browser with sample cards, cameras, imports and Lightroom catalogs. Nothing is read
 * from or written to this computer, a card or a NAS: everything lives in this page and
 * starts over when it reloads. prototype/build-demo.cjs puts this script in front of
 * the page to make prototype/card-intake.html.
 */
(function () {
  "use strict";

  // Every wait is divided by this. Tests raise it so imports finish at once.
  var SPEED = window.__DEMO_SPEED__ || 1;

  function wait(ms, value) {
    return new Promise(function (resolve) { setTimeout(function () { resolve(value); }, ms / SPEED); });
  }
  function copy(value) { return value == null ? value : JSON.parse(JSON.stringify(value)); }
  function pad(n) { return String(n).padStart(2, "0"); }

  // Dates relative to today, in the camera-local form the app sends ("2026-11-07T14:38:29").
  function day(offset) {
    var d = new Date();
    d.setDate(d.getDate() + offset);
    return d.getFullYear() + "-" + pad(d.getMonth() + 1) + "-" + pad(d.getDate());
  }
  function at(offset, time) { return day(offset) + "T" + time; }
  function now() {
    var d = new Date();
    return day(0) + "T" + pad(d.getHours()) + ":" + pad(d.getMinutes()) + ":" + pad(d.getSeconds()) + "." + String(d.getMilliseconds()).padStart(3, "0");
  }
  function baseName(p) { var parts = String(p).split(/[\\/]/).filter(Boolean); return parts[parts.length - 1] || String(p); }
  function join(first) {
    var rest = [].slice.call(arguments, 1).map(function (p) { return String(p).replace(/^[\\/]+|[\\/]+$/g, ""); });
    return [String(first).replace(/[\\/]+$/, "")].concat(rest).join(first.indexOf("\\") >= 0 ? "\\" : "/");
  }
  // The same folder name rule as the page and the app.
  function cleanName(v) {
    return String(v || "").replace(/[\\\/:*?"<>|]/g, "").replace(/[\u0000-\u001f]/g, "").trim().split(/\s+/).filter(Boolean).join("-").replace(/^\.+|\.+$/g, "");
  }
  function folderName(date, name) { var n = cleanName(name); return n ? date + "_" + n : date; }

  /* ---------- NAS ---------- */
  var DEFAULT_NAS = "\\\\NAS\\photos";
  var nas = { root: DEFAULT_NAS, source: "settings", connected: true, network: true, free_bytes: 31.2e12 };
  function isNetwork(path) { return /^(\\\\|\/\/|\/Volumes\/)/.test(path); }

  function checkNas(path) {
    path = String(path || "").trim();
    var items = [];
    function add(status, title, detail) { items.push({ status: status, title: title, detail: detail }); }
    if (!path) {
      add("fail", "No folder entered", "Type the photo share's path, e.g. \\\\NAS\\photos.");
    } else if (!/^(\\\\|\/\/|[A-Za-z]:[\\/]|\/)/.test(path)) {
      add("fail", "Not a full path", "Use a full path such as \\\\NAS\\photos or Z:\\photos.");
    } else {
      add("pass", "Folder found", path);
      if (isNetwork(path)) add("pass", "On a network share", "Windows network share");
      else add("warn", "On this computer's own disk", "Imports go to the NAS, so a card's photos survive this computer failing.");
      add("pass", "Test file written, read back and deleted", "64 MB came back identical. Write speed about 1,012 MB/s.");
      add("pass", "Enough space", "31.2 TB free of 48.0 TB");
      add("pass", "Looks like your photo share", "Year folders 2024, 2025, 2026 · 412 shoot folders");
    }
    var ok = !items.some(function (i) { return i.status === "fail"; });
    return wait(1200, { path: path, ok: ok, items: items, free_bytes: ok ? 31.2e12 : null, total_bytes: ok ? 48e12 : null,
      write_mb_per_s: ok ? 1012 : null, years: ok ? ["2024", "2025", "2026"] : [], shoot_folders: ok ? 412 : 0 });
  }

  /* ---------- Cards in the reader ---------- */
  var CF = "E:\\", SD = "F:\\";
  var CF_KEY = "sn:BF7F0763030400117918", SD_KEY = "5E21-9C04";
  var roots = [CF, SD];

  function cfHealth(temperature) {
    return { model: "CEA-G240T", serial: "BF7F0763030400117918", firmware: "X2J0R100", passed: true, critical_warning: 0,
      percentage_used: 1, available_spare: 100, available_spare_threshold: 60, data_read_bytes: 3.32e12, data_written_bytes: 1.77e12,
      host_read_commands: 23163300, host_write_commands: 10426020, controller_busy_minutes: null, power_cycles: 214, power_on_hours: 12,
      unsafe_shutdowns: 0, media_errors: 0, error_log_entries: 0, temperature_c: temperature, warning_temp_minutes: null,
      critical_temp_minutes: null, nvme_version: "1.4", bridge: null, device_type: null };
  }
  var platinumHealth = { model: "CHIPFANCIER CFE Platinum", serial: "P640GA20223532F1100", firmware: "ELFM20.2", passed: true,
    critical_warning: 0, percentage_used: 4, available_spare: 100, available_spare_threshold: 10, data_read_bytes: 41.8e12,
    data_written_bytes: 18.2e12, host_read_commands: 302114880, host_write_commands: 141907112, controller_busy_minutes: 1312,
    power_cycles: 611, power_on_hours: 97, unsafe_shutdowns: 2, media_errors: 0, error_log_entries: 0, temperature_c: 41,
    warning_temp_minutes: 0, critical_temp_minutes: 0, nvme_version: "1.4", bridge: "JMicron JMS583", device_type: null };

  var ARW = 58.6e6, XAVC = 2.1e9, JPG = 9.8e6, MP4 = 1.1e9;
  var reports = {};
  reports[CF] = {
    root: CF,
    volume: { id: "738E-0C9A", name: null, filesystem: "exFAT", total_bytes: 239990210560, free_bytes: 198.05e9, reader: null, health: null },
    drive: { bus: "NVMe", product: "CEA-G240T", card_serial: "BF7F0763030400117918", usb_id: null, bridge: null },
    health: cfHealth(34),
    details: { removable: true, write_protected: false, capacity_bytes: 240057409536, sector_bytes: 512, partition_table: "MBR",
      partition_offset_bytes: 33554432, partition_type: "07",
      file_system: { name: "exFAT", revision: "1.00", cluster_bytes: 262144, dirty: false, media_failure: false } },
    card_key: CF_KEY,
    photos: 608, photo_bytes: 608 * ARW, videos: 3, video_bytes: 3 * XAVC,
    times: { exif: 608, sidecar: 3, file_time: 0 },
    groups: [
      { date: day(-1), first: at(-1, "17:21:17"), last: at(-1, "20:51:07"), photos: 196, photo_bytes: 196 * ARW, videos: 3, video_bytes: 3 * XAVC },
      { date: day(0), first: at(0, "14:38:29"), last: at(0, "16:05:12"), photos: 412, photo_bytes: 412 * ARW, videos: 0, video_bytes: 0 }
    ],
    // Yesterday evening's first 120 photos and its clips were imported before the shoot ended.
    destinations: [{ date: day(-1), suggested_title: "Harbour-night",
      folders: [{ name: day(-1) + "_Harbour-night", title: "Harbour-night", photos: 120, videos: 3, bytes: 120 * ARW + 3 * XAVC }],
      photos_on_nas: 120, videos_on_nas: 3, photo_bytes_on_nas: 120 * ARW, video_bytes_on_nas: 3 * XAVC }],
    camera: { make: "SONY", model: "ILCE-7RM6", serial: "03506694", internal_serial: "73 255 0 0 25 9", firmware: "1.01",
      lens: "FE 35mm F1.4 GM", shutter_count: 1620, electronic_count: null, shutter_note: null },
    scan_ms: 640
  };
  // A DJI Osmo Pocket 3's card in a two-slot USB reader: new to this app, and so is the camera.
  reports[SD] = {
    root: SD,
    volume: { id: SD_KEY, name: "Untitled", filesystem: "exFAT", total_bytes: 255869321216, free_bytes: 235.8e9, reader: "USB Storage", health: null },
    drive: { bus: "USB", product: "MassStorageClass", card_serial: null, usb_id: "05E3:0764", bridge: null },
    health: null,
    details: { reader: { name: "USB Storage", usb_id: "05E3:0764", usb_version: "3.20", protocol: "BOT", link: "USB 5 Gbps", link_capable: "USB 5 Gbps" },
      slot: 2, slots: 2, removable: true, write_protected: false, capacity_bytes: 255869321216, sector_bytes: 512, sd_class: "SDXC",
      partition_table: "MBR", partition_offset_bytes: 32768, partition_type: "07",
      file_system: { name: "exFAT", cluster_bytes: 131072, revision: "1.00", dirty: false, media_failure: false } },
    card_key: SD_KEY,
    photos: 24, photo_bytes: 24 * JPG, videos: 18, video_bytes: 18 * MP4,
    times: { exif: 42, sidecar: 0, file_time: 0 },
    groups: [{ date: day(0), first: at(0, "09:12:41"), last: at(0, "11:40:03"), photos: 24, photo_bytes: 24 * JPG, videos: 18, video_bytes: 18 * MP4 }],
    destinations: [],
    camera: { make: "DJI", model: "PP-101", serial: "5WCQNBL6BB1A22", internal_serial: null, firmware: "10.00.55.15", lens: null,
      shutter_count: null, electronic_count: null,
      shutter_note: "The Osmo Pocket 3 has no mechanical shutter: its count is the electronic shutter's, and ExifTool found none in this card's photos." },
    scan_ms: 210
  };

  // File names for the progress line: [folder, prefix, first number, digits, extension].
  var NAMES = {};
  NAMES[CF] = {
    photo: function (date, i) { return "DCIM\\100MSDCF\\EM_" + String((date === day(-1) ? 1013 : 1209) + i).padStart(5, "0") + ".ARW"; },
    video: function (date, i) { return "PRIVATE\\M4ROOT\\CLIP\\C" + String(41 + i).padStart(4, "0") + ".MP4"; }
  };
  NAMES[SD] = {
    photo: function (date, i) { return "DCIM\\DJI_001\\DJI_" + String(1 + i).padStart(4, "0") + ".JPG"; },
    video: function (date, i) { return "DCIM\\DJI_001\\DJI_" + String(25 + i).padStart(4, "0") + ".MP4"; }
  };
  var COPY_SPEED = {};
  COPY_SPEED[CF] = 702e6;   // CFexpress Type A on the NVMe bus, to the NAS over 10 GbE
  COPY_SPEED[SD] = 88e6;    // an SD card in a USB 5 Gbps reader

  function destination(report, date) {
    var d = report.destinations.filter(function (x) { return x.date === date; })[0];
    if (!d) {
      d = { date: date, suggested_title: null, folders: [], photos_on_nas: 0, videos_on_nas: 0, photo_bytes_on_nas: 0, video_bytes_on_nas: 0 };
      report.destinations.push(d);
    }
    return d;
  }

  /* ---------- My Cards and import history ---------- */
  function card(o) {
    return Object.assign({ name: null, model: null, bus: "USB", sd_class: null, formats: 0, filesystem: "exFAT", reader: null,
      insertions: 1, hardware_serial: null, health: [], registered: true }, o);
  }
  var ledger = {
    cards: [
      card({ id: CF_KEY, volume_id: "738E-0C9A", model: "CEA-G240T", bus: "NVMe", formats: 3, capacity_bytes: 239990210560,
        first_seen: at(-40, "10:12:00"), last_seen: now(), insertions: 18, hardware_serial: "BF7F0763030400117918",
        health: [{ at: at(-1, "19:01:44"), health: cfHealth(36) }] }),
      card({ id: "sn:P640GA20223532F1100", volume_id: "4C1A-77E0", model: "CHIPFANCIER CFE Platinum", formats: 7, capacity_bytes: 640e9,
        first_seen: at(-120, "08:30:00"), last_seen: at(-2, "22:40:00"), insertions: 31, hardware_serial: "P640GA20223532F1100",
        health: [{ at: at(-2, "22:40:00"), health: platinumHealth }] }),
      card({ id: "9A3E-1B20", volume_id: "9A3E-1B20", sd_class: "SDXC", capacity_bytes: 127865454592,
        first_seen: at(-60, "12:00:00"), last_seen: at(-5, "20:15:00"), insertions: 9 }),
      // The card in F:, seen for the first time: not saved until it's registered.
      card({ id: SD_KEY, volume_id: SD_KEY, sd_class: "SDXC", capacity_bytes: 255869321216, first_seen: now(), last_seen: now(), registered: false })
    ],
    cameras: [],
    imports: []
  };

  function stars(n) {
    var shares = [0.01, 0.02, 0.04, 0.09, 0.43], out = shares.map(function (s) { return Math.floor(n * s); });
    out.push(n - out.reduce(function (a, b) { return a + b; }, 0));
    return out;
  }
  function ratings(rated, locked, camera, kept) {
    return { rated: rated, locked: locked, camera: camera, computed: rated - locked - camera, kept: kept || 0, stars: stars(rated) };
  }
  function done(o) {
    var files = o.photos + o.videos;
    return Object.assign({ status: "complete", root: CF, nas_root: DEFAULT_NAS, total_files: files, copied_files: files, skipped_files: 0,
      verified_files: files, completed_bytes: o.total_bytes, skipped_elsewhere: 0, found_in: [], error: null, warnings: [], ejected: false }, o);
  }
  function rel(date, name) { return date.slice(0, 4) + "\\" + folderName(date, name); }

  ledger.imports = [
    done({ started_at: at(-1, "19:02:10.512"), finished_at: at(-1, "19:03:31.204"), card_id: "738E-0C9A", card_key: CF_KEY,
      camera: "ILCE-7RM6", photos: 120, videos: 3, total_bytes: 120 * ARW + 3 * XAVC, average_bytes_per_sec: 702e6,
      shoots: [rel(day(-1), "Harbour-night")], ratings: ratings(120, 1, 4) }),
    done({ started_at: at(-2, "22:31:05.118"), finished_at: at(-2, "22:32:12.930"), root: "G:\\", card_id: "4C1A-77E0",
      card_key: "sn:P640GA20223532F1100", camera: "NIKON Z 9", photos: 1204, videos: 0, total_bytes: 1204 * 45e6,
      average_bytes_per_sec: 812e6, shoots: [rel(day(-2), "Wedding-Rossi")], ratings: ratings(1204, 0, 0) }),
    done({ started_at: at(-5, "20:11:40.007"), finished_at: at(-5, "20:13:52.660"), root: "F:\\", card_id: "9A3E-1B20", card_key: "9A3E-1B20",
      camera: "NIKON Z f", photos: 238, videos: 4, total_bytes: 238 * 25e6 + 4 * 1.4e9, average_bytes_per_sec: 88e6,
      shoots: [rel(day(-5), "Street")], ratings: ratings(238, 0, 3) }),
    done({ started_at: at(-8, "21:45:19.441"), finished_at: at(-8, "21:46:03.882"), card_id: "738E-0C9A", card_key: CF_KEY,
      camera: "ILCE-7RM6", photos: 397, videos: 2, total_bytes: 397 * ARW + 2 * XAVC, average_bytes_per_sec: 698e6,
      shoots: [rel(day(-9), "Kyoto"), rel(day(-8), "Kyoto")], ratings: ratings(397, 1, 0) }),
    done({ status: "cancelled", started_at: at(-11, "23:02:00.150"), finished_at: at(-11, "23:02:31.090"), root: "G:\\", card_id: "4C1A-77E0",
      card_key: "sn:P640GA20223532F1100", camera: "NIKON Z 9", photos: 0, videos: 0, total_files: 860, total_bytes: 860 * 45e6,
      copied_files: 312, verified_files: 312, completed_bytes: 312 * 45e6, average_bytes_per_sec: 806e6,
      shoots: [rel(day(-11), "Portraits")], ratings: ratings(0, 0, 0),
      error: "Import stopped. Completed files are kept; retry to check and skip them." })
  ];
  var pending = null;

  /* ---------- My Cameras: shutter count history ---------- */
  function counts(offset, min, max, first, last, firmware, electronic) {
    return { date: day(offset), min: min, max: max, electronic_max: electronic || null, first_shot: at(offset, first), last_shot: at(offset, last), firmware: firmware };
  }
  function camera(o) {
    var v = Object.assign({ name: null, serial: null, internal_serial: null, firmware: null, rated_shutter: null, rating_source: null,
      has_mechanical_shutter: true, electronic_count: null, shutter_reference: null, days: [] }, o);
    var last = v.days[v.days.length - 1];
    v.count = last ? last.max : null;
    v.last_shot = last ? last.last_shot : (o.last_shot || null);
    var electronic = v.days.map(function (d) { return d.electronic_max; }).filter(function (n) { return n != null; });
    if (electronic.length) v.electronic_count = Math.max.apply(null, electronic);
    return v;
  }
  // Adds a card's days to a camera's history, as the app's merge does.
  function addDays(view, days) {
    var byDate = {};
    view.days.concat(days).forEach(function (d) {
      var o = byDate[d.date];
      byDate[d.date] = !o ? d : Object.assign({}, o, { min: Math.min(o.min, d.min), max: Math.max(o.max, d.max),
        first_shot: o.first_shot < d.first_shot ? o.first_shot : d.first_shot, last_shot: o.last_shot > d.last_shot ? o.last_shot : d.last_shot });
    });
    return camera(Object.assign({}, view, { days: Object.keys(byDate).sort().map(function (k) { return byDate[k]; }) }));
  }
  // Each card's camera with no history, and the days its photos add.
  var BODY = {}, CARD_DAYS = {};
  BODY[CF] = function () {
    return camera({ model: "ILCE-7RM6", name: "α7R VI", serial: "03506694", internal_serial: "73 255 0 0 25 9", firmware: "1.01", shutter_reference: 500000 });
  };
  BODY[SD] = function () {
    return camera({ model: "PP-101", name: "Osmo Pocket 3", serial: "5WCQNBL6BB1A22", firmware: "10.00.55.15", has_mechanical_shutter: false,
      last_shot: at(0, "11:40:03") });
  };
  CARD_DAYS[CF] = [counts(-1, 1013, 1208, "17:21:17", "20:51:07", "1.01"), counts(0, 1209, 1620, "14:38:29", "16:05:12", "1.01")];
  CARD_DAYS[SD] = [];

  var myCameras = [
    // Yesterday's first 120 photos were read when they were imported.
    addDays(BODY[CF](), [
      counts(-30, 2, 30, "17:54:04", "23:03:22", "1.01"),
      counts(-29, 31, 614, "11:44:16", "18:17:49", "1.01"),
      counts(-9, 615, 803, "09:02:40", "17:40:11", "1.01"),
      counts(-8, 804, 1012, "08:31:05", "16:55:30", "1.01"),
      counts(-1, 1013, 1132, "17:21:17", "18:58:40", "1.01")
    ]),
    camera({ model: "NIKON Z 9", name: "Z 9", serial: "8016296", firmware: "5.32", has_mechanical_shutter: false, days: [
      counts(-90, 160102, 160880, "10:05:12", "18:40:51", "5.30"),
      counts(-40, 161000, 163342, "09:15:00", "21:12:09", "5.32"),
      counts(-11, 163343, 164545, "13:00:41", "19:22:10", "5.32"),
      counts(-2, 164546, 165750, "11:02:33", "22:05:48", "5.32")
    ] }),
    camera({ model: "NIKON Z f", name: "Zf", serial: "8043077", firmware: "3.10", rated_shutter: 200000,
      rating_source: "Nikon: mechanical shutter tested to 200,000 cycles (Z f product page, Nikon USA)", days: [
        counts(-50, 9100, 9512, "07:40:00", "12:31:12", "3.10", 14200),
        counts(-20, 9513, 9807, "15:10:44", "19:02:30", "3.10", 15010),
        counts(-5, 9808, 10045, "16:20:05", "19:58:12", "3.10", 15557)
      ] })
  ];
  var newCameras = [];

  function sameCamera(c, model, serial, internal) {
    if (c.model !== model) return false;
    if (serial) return c.serial === serial;
    if (internal) return c.internal_serial === internal;
    return !c.serial && !c.internal_serial;
  }
  function camerasResponse() {
    return copy({ cameras: myCameras, new_cameras: newCameras,
      shared: { path: nas.network ? join(nas.root, ".card-intake", "shutter.sqlite") : null, synced: !!nas.network, error: null, folders: 196 } });
  }

  // A card read again after it was removed from My Cards is new again.
  function cardRecord(root) {
    var r = reports[root], rec = ledger.cards.filter(function (c) { return c.id === r.card_key; })[0];
    if (!rec) {
      rec = card({ id: r.card_key, volume_id: r.volume.id, model: r.drive.card_serial ? r.drive.product : null, bus: r.drive.bus,
        sd_class: r.details.sd_class || null, capacity_bytes: r.volume.total_bytes, first_seen: now(), hardware_serial: r.drive.card_serial,
        health: r.health ? [{ at: now(), health: r.health }] : [], registered: false });
      ledger.cards.unshift(rec);
    }
    rec.last_seen = now();
    return rec;
  }

  /* ---------- Import ---------- */
  var running = null;

  function startImport(request, channel) {
    var root = request.root, report = reports[root];
    if (!report || roots.indexOf(root) < 0) return Promise.reject("The card isn't in the reader any more.");
    if (running) return Promise.reject("An import is already running.");
    running = { cancel: false };
    return runImport(running, request, channel, report).finally(function () { running = null; });
  }

  async function runImport(job, request, channel, report) {
    var root = request.root, started = now(), speed = COPY_SPEED[root];
    var files = [], shoots = [], foundIn = [], elsewhere = 0;
    request.shoots.forEach(function (s) {
      var g = report.groups.filter(function (x) { return x.date === s.date; })[0];
      if (!g) return;
      var folder = folderName(s.date, s.name), d = destination(report, s.date);
      var same = d.folders.filter(function (f) { return f.name.toLowerCase() === folder.toLowerCase(); })[0];
      var shoot = { date: s.date, name: s.name, folder: folder, relative: rel(s.date, s.name), same: same, dest: d };
      var kinds = [["photo", g.photos, g.photo_bytes, d.photos_on_nas]];
      if (request.include_videos) kinds.push(["video", g.videos, g.video_bytes, d.videos_on_nas]);
      kinds.forEach(function (k) {
        var there = Math.min(k[1], k[3]);
        for (var i = 0; i < k[1]; i++) {
          files.push({ shoot: shoot, kind: k[0], size: k[2] / k[1], there: i < there, name: NAMES[root][k[0]](s.date, i) });
        }
        if (!same) elsewhere += there;
      });
      if (!same && (d.photos_on_nas || d.videos_on_nas)) d.folders.forEach(function (f) { foundIn.push(s.date.slice(0, 4) + "\\" + f.name); });
      shoots.push(shoot);
    });

    var total = files.reduce(function (sum, f) { return sum + f.size; }, 0);
    var p = { phase: "preparing", current_file: null, completed_files: 0, skipped_files: 0, total_files: files.length,
      processed_bytes: 0, total_bytes: Math.round(total), bytes_per_sec: null, eta_secs: null };
    function emit(phase, file, rate) {
      p.phase = phase;
      p.current_file = file ? join(root, file.name) : null;
      p.bytes_per_sec = rate || null;
      p.eta_secs = rate ? (total - p.processed_bytes) / (rate * (request.verify ? 0.8 : 1)) : null;
      p.processed_bytes = Math.round(p.processed_bytes);
      channel.onmessage(copy(p));
    }
    function jitter(base) { return base * (0.97 + Math.random() * 0.06); }

    emit("preparing");
    await wait(700);

    // Files already on the NAS are read back, compared with the card and skipped.
    var existing = files.filter(function (f) { return f.there; });
    var step = Math.max(1, Math.ceil(existing.length / 10));
    for (var i = 0; i < existing.length && !job.cancel; i += step) {
      existing.slice(i, i + step).forEach(function (f) { f.done = true; p.processed_bytes += f.size; p.completed_files++; p.skipped_files++; });
      emit("checking_existing", existing[Math.min(existing.length, i + step) - 1], jitter(1050e6));
      await wait(90);
    }

    // New files: each batch is copied, then checked against the card during the next one.
    var fresh = files.filter(function (f) { return !f.there; }), checking = [];
    step = Math.max(1, Math.ceil(fresh.length / 80));
    var copiedBytes = 0, copyTime = 0;
    for (i = 0; (i < fresh.length || checking.length) && !(job.cancel && !checking.length); i += step) {
      var batch = job.cancel ? [] : fresh.slice(i, i + step);
      checking.forEach(function (f) { f.done = true; p.processed_bytes += f.size / 2; p.completed_files++; });
      batch.forEach(function (f) {
        copiedBytes += f.size;
        if (request.verify) p.processed_bytes += f.size / 2;
        else { f.done = true; p.processed_bytes += f.size; p.completed_files++; }
      });
      checking = request.verify ? batch : [];
      var tick = 120;
      copyTime += batch.reduce(function (sum, f) { return sum + f.size; }, 0) / speed;
      emit(batch.length ? "copying" : "verifying", batch.length ? batch[batch.length - 1] : null, jitter(batch.length ? speed : 1050e6));
      await wait(tick);
    }

    var cancelled = job.cancel;
    var finished = files.filter(function (f) { return f.done; });
    var raws = finished.filter(function (f) { return /\.ARW$/i.test(f.name); });
    var rated = raws.filter(function (f) { return !f.there; }).length, kept = raws.length - rated;
    if (!cancelled && raws.length) {
      for (i = 0; i < 5; i++) { emit("rating", raws[Math.floor(i * raws.length / 5)]); await wait(110); }
    }

    // What's on the NAS now, for the next read of this card.
    shoots.forEach(function (s) {
      var mine = finished.filter(function (f) { return f.shoot === s && !f.there; });
      var photos = mine.filter(function (f) { return f.kind === "photo"; }), videos = mine.filter(function (f) { return f.kind === "video"; });
      if (!mine.length && !s.same) return;
      var folder = s.same || { name: s.folder, title: cleanName(s.name), photos: 0, videos: 0, bytes: 0 };
      if (!s.same) s.dest.folders.push(folder);
      folder.photos += photos.length; folder.videos += videos.length;
      folder.bytes += mine.reduce(function (sum, f) { return sum + f.size; }, 0);
      s.dest.photos_on_nas += photos.length; s.dest.videos_on_nas += videos.length;
      s.dest.photo_bytes_on_nas += photos.reduce(function (sum, f) { return sum + f.size; }, 0);
      s.dest.video_bytes_on_nas += videos.reduce(function (sum, f) { return sum + f.size; }, 0);
      if (s.dest.suggested_title == null) s.dest.suggested_title = cleanName(s.name);
    });

    var copied = finished.filter(function (f) { return !f.there; }).length;
    var result = {
      started_at: started, finished_at: now(), root: root, nas_root: nas.root, card_id: report.volume.id, card_key: report.card_key,
      status: cancelled ? "cancelled" : "complete",
      total_files: files.length, total_bytes: Math.round(total),
      copied_files: copied, skipped_files: finished.length - copied,
      verified_files: request.verify ? finished.length : finished.length - copied,
      completed_bytes: Math.round(finished.reduce(function (sum, f) { return sum + f.size; }, 0)),
      photos: cancelled ? 0 : files.filter(function (f) { return f.kind === "photo"; }).length,
      videos: cancelled ? 0 : files.filter(function (f) { return f.kind === "video"; }).length,
      skipped_elsewhere: cancelled ? 0 : elsewhere, found_in: cancelled ? [] : foundIn,
      camera: request.camera, average_bytes_per_sec: copyTime ? Math.round(copiedBytes / copyTime) : null,
      shoots: shoots.map(function (s) { return s.relative; }),
      error: cancelled ? "Import stopped. Completed files are kept; retry to check and skip them." : null,
      warnings: [],
      ratings: cancelled ? ratings(0, 0, 0) : ratings(rated, rated ? 1 : 0, Math.min(Math.max(0, rated - 1), Math.round(rated * 0.01)), kept),
      ejected: !cancelled && !!request.eject
    };
    ledger.imports.unshift(result);
    ledger.imports = ledger.imports.slice(0, 100);
    cardRecord(root);
    pending = cancelled ? { request: Object.assign({}, request, { nas_root: nas.root }), eject: !!request.eject } : null;
    if (result.ejected) roots = roots.filter(function (r) { return r !== root; });
    return copy(result);
  }

  /* ---------- Lightroom ---------- */
  var lightroom = {
    executable: "C:\\Program Files\\Adobe\\Adobe Lightroom Classic\\Lightroom.exe",
    template: "D:\\Lightroom\\Empty\\Empty.lrcat",
    catalog_root: "D:\\Lightroom\\Card Intake",
    presets: null
  };
  var PLUGIN = "C:\\Users\\you\\AppData\\Roaming\\Adobe\\Lightroom\\Modules\\CardIntake.lrplugin";
  function lightroomSetup() {
    return { settings: copy(lightroom), plugin: PLUGIN, presets_installed: lightroom.presets ? 98 : 0, presets_error: null };
  }

  // Catalogs by shoot folder; a trip holds several. A shoot imported again reuses its catalog.
  var catalogs = [
    { name: day(-1) + "_Harbour-night", trip: false, members: [rel(day(-1), "Harbour-night")], state: "local" },
    { name: day(-2) + "_Wedding-Rossi", trip: false, members: [rel(day(-2), "Wedding-Rossi")], state: "nas" },
    { name: day(-9) + "_Kyoto", trip: true, members: [rel(day(-9), "Kyoto"), rel(day(-8), "Kyoto")], state: "nas" }
  ];
  function catalogOf(shoot) { return catalogs.filter(function (c) { return c.members.indexOf(shoot) >= 0; })[0] || null; }
  function importById(id) {
    return ledger.imports.filter(function (r) { return r.started_at === id && r.status === "complete"; })[0] || null;
  }
  function catalogStates() {
    var list = [];
    ledger.imports.forEach(function (r) {
      if (r.status !== "complete") return;
      var seen = [];
      r.shoots.forEach(function (s, i) {
        var c = catalogOf(s);
        var entry = c && seen.filter(function (e) { return e.name === c.name; })[0];
        if (entry) { entry.shoots.push(i); return; }
        entry = { import_id: r.started_at, shoots: [i], name: c ? c.name : baseName(s), trip: c ? c.trip : false, state: c ? c.state : "none" };
        seen.push(entry);
        list.push(entry);
      });
    });
    return list;
  }
  function catalogPath(c) {
    var year = c.name.slice(0, 4);
    return c.trip ? join(lightroom.catalog_root, year, "_trips", c.name, c.name + ".lrcat") : join(lightroom.catalog_root, year, c.name, c.name + ".lrcat");
  }

  var deliveryStates = {};
  var folded = [];
  var deliverySetup = { deliver_to: "\\\\NAS\\Picture\\MyPhoto", url: "", has_key: false, library_id: "", libraries: [] };
  var demoLibrary = { id: "demo-library", name: "MyPhoto", assetCount: 7214, importPaths: ["/photos/MyPhoto"] };
  var demoPhotos = ["01226", "01228", "01249", "01257", "01263", "01360", "01388"];
  // A small coloured tile stands in for each photo's embedded thumbnail.
  function demoThumb(i) {
    var hue = [210, 28, 250, 190, 330, 45, 160][i % 7];
    var svg = '<svg xmlns="http://www.w3.org/2000/svg" width="120" height="160"><rect width="120" height="160" fill="hsl(' + hue + ',45%,42%)"/>' +
      '<circle cx="60" cy="62" r="24" fill="hsl(' + hue + ',50%,78%)"/><rect x="26" y="96" width="68" height="44" rx="20" fill="hsl(' + hue + ',50%,78%)"/></svg>';
    return "data:image/svg+xml," + encodeURIComponent(svg);
  }
  function deliveryFolders(id, index) {
    var record = importById(id), catalog = record && catalogOf(record.shoots[index]);
    if (!record) return null;
    return { record: record, folders: catalog && catalog.trip ? catalog.members : [record.shoots[index]] };
  }
  function demoPlan(record, folder) {
    var done = !!deliveryStates[folder];
    return {
      shoot: join(record.nas_root, folder), target: join(deliverySetup.deliver_to, baseName(folder)), target_exists: done, plan_only: true,
      "new": done ? 0 : demoPhotos.length, existing: done ? demoPhotos.length : 0, delivered: 0,
      photos: demoPhotos.map(function (n, i) {
        return { name: "EM_" + n + ".jpg", source: "final\\EM_" + n + "_(2).jpg", kind: "pixcake", stars: 5 - (i % 3), stars_from: "catalog",
          action: done ? "exists" : "new", thumbnail: done ? undefined : demoThumb(i) };
      }),
      skipped: ["01226", "01249", "01257", "01263", "01360", "01388"].map(function (n) { return { source: "topix\\EM_" + n + ".jpg", reason: "replaced by PixCake" }; }),
      warnings: [], errors: []
    };
  }
  function planDemo(id, index) {
    var job = deliveryFolders(id, index);
    if (!job) return Promise.reject("That import isn't in the history any more.");
    if (!deliverySetup.deliver_to) return Promise.reject("Set the finished photos folder (e.g. \\\\NAS\\Picture\\MyPhoto) under Deliver first.");
    return wait(600).then(function () {
      return { shoots: job.folders.map(function (f) { return demoPlan(job.record, f); }), errors: [] };
    });
  }
  function deliverDemo(id, index, confirmed) {
    var job = deliveryFolders(id, index);
    if (!job || running) return Promise.reject("Wait for the import to finish before delivering.");
    return wait(1500).then(function () {
      var shoots = job.folders.filter(function (folder) {
        return confirmed.some(function (c) { return c.shoot === join(job.record.nas_root, folder); });
      }).map(function (folder) {
        var r = demoPlan(job.record, folder);
        r.plan_only = false;
        r.photos.forEach(function (p) { if (p.action === "new") { p.action = "delivered"; r.delivered++; } delete p.thumbnail; });
        deliveryStates[folder] = { count: demoPhotos.length, last_delivered: new Date().toISOString() };
        return r;
      });
      return { shoots: shoots, rescanning: !!deliverySetup.library_id, warnings: [], errors: [] };
    });
  }
  function demoDeliveryStates() {
    return catalogStates().map(function (c) {
      var record = importById(c.import_id), catalog = catalogOf(record.shoots[c.shoots[0]]);
      var folders = catalog && catalog.trip ? catalog.members : [record.shoots[c.shoots[0]]];
      var count = 0, last = null;
      folders.forEach(function (f) { var s = deliveryStates[f]; if (s) { count += s.count; if (!last || s.last_delivered > last) last = s.last_delivered; } });
      return { import_id: c.import_id, shoot: c.shoots[0], count: count, last_delivered: last };
    });
  }

  function openInLightroom(importId, shoot, trip) {
    var record = importById(importId);
    if (!record) return Promise.reject("That import isn't in the history any more.");
    if (!lightroom.template) return Promise.reject("Choose an empty catalog as the template under Lightroom first.");
    var c;
    if (trip != null) {
      var free = record.shoots.filter(function (s) { return !catalogOf(s); });
      if (!free.length) return Promise.reject("Every shoot of this import already has a catalog. Open them one by one.");
      c = { name: folderName(free.slice().sort()[0].split("\\").pop().slice(0, 10), trip), trip: true, members: free, state: "none" };
      catalogs.push(c);
    } else {
      var s = record.shoots[shoot];
      c = catalogOf(s);
      if (!c) { c = { name: baseName(s), trip: false, members: [s], state: "none" }; catalogs.push(c); }
    }
    var already = c.state === "open";
    // Lightroom has one catalog open at a time: the one before is closed, and stays on this PC.
    catalogs.forEach(function (x) { if (x !== c && x.state === "open") x.state = "local"; });
    c.state = "open";
    return wait(800, { catalog: catalogPath(c), name: c.name, already_open: already });
  }

  function finishCatalog(importId, shoot) {
    var record = importById(importId), c = record && catalogOf(record.shoots[shoot]);
    if (!c) return Promise.reject("This shoot has no catalog yet.");
    if (c.state === "open") return Promise.reject("Close " + c.name + " in Lightroom first: open another catalog, or quit Lightroom.");
    if (c.state !== "local") return Promise.reject(c.name + " isn't on this computer.");
    c.state = "nas";
    var year = c.name.slice(0, 4);
    var stored = c.trip ? join(nas.root, year, "_trips", c.name) : join(nas.root, year, c.name, "_catalog");
    return wait(1500, { name: c.name, stored: stored });
  }

  /* ---------- Reading every photo on the NAS ---------- */
  var scan = null;
  async function scanLibrary(channel) {
    var job = scan = { cancel: false };
    channel.onmessage({ phase: "listing", progress: null });
    await wait(900);
    var folders = ["Harbour-night", "Wedding-Rossi", "Street", "Kyoto", "Portraits", "Market", "Studio-Ana", "Coast"];
    var total = 1840, done = 0, read = 0;
    while (done < total && !job.cancel) {
      done = Math.min(total, done + 60);
      read = Math.floor(done / total * 23);
      channel.onmessage({ phase: "reading", progress: { folders_done: read, folders_total: 23, photos_done: done, photos_total: total,
        current: rel(day(-3 * (read % 8) - 1), folders[read % folders.length]) } });
      await wait(150);
    }
    scan = null;
    return { folders_read: read, folders_unchanged: 173, photos_read: done, shutter_counts: done, failed: [], failed_count: 0,
      cancelled: job.cancel, cameras: camerasResponse() };
  }

  /* ---------- The app's commands ---------- */
  function invoke(command, args) {
    args = args || {};
    switch (command) {
      case "list_cards":
        return Promise.resolve(roots.map(function (r) { return { root: r, has_sony_video: r === CF }; }));
      case "eject_card":
        if (running) return Promise.reject("Wait for the import to finish, or stop it, before ejecting the card.");
        return wait(700).then(function () { roots = roots.filter(function (r) { return r !== args.root; }); });
      case "read_card":
        if (roots.indexOf(args.root) < 0) return Promise.reject("The card isn't in the reader any more.");
        cardRecord(args.root);
        return wait(900, copy(reports[args.root]));
      case "read_card_health":
        if (args.root !== CF) return Promise.resolve({ kind: "unavailable", reason: "SD cards keep no health log." });
        return wait(1500).then(function () {
          var reading = { at: now(), health: cfHealth(35) };
          var rec = cardRecord(CF);
          rec.health.push(reading);
          return { kind: "read", health: reading.health, card_key: CF_KEY };
        });
      case "read_card_shutter":
        return wait(1600).then(function () {
          // My Cameras keep the card's counts; a camera that isn't one of them is new, with only these.
          var cam = reports[args.root].camera, days = CARD_DAYS[args.root];
          var same = function (c) { return sameCamera(c, cam.model, cam.serial, cam.internal_serial); };
          var mine = myCameras.findIndex(same), fresh = newCameras.findIndex(same);
          if (mine >= 0) myCameras[mine] = addDays(myCameras[mine], days);
          else if (fresh >= 0) newCameras[fresh] = addDays(newCameras[fresh], days);
          else newCameras.push(addDays(BODY[args.root](), days));
          return camerasResponse();
        });
      case "get_cameras":
        return Promise.resolve(camerasResponse());
      case "register_camera":
        return wait(400).then(function () {
          var same = function (c) { return sameCamera(c, args.model, args.serial, args.internalSerial); };
          var c = newCameras.filter(same)[0];
          newCameras = newCameras.filter(function (x) { return x !== c; });
          if (!c) {
            var root = [CF, SD].filter(function (r) { return same(reports[r].camera); })[0];
            c = root ? BODY[root]() : camera({ model: args.model, serial: args.serial, internal_serial: args.internalSerial });
          }
          myCameras.push(c);
          return camerasResponse();
        });
      case "remove_camera":
        return wait(400).then(function () {
          myCameras = myCameras.filter(function (x) { return !sameCamera(x, args.model, args.serial, args.internalSerial); });
          return camerasResponse();
        });
      case "register_card":
        ledger.cards.forEach(function (c) { if (c.id === args.cardKey) c.registered = true; });
        return wait(300, copy(ledger));
      case "remove_card":
        ledger.cards = ledger.cards.filter(function (c) { return c.id !== args.cardKey; });
        return wait(300, copy(ledger));
      case "get_ledger":
        return Promise.resolve(copy(ledger));
      case "get_settings":
        return Promise.resolve({ nas_root: nas.root, lightroom: copy(lightroom), deliver_to: deliverySetup.deliver_to, folded: folded.slice() });
      case "nas_status":
        return Promise.resolve(copy(nas));
      case "check_nas":
        return checkNas(args.path);
      case "save_nas_root":
        return wait(300).then(function () {
          var path = String(args.nasRoot || "").trim();
          nas = { root: path || DEFAULT_NAS, source: path ? "settings" : "default", connected: true,
            network: isNetwork(path || DEFAULT_NAS), free_bytes: 31.2e12 };
          return copy(nas);
        });
      case "scan_library":
        return scanLibrary(args.onProgress);
      case "cancel_library_scan":
        if (scan) scan.cancel = true;
        return Promise.resolve();
      case "start_import":
        return startImport(args.request, args.onProgress);
      case "cancel_import":
        if (running) running.cancel = true;
        return Promise.resolve();
      case "get_pending_import":
        return Promise.resolve(copy(pending));
      case "get_lightroom_settings":
        return Promise.resolve(lightroomSetup());
      case "save_lightroom_settings":
        return wait(400).then(function () { lightroom = Object.assign({}, args.settings); return lightroomSetup(); });
      case "open_in_lightroom":
        return openInLightroom(args.importId, args.shoot, args.trip);
      case "finish_catalog":
        return finishCatalog(args.importId, args.shoot);
      case "lightroom_catalogs":
        return Promise.resolve(copy(catalogStates()));
      case "plan_delivery": return planDemo(args.importId, args.shoot);
      case "deliver": return deliverDemo(args.importId, args.shoot, args.confirmed);
      case "delivery_states": return Promise.resolve(copy(demoDeliveryStates()));
      case "get_delivery_settings": return Promise.resolve(copy(deliverySetup));
      case "save_delivery_settings":
        var on = !!args.settings.url;
        deliverySetup = { deliver_to: args.settings.deliver_to, url: args.settings.url, has_key: on, library_id: on ? args.settings.library_id || "demo-library" : "", libraries: on ? [copy(demoLibrary)] : [] };
        return wait(300, copy(deliverySetup));
      case "save_folded":
        folded = args.folded.slice();
        return Promise.resolve(null);
      default:
        return Promise.reject("The demo doesn't have the command " + command + ".");
    }
  }

  function Channel() { this.onmessage = function () {}; }
  window.__TAURI__ = { core: { invoke: invoke, Channel: Channel } };

  // Say it's a demo, beside the app's name.
  document.addEventListener("DOMContentLoaded", function () {
    var brand = document.querySelector(".brand");
    if (!brand) return;
    brand.style.display = "flex";
    brand.style.alignItems = "center";
    brand.style.gap = "10px";
    brand.insertAdjacentHTML("beforeend", '<span class="chip live" id="demo-chip" title="Imports take about ten seconds, whatever their size. ' +
      'Reload the page to start over.">Demo · sample data, nothing is read or written</span>');
  });
})();
