const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { JSDOM } = require('jsdom');

const html = fs.readFileSync(path.join(__dirname, '../../ui/index.html'), 'utf8');
const plain = value => JSON.parse(JSON.stringify(value));
const settle = async () => { for (let i = 0; i < 5; i++) await new Promise(resolve => setImmediate(resolve)); };

// The real page with a mocked Tauri bridge. `options` changes the card report, the
// ledger, cameras, the saved unfinished import and Lightroom catalogs.
async function app(t, network = true, options = {}) {
  const dom = new JSDOM(html, { runScripts: 'outside-only', url: 'http://tauri.localhost/' });
  t.after(() => dom.window.close());
  const { window } = dom;
  const calls = [], timers = new Map();
  const fixture = {
    roots: [{ root: 'E:\\', has_sony_video: true }],
    ledger: { cards: [], cameras: [], imports: [] },
    request: null, channel: null, finish: null,
    pending: options.pending || null,
    lrStatus: null,
    catalogs: options.catalogs || [],
    cameras: options.cameras || { cameras: [], shared: { path: null, synced: false, error: null, folders: 0 } },
    library: null,
    nas: { root: '\\\\NAS\\photos', connected: true, network, free_bytes: 1e12, source: 'settings' }
  };
  window.setInterval = (fn, ms) => { timers.set(ms, fn); return ms; };
  const report = {
    root: 'E:\\', volume: { id: 'CARD-ID', name: 'Camera card', filesystem: 'exFAT', total_bytes: 240e9, free_bytes: 200e9 },
    drive: options.drive || { bus: 'USB', product: 'MRW-G3', card_serial: null },
    health: options.health || null,
    card_key: options.cardKey || 'CARD-ID',
    photos: 2, photo_bytes: 200, videos: 1, video_bytes: 100,
    camera: { model: 'ILCE-7RM6', serial: '03506694', shutter_count: 894 },
    scan_ms: 12, times: { exif: 2, sidecar: 1, file_time: 0 },
    groups: [{ date: '2026-11-06', first: '2026-11-06T17:00:00', last: '2026-11-06T21:00:00', photos: 2, photo_bytes: 200, videos: 1, video_bytes: 100 }],
    destinations: options.destinations || []
  };
  window.__TAURI__ = { core: {
    Channel: class { onmessage() {} },
    invoke: (command, args) => {
      calls.push({ command, args });
      switch (command) {
        case 'list_cards': return Promise.resolve(fixture.roots);
        case 'read_card': return Promise.resolve(report);
        case 'get_ledger': return Promise.resolve(fixture.ledger);
        case 'nas_status': return Promise.resolve(fixture.nas);
        case 'cancel_import': return Promise.resolve();
        case 'get_pending_import': return Promise.resolve(fixture.pending);
        case 'get_lightroom_settings': return Promise.resolve({ settings: { executable: 'C:\\Adobe\\Lightroom.exe', template: 'C:\\Empty.lrcat', catalog_root: 'D:\\LrC Catalogs' }, plugin_path: 'C:\\AppData\\CardIntake.lrplugin' });
        case 'save_lightroom_settings': return Promise.resolve({ settings: args.settings, plugin_path: 'C:\\AppData\\CardIntake.lrplugin' });
        case 'lightroom_catalogs': return Promise.resolve(fixture.catalogs);
        case 'open_in_lightroom': return Promise.resolve({ catalog: 'D:\\LrC Catalogs\\2026\\2026-11-06_Tokyo\\2026-11-06_Tokyo.lrcat', request_id: 'lr-request' });
        case 'lightroom_status': return Promise.resolve(fixture.lrStatus);
        case 'get_cameras': return Promise.resolve(fixture.cameras);
        case 'read_card_shutter': return Promise.resolve(fixture.cameras);
        case 'cancel_library_scan': return Promise.resolve();
        case 'scan_library':
          fixture.libraryChannel = args.onProgress;
          return new Promise(resolve => { fixture.library = resolve; });
        case 'start_import':
          fixture.request = args.request;
          fixture.requests = (fixture.requests || []).concat([args.request]);
          fixture.pending = { request: { ...args.request, nas_root: fixture.nas.root }, eject: args.request.eject };
          fixture.channel = args.onProgress;
          return new Promise(resolve => { fixture.finish = result => { fixture.ledger.imports.unshift(result); if (result.status === 'complete') fixture.pending = null; resolve(result); }; });
        default: throw new Error('Unexpected command: ' + command);
      }
    }
  } };
  if (options.ledger) Object.assign(fixture.ledger, options.ledger);
  window.eval(window.document.querySelector('script').textContent);
  await settle();
  const $ = selector => window.document.querySelector(selector);
  const type = (selector, value) => { $(selector).value = value; $(selector).dispatchEvent(new window.Event('input')); };
  return { window, fixture, calls, timers, $, type };
}

function result(overrides = {}) {
  return {
    status: 'complete', root: 'E:\\', nas_root: '\\\\NAS\\photos', card_id: 'CARD-ID', card_key: 'CARD-ID',
    started_at: '2026-11-06T22:00:00', finished_at: '2026-11-06T22:01:00',
    total_files: 4, total_bytes: 310, copied_files: 4, skipped_files: 0, verified_files: 4,
    completed_bytes: 310, photos: 2, videos: 1, skipped_elsewhere: 0, found_in: [], camera: 'ILCE-7RM6',
    average_bytes_per_sec: 705e6, shoots: ['2026\\2026-11-06_Tokyo'], error: null, warnings: [], ejected: false,
    ...overrides
  };
}

// The NAS already has 2026-11-06_夜景 holding one of the card's photos and its clip.
const existing = [{
  date: '2026-11-06', suggested_title: '夜景',
  folders: [{ name: '2026-11-06_夜景', title: '夜景', photos: 1, videos: 1, bytes: 200 }],
  photos_on_nas: 1, videos_on_nas: 1, photo_bytes_on_nas: 100, video_bytes_on_nas: 100
}];

test('imports like the prototype: progress in the desk with the form locked, then the done panel and history row', async t => {
  const a = await app(t);
  assert.equal(a.$('#start').textContent, 'Import to NAS');
  a.type('#name-0', 'Tokyo');
  assert.match(a.$('#path-0').textContent, /2026-11-06_Tokyo/);
  a.$('#start').click();
  assert.deepEqual(plain(a.fixture.request), {
    root: 'E:\\', expected_card_id: 'CARD-ID', shoots: [{ date: '2026-11-06', name: 'Tokyo' }],
    include_videos: true, verify: true, eject: false, camera: 'ILCE-7RM6'
  });
  // The card stays on screen; the form is locked and the action bar gives way to progress.
  assert.match(a.$('#desk').textContent, /Shoots on this card/);
  assert.equal(a.$('#name-0').disabled, true);
  assert.equal(a.$('#opt-verify').disabled, true);
  assert.equal(a.$('#actions').hidden, true);
  assert.equal(a.$('#progress').hidden, false);
  assert.equal(a.$('#nas-save').disabled, true);
  a.fixture.channel.onmessage({ phase: 'copying', current_file: '2026\\2026-11-06_Tokyo\\DSC03518.ARW', completed_files: 12, total_files: 1290, skipped_files: 0, processed_bytes: 5e9, total_bytes: 168e9, bytes_per_sec: 680e6, eta_secs: 247 });
  assert.equal(a.$('#phase').textContent, 'Copying to NAS');
  assert.equal(a.$('#pct').textContent, '2%');
  assert.equal(a.$('#pline').textContent, 'DSC03518.ARW · 12 of 1,290 files · 680 MB/s · about 4 min 07 s left');
  a.fixture.channel.onmessage({ phase: 'file_complete', current_file: '2026\\2026-11-06_Tokyo\\DSC03518.ARW', completed_files: 13, total_files: 1290, skipped_files: 0, processed_bytes: 6e9, total_bytes: 168e9, bytes_per_sec: null, eta_secs: null });
  assert.equal(a.$('#phase').textContent, 'Copying to NAS');
  // Card polling pauses during the import.
  const polls = a.calls.filter(c => c.command === 'list_cards').length;
  a.timers.get(2000)();
  assert.equal(a.calls.filter(c => c.command === 'list_cards').length, polls);

  a.fixture.finish(result());
  await settle();
  const done = a.$('#done');
  assert.equal(done.hidden, false);
  assert.match(done.textContent, /Import complete/);
  assert.match(done.textContent, /All 2 photos and 1 clip are on the NAS and match the card\. You can format this card in the camera\./);
  assert.equal(a.$('#done-path-0').textContent, '\\\\NAS\\photos\\2026\\2026-11-06_Tokyo');
  assert.match(done.textContent, /Catalog: D:\\LrC Catalogs\\2026\\2026-11-06_Tokyo\\2026-11-06_Tokyo\.lrcat/);
  assert.ok(done.querySelector('[data-open="0"]'));
  assert.equal(a.$('#open-all'), null);
  assert.equal(a.$('#actions').hidden, true);
  assert.equal(a.$('#nas-save').disabled, false);
  const row = a.$('#history tbody tr');
  assert.equal(row.className, 'fresh');
  const cells = [...row.querySelectorAll('td')].map(td => td.textContent);
  // The mock ledger has no card records, so the Card column has nothing to name.
  assert.deepEqual(cells.slice(1, 8), ['—', 'ILCE-7RM6', '2 + 1 clips', '0.0 GB', '705 MB/s', 'Verified', '2026-11-06_Tokyo']);
  // The result stays when the card is removed.
  a.fixture.roots = [];
  a.timers.get(2000)();
  await settle();
  assert.match(a.$('#done').textContent, /Import complete/);
  a.$('#again').click();
  await settle();
  assert.match(a.$('#desk').textContent, /Insert a camera card/);
});

test('a date with a folder on the NAS keeps its title; another title skips what that folder already holds', async t => {
  const a = await app(t, true, { destinations: existing });
  assert.equal(a.$('#name-0').value, '夜景');
  assert.match(a.$('#path-0').textContent, /2026-11-06_夜景/);
  assert.equal(a.$('#note-0').textContent, 'Existing folder · 2 of 3 already there, checked and skipped');
  // Card summary: what's new and what's already on the NAS.
  const stats = [...a.window.document.querySelectorAll('.stat')].map(s => s.textContent);
  assert.deepEqual(stats.slice(0, 3), ['1new photos', '0new video clips', '2already on NAS, skipped']);
  assert.match(a.$('.legend').textContent, /Already on NAS 0\.0 GB/);
  assert.match(a.$('#summary').textContent, /^1 photo · 0\.0 GB → 1 folder$/);
  assert.match(a.$('#estimate').textContent, /2 files are already on the NAS: compared with the card, then skipped\./);

  a.type('#name-0', 'Night walk');
  assert.match(a.$('#path-0').textContent, /2026-11-06_Night-walk/);
  assert.equal(a.$('#note-0').textContent, 'New folder · 2 already in 2026-11-06_夜景 are checked and skipped, not copied again');
  a.type('#name-0', '');
  assert.match(a.$('#path-0').textContent, /2026\\2026-11-06\\$/);
  assert.match(a.$('#note-0').textContent, /^New folder/);

  a.type('#name-0', 'Night walk');
  a.$('#start').click();
  assert.equal(a.fixture.request.shoots[0].name, 'Night walk');
  a.fixture.finish(result({
    copied_files: 1, skipped_files: 3, skipped_elsewhere: 3, found_in: ['2026\\2026-11-06_夜景'],
    shoots: ['2026\\2026-11-06_Night-walk', '2026\\2026-11-06_夜景']
  }));
  await settle();
  assert.match(a.$('#done').textContent, /3 files were already in 2026-11-06_夜景 and weren't copied again\./);
  assert.match(a.$('#done-all, .done-all').textContent, /Open all 2 shoots in one catalog/);
});

test('stop shows how far it got and Resume import repeats the same selection', async t => {
  const a = await app(t);
  a.type('#name-0', 'Keep this name');
  a.$('#start').click();
  a.$('#stop').click();
  assert.ok(a.calls.some(c => c.command === 'cancel_import'));
  assert.equal(a.$('#stop').disabled, true);
  assert.match(a.$('#stopped').textContent, /Stopping/);
  a.fixture.finish(result({ status: 'cancelled', total_files: 1290, copied_files: 120, skipped_files: 0, verified_files: 120, error: 'Import stopped.' }));
  await settle();
  assert.match(a.$('#progress').textContent, /Stopped after 120 of 1,290 files\. Files already copied stay on the NAS; resuming checks and skips them\./);
  assert.equal(a.$('#done').hidden, true);
  assert.equal(a.$('#actions').hidden, false);
  assert.equal(a.$('#start').textContent, 'Resume import');
  assert.equal(a.$('#name-0').disabled, true);
  a.$('#start').click();
  assert.equal(a.fixture.requests.length, 2);
  assert.deepEqual(plain(a.fixture.requests[1]), plain(a.fixture.requests[0]));
  assert.match(a.$('#history').textContent, /Stopped/);
});

test('an import left unfinished is offered again for the same card after a restart', async t => {
  const pending = { request: { root: 'E:\\', nas_root: '\\\\NAS\\photos', expected_card_id: 'CARD-ID', shoots: [{ date: '2026-11-06', name: 'Saved name' }], include_videos: false, verify: true, workers: 0 }, eject: true };
  const a = await app(t, true, { pending, ledger: { imports: [result({ status: 'cancelled', total_files: 10, copied_files: 4, verified_files: 4 })] } });
  await settle();
  assert.equal(a.$('#name-0').value, 'Saved name');
  assert.equal(a.$('#opt-video').checked, false);
  assert.equal(a.$('#opt-eject').checked, true);
  assert.equal(a.$('#start').textContent, 'Resume import');
  assert.match(a.$('#progress').textContent, /Stopped after 4 of 10 files/);
  a.$('#start').click();
  assert.equal(a.fixture.request.shoots[0].name, 'Saved name');
  assert.equal(a.fixture.request.include_videos, false);

  const other = await app(t, true, { pending: { ...pending, request: { ...pending.request, expected_card_id: 'OTHER' } } });
  await settle();
  assert.equal(other.$('#name-0').value, '');
  assert.equal(other.$('#start').textContent, 'Import to NAS');
  assert.equal(other.$('#progress').hidden, true);
});

test('unverified copying disables ejection and never claims verification', async t => {
  const a = await app(t);
  a.$('#opt-eject').click();
  a.$('#opt-verify').click();
  a.$('#opt-video').click();
  assert.equal(a.$('#opt-eject').checked, false);
  assert.equal(a.$('#opt-eject').disabled, true);
  assert.match(a.$('#estimate').textContent, /won't be read back/);
  a.$('#start').click();
  assert.equal(a.fixture.request.verify, false);
  assert.equal(a.fixture.request.include_videos, false);
  assert.equal(a.fixture.request.eject, false);
  a.fixture.finish(result({ total_files: 2, copied_files: 2, verified_files: 0, videos: 0 }));
  await settle();
  assert.match(a.$('#done').textContent, /weren't checked against the card, so keep the card/);
  assert.match(a.$('#history').textContent, /2 files not checked/);
});

test('local or unknown destinations and unsaved edits block starting', async t => {
  for (const network of [false, null]) {
    const a = await app(t, network);
    assert.equal(a.$('#start').disabled, true);
    a.$('#start').click();
    assert.equal(a.fixture.request, null);
  }
  const a = await app(t);
  a.type('#nas-path', 'Z:\\different');
  assert.equal(a.$('#start').disabled, true);
  assert.match(a.$('#estimate').textContent, /Save the NAS path/);
});

test('failed imports and warnings stay visible as plain text', async t => {
  const a = await app(t);
  a.$('#start').click();
  a.fixture.finish(result({ status: 'failed', copied_files: 1, verified_files: 1, error: '<img src=x onerror=alert(1)> conflict', warnings: ['Card is busy. Eject in Explorer.'] }));
  await settle();
  assert.match(a.$('#progress').textContent, /Stopped by an error after 1 of 4 files/);
  assert.match(a.$('#progress').textContent, /conflict/);
  assert.match(a.$('#progress').textContent, /Card is busy/);
  assert.equal(a.$('#desk img'), null);
  assert.match(a.$('#history').textContent, /Failed/);
  assert.equal(a.$('#start').textContent, 'Resume import');
});

const nvmeHealth = { serial: 'BF7F0763030400117918', model: 'CEA-G240T', passed: true, critical_warning: 0, percentage_used: 2, available_spare: 100,
  data_written_bytes: 4.6e12, data_read_bytes: 9.3e12, power_cycles: 386, media_errors: 0 };

test('an NVMe card shows its health on insertion and is listed by serial with the In reader tag', async t => {
  const a = await app(t, true, {
    drive: { bus: 'NVMe', product: 'CEA-G240T', card_serial: 'BF7F0763030400117918' },
    health: nvmeHealth, cardKey: 'sn:BF7F0763030400117918',
    ledger: { cards: [
      { id: 'sn:BF7F0763030400117918', volume_id: 'CARD-ID', name: null, model: 'CEA-G240T', bus: 'NVMe', formats: 3, capacity_bytes: 240e9, filesystem: 'exFAT',
        first_seen: '2026-10-01T10:00:00', last_seen: '2026-11-07T10:00:00', insertions: 9, hardware_serial: 'BF7F0763030400117918', health: [{ at: '2026-11-07T10:00:00', health: nvmeHealth }] },
      { id: 'SD-VOL', volume_id: null, name: null, model: null, bus: 'USB', formats: 0, capacity_bytes: 128e9, filesystem: 'exFAT',
        first_seen: '2026-09-01T10:00:00', last_seen: '2026-10-19T10:00:00', insertions: 4, hardware_serial: null, health: [] }
    ], imports: [
      result({ card_id: 'SD-VOL', card_key: 'SD-VOL', completed_bytes: 14.9e9 }),
      result({ card_key: 'sn:BF7F0763030400117918', finished_at: '2026-10-12T10:00:00' })
    ] }
  });
  assert.match(a.$('#health-title').innerHTML, /Healthy/);
  assert.match(a.$('#health-body').textContent, /Read from the card when it was inserted/);
  assert.equal(a.$('#health-read').textContent, 'Read again');
  assert.match(a.$('.eyebrow').textContent, /Inserted card · drive E: · recognized by its serial number/);
  assert.equal(a.$('#card-name').textContent, 'CEA-G240T');
  assert.match(a.$('.card-title .chip').textContent, /CFexpress Type A · 240 GB · exFAT/);
  assert.match(a.$('.card-meta').textContent, /Last imported Oct 12 · 1 import by this app/);
  const [cf, sd] = a.window.document.querySelectorAll('#cards .item');
  assert.match(cf.textContent, /CEA-G240T/);
  assert.match(cf.querySelector('.chips').textContent, /In reader/);
  assert.match(cf.textContent, /CFexpress Type A/);
  assert.match(cf.textContent, /Written4\.60 TB/);
  assert.doesNotMatch(a.$('#cards').textContent, /In the reader now|health log|imported by this app|own counts|report wear/);
  assert.equal(sd.querySelector('.chip.live'), null);
  assert.match(sd.textContent, /No wear data/);
  assert.match(sd.textContent, /Imports1/);
  assert.match(sd.textContent, /Imported14\.9 GB/);
});

const day = (date, min, max, first, last) => ({ date, min, max, first_shot: date + 'T' + first, last_shot: date + 'T' + last, firmware: '1.01' });

test('cameras show the shutter count graph like the prototype', async t => {
  const a = await app(t, true, { cameras: {
    shared: { path: '\\\\NAS\\photos\\.card-intake\\shutter-counts.json', synced: true, error: null, folders: 5 },
    cameras: [
      { model: 'ILCE-1', name: 'α1', serial: '5021734', firmware: '2.01', rated_shutter: 500000, rating_source: 'Sony', shutter_reference: null, count: 148213, electronic_count: null,
        last_shot: '2026-11-04T17:05:00', days: [day('2026-10-26', 136480, 146872, '08:00:00', '18:00:00'), day('2026-11-04', 146900, 148213, '16:20:00', '17:05:00')] },
      { model: 'ILCE-7RM6', name: 'α7R VI', serial: '03506694', firmware: '1.01', rated_shutter: null, rating_source: null, shutter_reference: 500000, count: 894, electronic_count: null,
        last_shot: '2026-11-06T20:51:07', days: [day('2026-10-30', 2, 30, '17:54:04', '23:03:22'), day('2026-11-06', 699, 894, '17:21:17', '20:51:07')] }
    ] } });
  await settle();
  const [a1, a7r6] = a.window.document.querySelectorAll('#cameras .item');
  assert.match(a1.textContent, /Shutter count148,213/);
  assert.match(a1.textContent, /\+1,341 since Oct 26/);
  assert.match(a1.textContent, /29\.6% of 500,000 rated life/);
  assert.doesNotMatch(a.$('#cameras').textContent, /set by you|Electronic|Not reported|Mechanical/);
  assert.ok(a1.querySelector('svg.spark'));
  assert.ok(a7r6.querySelector('.life'));
  assert.match(a7r6.textContent, /0\.2% of 500,000 expected life/);
  assert.match(a7r6.querySelector('.spark-labels').textContent, /Oct 30 · 2.*Nov 6 · 894/);
  assert.equal(a.$('#cams-note').textContent, 'Shared through the NAS');
});

test('reading every NAS photo shows progress and can be stopped', async t => {
  const a = await app(t, true, { cameras: { cameras: [], shared: { path: '\\\\NAS\\photos\\.card-intake\\shutter-counts.json', synced: true, error: null, folders: 0 } } });
  await settle();
  assert.equal(a.$('#lib-scan').disabled, false);
  a.$('#lib-scan').click();
  assert.equal(a.$('#lib-scan').disabled, true);
  a.fixture.libraryChannel.onmessage({ phase: 'reading', progress: { folders_done: 1, folders_total: 4, photos_done: 120, photos_total: 287, current: '2026/2026-11-06_夜景' } });
  assert.match(a.$('#cameras').textContent, /120 of 287 photos · 2026\/2026-11-06_夜景/);
  a.$('#lib-stop').click();
  assert.ok(a.calls.some(c => c.command === 'cancel_library_scan'));
  a.fixture.library({ folders_read: 1, folders_unchanged: 2, photos_read: 120, shutter_counts: 120, failed: [], failed_count: 0, cancelled: true, cameras: a.fixture.cameras });
  await settle();
  assert.match(a.$('#toast').textContent, /Stopped\. Read 120 photos in 1 folder\. 2 folders were already read\./);
  assert.equal(a.$('#lib-scan').disabled, false);
});

test('without a connected share the shutter history stays on this computer', async t => {
  const a = await app(t);
  await settle();
  assert.match(a.$('#cams-note').textContent, /This computer only/);
  assert.equal(a.$('#lib-scan').disabled, true);
});

test('Lightroom: open a shoot or all shoots from the done panel, and Open or Reopen from the history', async t => {
  const a = await app(t, true, {
    catalogs: [{ import_id: '2026-10-12T10:00:00', state: 'local', catalog: 'D:\\x.lrcat' }, { import_id: '2026-10-01T10:00:00', state: 'nas', catalog: 'D:\\y.lrcat' }],
    ledger: { imports: [
      result({ started_at: '2026-10-12T10:00:00', finished_at: '2026-10-12T10:05:00' }),
      result({ started_at: '2026-10-01T10:00:00', finished_at: '2026-10-01T10:05:00', shoots: ['2026\\2026-10-01_A', '2026\\2026-10-01_B'] }),
      result({ started_at: '2026-09-01T10:00:00', status: 'cancelled' })
    ] }
  });
  await settle();
  const rows = a.window.document.querySelectorAll('#history tbody tr');
  assert.match(rows[0].textContent, /On this PCOpen/);
  assert.match(rows[1].textContent, /2026-10-01_A \+1 more/);
  assert.match(rows[1].textContent, /Stored on NASReopen/);
  assert.match(rows[2].lastElementChild.textContent, /—/);
  rows[1].querySelector('[data-hopen]').click();
  await settle();
  assert.deepEqual(plain(a.calls.filter(c => c.command === 'open_in_lightroom').pop().args), { importId: '2026-10-01T10:00:00', shoot: null });

  a.$('#start').click();
  a.fixture.finish(result({ shoots: ['2026\\2026-11-06_Tokyo', '2026\\2026-11-07_Kyoto'] }));
  await settle();
  a.$('[data-open="1"]').click();
  await settle();
  assert.deepEqual(plain(a.calls.filter(c => c.command === 'open_in_lightroom').pop().args), { importId: '2026-11-06T22:00:00', shoot: 1 });
  assert.equal(a.$('[data-open="1"]').textContent, 'Open again');
  assert.match(a.$('#toast').textContent, /Opened 2026-11-06_Tokyo\.lrcat in Lightroom Classic/);
  a.$('#open-all').click();
  await settle();
  assert.equal(a.calls.filter(c => c.command === 'open_in_lightroom').pop().args.shoot, null);
  a.fixture.lrStatus = { status: 'complete', added: 1284, skipped: 0, failed: 0, detail: '' };
  a.timers.get(1500)();
  await settle();
  assert.match(a.$('#toast').textContent, /Lightroom added 1,284 photos to 2026-11-06_Tokyo\.lrcat/);
});
