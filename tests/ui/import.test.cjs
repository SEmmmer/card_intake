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
  const removalTimeouts = [];
  const setTimeout = window.setTimeout.bind(window);
  window.setTimeout = (fn, ms) => {
    if (ms === 5000) { removalTimeouts.push(fn); return -removalTimeouts.length; }
    return setTimeout(fn, ms);
  };
  const fixture = {
    roots: [{ root: 'E:\\', has_sony_video: true }],
    ledger: { cards: [], cameras: [], imports: [] },
    request: null, channel: null, finish: null,
    pending: options.pending || null,
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
    details: options.details || {},
    card_key: options.cardKey || 'CARD-ID',
    photos: 2, photo_bytes: 200, videos: 1, video_bytes: 100,
    camera: options.camera || { model: 'ILCE-7RM6', serial: '03506694', shutter_count: 894 },
    scan_ms: 12, times: { exif: 2, sidecar: 1, file_time: 0 },
    groups: options.groups || [{ date: '2026-11-06', first: '2026-11-06T17:00:00', last: '2026-11-06T21:00:00', photos: 2, photo_bytes: 200, videos: 1, video_bytes: 100 }],
    destinations: options.destinations || []
  };
  window.__TAURI__ = { core: {
    Channel: class { onmessage() {} },
    invoke: (command, args) => {
      calls.push({ command, args });
      switch (command) {
        case 'list_cards': return Promise.resolve(fixture.roots);
        case 'read_card':
          if (fixture.removedCard && fixture.removedCard.id === report.card_key) {
            fixture.ledger.cards.push({ ...fixture.removedCard, registered: false, insertions: 1, formats: 0, health: [] });
            fixture.removedCard = null;
          }
          return Promise.resolve(report);
        case 'get_ledger': return Promise.resolve(fixture.ledger);
        case 'nas_status': return Promise.resolve(fixture.nas);
        case 'cancel_import': return Promise.resolve();
        case 'get_pending_import': return Promise.resolve(fixture.pending);
        case 'get_lightroom_settings': return Promise.resolve({ settings: { executable: 'C:\\Adobe\\Lightroom.exe', template: 'C:\\Empty.lrcat', catalog_root: 'D:\\LrC Catalogs', presets: null }, plugin: PLUGIN, presets_installed: 0 });
        case 'save_lightroom_settings': return Promise.resolve({ settings: args.settings, plugin: PLUGIN, presets_installed: args.settings.presets ? 98 : 0 });
        case 'lightroom_catalogs': return Promise.resolve(fixture.catalogs);
        case 'delivery_states': return Promise.resolve(fixture.deliveries || options.deliveries || []);
        case 'plan_delivery': return new Promise((resolve, reject) => { fixture.plan = resolve; fixture.planError = reject; });
        case 'deliver': return new Promise((resolve, reject) => { fixture.deliver = resolve; fixture.deliverError = reject; });
        case 'get_delivery_settings': return Promise.resolve(options.delivery || { deliver_to: '', url: '', has_key: false, library_id: '', libraries: [] });
        case 'save_delivery_settings':
          fixture.deliverySaved = args.settings;
          return Promise.resolve({ deliver_to: args.settings.deliver_to, url: args.settings.url, has_key: true, library_id: 'lib1', libraries: [{ id: 'lib1', name: 'MyPhoto', assetCount: 42, importPaths: ['/photos/MyPhoto'] }] });
        case 'get_settings': return Promise.resolve(options.settings || { folded: [] });
        case 'save_folded': fixture.folded = args.folded; return Promise.resolve(null);
        case 'open_in_lightroom': return Promise.resolve(fixture.launch || { catalog: 'D:\\LrC Catalogs\\2026\\2026-11-06_Tokyo\\2026-11-06_Tokyo.lrcat', name: '2026-11-06_Tokyo', already_open: false });
        case 'finish_catalog': return Promise.resolve({ name: '2026-11-06_Tokyo', stored: '\\\\NAS\\photos\\2026\\2026-11-06_Tokyo\\_catalog' });
        case 'get_cameras': return Promise.resolve(fixture.cameras);
        case 'read_card_shutter': return Promise.resolve(fixture.cameras);
        case 'register_camera':
          if (options.camerasAfterRegister) fixture.cameras = options.camerasAfterRegister;
          return Promise.resolve(fixture.cameras);
        case 'register_card':
          fixture.ledger.cards.forEach(c => { if (c.id === args.cardKey) c.registered = true; });
          return Promise.resolve(plain(fixture.ledger));
        case 'remove_camera':
          if (options.removeCameraError) return Promise.reject(options.removeCameraError);
          fixture.cameras = options.camerasAfterRemove || {
            ...fixture.cameras,
            cameras: fixture.cameras.cameras.filter(c => !(c.model === args.model && (args.serial ? c.serial === args.serial : c.internal_serial === args.internalSerial)))
          };
          return Promise.resolve(fixture.cameras);
        case 'remove_card':
          if (options.removeCardError) return Promise.reject(options.removeCardError);
          fixture.removedCard = fixture.ledger.cards.find(c => c.id === args.cardKey);
          fixture.ledger.cards = fixture.ledger.cards.filter(c => c.id !== args.cardKey);
          return Promise.resolve(plain(fixture.ledger));
        case 'read_card_health':
          if (options.ledgerAfterHealth) Object.assign(fixture.ledger, options.ledgerAfterHealth);
          return Promise.resolve(options.healthAnswer);
        case 'cancel_library_scan': return Promise.resolve();
        case 'eject_card':
          if (options.ejectError) return Promise.reject(options.ejectError);
          fixture.roots = fixture.roots.filter(r => r.root !== args.root);
          return Promise.resolve();
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
  const last = command => plain(calls.filter(c => c.command === command).pop().args);
  return { window, fixture, calls, timers, removalTimeouts, $, type, last };
}

const PLUGIN = 'C:\\Users\\me\\AppData\\Roaming\\Adobe\\Lightroom\\Modules\\CardIntake.lrplugin';
const cardDay = date => ({ date, first: date + 'T17:00:00', last: date + 'T21:00:00', photos: 2, photo_bytes: 200, videos: 1, video_bytes: 100 });

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

test('Deliver has its own button beside Finish/Reopen and works with open and uncreated catalogs', async t => {
  for (const state of ['open','local','nas','none']) {
    const r = result();
    const a = await app(t, true, { ledger: { imports: [r] }, catalogs: [{ import_id: r.started_at, shoots: [0], name: '2026-11-06_Tokyo', trip: false, state }] });
    const cell = a.$('#history .cat-cell');
    assert.equal(cell.querySelectorAll('[data-deliver]').length, 1);
    assert.equal(cell.querySelector('[data-deliver]').disabled, false);
    assert.ok(cell.querySelector('[data-deliver]').classList.contains('go'));
    if (state === 'local') assert.equal(cell.querySelector('[data-hfinish]').textContent, 'Finish');
    if (state === 'nas') assert.equal(cell.querySelector('[data-hopen]').textContent, 'Reopen');
  }
});

// What plan_delivery returns for one shoot: seven new PixCake JPEGs, six Lightroom twins skipped.
function plannedShoot(overrides = {}) {
  return {
    shoot: '\\\\NAS\\photos\\2026\\2026-11-06_夜景', target: '\\\\NAS\\Picture\\MyPhoto\\2026-11-06_夜景', target_exists: false, plan_only: true,
    new: 7, existing: 0, delivered: 0,
    photos: Array.from({ length: 7 }, (_, i) => ({ name: `EM_${i}.jpg`, source: `final\\EM_${i}_(2).jpg`, kind: 'pixcake', stars: 5, stars_from: 'catalog', action: 'new', thumbnail: 'data:image/jpeg;base64,/9j/AAAA' })),
    skipped: Array.from({ length: 6 }, (_, i) => ({ source: `topix\\EM_${i}.jpg`, reason: 'replaced by PixCake' })),
    warnings: [], errors: [], ...overrides
  };
}
const delivered = s => ({ ...s, plan_only: false, delivered: s.new,
  photos: s.photos.map(p => ({ ...p, thumbnail: undefined, action: p.action === 'new' ? 'delivered' : p.action })) });

test('Deliver first shows the new photos with thumbnails, and copies only what was confirmed', async t => {
  const r = result({ shoots: ['2026\\2026-11-06_夜景', '2026\\2026-11-07_Tokyo'] });
  const a = await app(t, true, { ledger: { imports: [r] }, catalogs: [{ import_id: r.started_at, shoots: [0, 1], name: '2026-11-06_Japan', trip: true, state: 'open' }] });
  assert.equal(a.window.document.querySelectorAll('[data-deliver]').length, 1);
  a.$('[data-deliver]').click();
  assert.deepEqual(a.last('plan_delivery'), { importId: r.started_at, shoot: 0 });
  assert.equal(a.$('[data-deliver]').disabled, true);
  assert.equal(a.$('[data-deliver]').textContent, 'Delivering…');
  assert.equal(a.$('#start').disabled, true);
  const tokyo = plannedShoot({
    shoot: '\\\\NAS\\photos\\2026\\2026-11-07_Tokyo', target: '\\\\NAS\\Picture\\MyPhoto\\2026-11-07_Tokyo', target_exists: true, new: 1, existing: 2, skipped: [],
    photos: [
      { name: 'DSC1.jpg', source: 'DSC1.jpg', kind: 'lightroom', stars: 3, stars_from: 'catalog', action: 'exists' },
      { name: 'DSC2.jpg', source: 'out\\DSC2.jpg', kind: 'lightroom', stars: 0, stars_from: 'none', action: 'new' },
      { name: 'DSC3.jpg', source: 'DSC3.jpg', kind: 'lightroom', stars: 4, stars_from: 'catalog', action: 'exists' }
    ]
  });
  a.fixture.plan({ shoots: [plannedShoot(), tokyo], errors: [] }); await settle();
  assert.equal(a.$('#deliver-dialog').hidden, false);
  assert.equal(a.$('#deliver-title').textContent, 'Deliver 8 photos?');
  assert.equal(a.$('#deliver-confirm').textContent, 'Deliver 8 photos');
  const body = a.$('#deliver-body');
  assert.equal(body.querySelectorAll('.thumb').length, 8);
  assert.equal(body.querySelectorAll('.thumb img').length, 7);
  assert.equal(body.querySelector('.thumb img').getAttribute('src'), 'data:image/jpeg;base64,/9j/AAAA');
  assert.match(body.textContent, /To \\\\NAS\\Picture\\MyPhoto\\2026-11-06_夜景 · a new folder/);
  assert.match(body.textContent, /2026-11-07_Tokyo · adds to the folder that’s already there/);
  assert.match(body.textContent, /Skipped: 6 Lightroom JPEGs replaced by PixCake\./);
  assert.match(body.textContent, /Skipped: 2 photos are already there and will be left alone\./);
  assert.match(body.textContent, /EM_0\.jpg★★★★★ · PixCake/);
  assert.match(body.textContent, /DSC2\.jpgNo stars · Lightroom/);
  assert.equal(a.calls.some(c => c.command === 'deliver'), false, 'nothing is copied before confirming');

  a.$('#deliver-confirm').click();
  assert.equal(a.$('#deliver-dialog').hidden, true);
  const sent = a.last('deliver');
  assert.equal(sent.importId, r.started_at);
  assert.deepEqual(sent.confirmed.map(s => s.photos.length), [7, 1]);
  assert.deepEqual(sent.confirmed[1], { shoot: tokyo.shoot, photos: [{ name: 'DSC2.jpg', source: 'out\\DSC2.jpg' }] });
  assert.equal(a.$('[data-deliver]').textContent, 'Delivering…');
  a.fixture.deliveries = [{ import_id: r.started_at, shoot: 0, count: 8, last_delivered: new Date().toISOString() }];
  a.fixture.deliver({ shoots: [delivered(plannedShoot()), delivered(tokyo)], rescanning: true, warnings: [], errors: [] }); await settle();
  assert.match(a.$('#toast').textContent, /^Delivered 8 photos to 2 folders: 7 from PixCake, 1 from Lightroom\. Immich is rescanning\.$/);
  assert.match(a.$('#history').textContent, /Delivered 8 · today/);
  assert.equal(a.$('[data-deliver]').disabled, false); assert.equal(a.$('#start').disabled, false);
  assert.match(a.$('#delivery-details').textContent, /EM_0\.jpg ← final\\EM_0_\(2\)\.jpg · PixCake · 5 stars \(catalog\) · delivered/);
  assert.match(a.$('#delivery-details').textContent, /DSC1\.jpg ← DSC1\.jpg · Lightroom · 3 stars \(catalog\) · already there, left alone/);
  assert.equal(a.calls.filter(c => c.command === 'finish_catalog').length, 0);
});

test('pressing Deliver again with nothing new says so and copies nothing', async t => {
  const r = result({ shoots: ['2026\\2026-11-06_夜景'] });
  const a = await app(t, true, { ledger: { imports: [r] }, catalogs: [{ import_id: r.started_at, shoots: [0], name: '2026-11-06_夜景', trip: false, state: 'nas' }] });
  a.$('[data-deliver]').click();
  const again = plannedShoot({ target_exists: true, new: 0, existing: 7 });
  again.photos = again.photos.map(p => ({ ...p, action: 'exists', thumbnail: undefined }));
  a.fixture.plan({ shoots: [again], errors: [] }); await settle();
  assert.equal(a.$('#deliver-dialog').hidden, true);
  assert.equal(a.$('#toast').textContent, 'Nothing new to deliver: all 7 photos are already in MyPhoto\\2026-11-06_夜景. Nothing was changed.');
  assert.equal(a.calls.some(c => c.command === 'deliver'), false);
  assert.equal(a.$('[data-deliver]').disabled, false);

  a.$('[data-deliver]').click();
  a.fixture.plan({ shoots: [plannedShoot({ new: 0, photos: [], skipped: [{ source: 'DSC00001.JPG', reason: 'camera JPEG' }] })], errors: [] }); await settle();
  assert.match(a.$('#toast').textContent, /^No finished JPEGs to deliver in 2026-11-06_夜景\. Export them from Lightroom or PixCake/);
  assert.equal(a.calls.some(c => c.command === 'deliver'), false);
});

test('Cancel or Escape closes the confirmation without copying anything', async t => {
  const r = result();
  const a = await app(t, true, { ledger: { imports: [r] }, catalogs: [{ import_id: r.started_at, shoots: [0], name: 'Tokyo', trip: false, state: 'none' }] });
  a.$('[data-deliver]').click(); a.fixture.plan({ shoots: [plannedShoot()], errors: [] }); await settle();
  assert.equal(a.$('#deliver-dialog').hidden, false);
  a.$('#deliver-cancel').click();
  assert.equal(a.$('#deliver-dialog').hidden, true);
  assert.equal(a.$('[data-deliver]').disabled, false); assert.equal(a.$('#start').disabled, false);
  a.$('[data-deliver]').click(); a.fixture.plan({ shoots: [plannedShoot()], errors: [] }); await settle();
  a.window.document.dispatchEvent(new a.window.KeyboardEvent('keydown', { key: 'Escape' }));
  assert.equal(a.$('#deliver-dialog').hidden, true);
  assert.equal(a.calls.some(c => c.command === 'deliver'), false);
});

test('Deliver errors restore the button, open the setup when needed, and an import disables delivery', async t => {
  const r = result();
  const a = await app(t, true, { ledger: { imports: [r] }, catalogs: [{ import_id: r.started_at, shoots: [0], name: 'Tokyo', trip: false, state: 'none' }] });
  a.$('[data-deliver]').click();
  a.fixture.planError('Set the finished photos folder (e.g. \\\\NAS\\Picture\\MyPhoto) under Deliver first.'); await settle();
  assert.equal(a.$('#deliver-setup').open, true);
  assert.match(a.$('#toast').textContent, /finished photos folder/);
  assert.equal(a.$('[data-deliver]').disabled, false);
  a.$('[data-deliver]').click(); a.fixture.plan({ shoots: [plannedShoot()], errors: [] }); await settle();
  a.$('#deliver-confirm').click();
  a.fixture.deliverError('The photos to deliver changed since you checked them. Press Deliver again to see the new list.'); await settle();
  assert.equal(a.$('[data-deliver]').disabled, false); assert.match(a.$('#toast').textContent, /changed since you checked/);
  a.$('#start').click(); assert.equal(a.$('[data-deliver]').disabled, true);
});

test('Deliver settings keep the finished photos folder and Immich library, and never show the key', async t => {
  const a = await app(t);
  assert.equal(a.$('#immich-patterns'), null, 'no exclusion patterns any more');
  a.type('#deliver-to', '\\\\NAS\\Picture\\MyPhoto'); a.type('#immich-url', 'http://nas:2283'); a.type('#immich-key', 'test-secret');
  assert.equal(a.$('#immich-key').type, 'password');
  a.$('#deliver-save').click(); await settle();
  assert.deepEqual(a.last('save_delivery_settings').settings, { deliver_to: '\\\\NAS\\Picture\\MyPhoto', url: 'http://nas:2283', api_key: 'test-secret', library_id: '' });
  assert.equal(a.$('#immich-key').value, ''); assert.match(a.$('#immich-key').placeholder, /Saved key/);
  assert.equal(a.window.document.body.textContent.includes('test-secret'), false);
  assert.equal(a.$('#immich-library').value, 'lib1');
  assert.match(a.$('#immich-library').selectedOptions[0].textContent, /MyPhoto · \/photos\/MyPhoto/);
  assert.equal(a.$('#deliver-note').textContent, 'Ready · Immich');
  assert.equal(a.$('#deliver-to').value, '\\\\NAS\\Picture\\MyPhoto');
  a.$('#deliver-save').click(); await settle();
  assert.equal(a.last('save_delivery_settings').settings.api_key, '');
});

test('a shoot folder gone from the NAS says so, while the import\'s other shoot still opens', async t => {
  const r = result({ shoots: ['2026\\2026-11-03', '2026\\2026-11-06_夜景'] });
  const note = '2026\\2026-11-03 isn\'t on the NAS any more. It may have been renamed or deleted.';
  const a = await app(t, true, { ledger: { imports: [r] }, catalogs: [
    { import_id: r.started_at, shoots: [0], name: '2026-11-03', trip: false, state: 'missing', note },
    { import_id: r.started_at, shoots: [1], name: '2026-11-06_夜景', trip: false, state: 'none' }
  ] });
  const cells = a.window.document.querySelectorAll('#history .cat-cell');
  assert.equal(cells[0].textContent, '2026-11-03Folder missing');
  assert.equal(cells[0].querySelector('.chip.warn').title, note);
  assert.equal(cells[0].querySelectorAll('button').length, 0);
  cells[1].querySelector('[data-hopen]').click();
  await settle();
  assert.deepEqual(a.last('open_in_lightroom'), { importId: r.started_at, shoot: 1, trip: null });
});

test('imports that fill the same single catalog share one row with their totals', async t => {
  const second = result({ started_at: '2026-11-07T18:44:17', finished_at: '2026-11-07T18:45:21', shoots: ['2026\\2026-11-07_test'],
    photos: 215, videos: 0, total_files: 215, verified_files: 215, total_bytes: 15e9, completed_bytes: 15e9, average_bytes_per_sec: 300e6 });
  const first = result({ started_at: '2026-11-07T15:49:01', finished_at: '2026-11-07T15:49:11', shoots: ['2026\\2026-11-07_test'],
    photos: 27, videos: 0, total_files: 27, verified_files: 27, total_bytes: 2e9, completed_bytes: 2e9, average_bytes_per_sec: 200e6 });
  const both = result({ started_at: '2026-11-07T01:47:34', finished_at: '2026-11-07T01:48:31', shoots: ['2026\\2026-11-03_video-test', '2026\\2026-11-06_夜景'] });
  const a = await app(t, true, { ledger: { imports: [second, first, both] }, catalogs: [
    { import_id: second.started_at, shoots: [0], name: '2026-11-07_test', trip: false, state: 'local' },
    { import_id: first.started_at, shoots: [0], name: '2026-11-07_test', trip: false, state: 'local' },
    { import_id: both.started_at, shoots: [0], name: '2026-11-03_video-test', trip: false, state: 'none' },
    { import_id: both.started_at, shoots: [1], name: '2026-11-06_夜景', trip: false, state: 'none' }
  ] });
  await settle();
  const rows = a.window.document.querySelectorAll('#history tbody tr');
  assert.equal(rows.length, 2);
  const cells = rows[0].querySelectorAll('td');
  assert.match(cells[0].textContent, /2 imports/);
  assert.match(cells[0].querySelector('.chip.merged').title, /215 photos[\s\S]*27 photos/);
  assert.equal(cells[3].textContent, '242');
  assert.equal(cells[4].textContent, '17.0 GB');
  assert.equal(cells[5].textContent, '283 MB/s');
  assert.equal(cells[7].textContent, '2026-11-07_test');
  assert.equal(rows[0].querySelectorAll('[data-deliver]').length, 1);
  assert.equal(rows[0].querySelectorAll('[data-hfinish]').length, 1);
  rows[0].querySelector('[data-deliver]').click();
  assert.deepEqual(a.last('plan_delivery'), { importId: second.started_at, shoot: 0 });
  // The import holding two shoots keeps its own row.
  assert.equal(rows[1].querySelectorAll('[data-deliver]').length, 2);
  assert.doesNotMatch(rows[1].textContent, /imports/);
});

test('each camera and card folds to its name, and the app remembers it', async t => {
  const a = await app(t, true, {
    settings: { folded: ['card:OTHER'] },
    cameras: { shared, cameras: [a7r6()], new_cameras: [] },
    ledger: { cards: [{ id: 'OTHER', name: 'Other card', last_seen: '2026-11-07T12:00:00' }] }
  });
  await settle();
  const card = () => a.$('#cards .item');
  const camera = () => a.$('#cameras article.item');
  assert.ok(card().classList.contains('folded'));
  assert.equal(card().querySelector('.facts'), null);
  assert.match(card().textContent, /Other card/);
  assert.equal(card().querySelector('[data-fold]').getAttribute('aria-expanded'), 'false');
  assert.ok(!camera().classList.contains('folded'));
  assert.match(camera().textContent, /Shutter count894/);
  camera().querySelector('[data-fold]').click();
  assert.ok(camera().classList.contains('folded'));
  assert.doesNotMatch(camera().textContent, /Shutter count/);
  assert.match(camera().textContent, /ILCE-7RM6/);
  assert.deepEqual(a.last('save_folded').folded.sort(), ['camera:03506694', 'card:OTHER']);
  card().querySelector('[data-fold]').click();
  assert.ok(!card().classList.contains('folded'));
  assert.ok(card().querySelector('.facts'));
  assert.deepEqual(a.last('save_folded').folded, ['camera:03506694']);
});

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

  a.fixture.channel.onmessage({ phase: 'rating', completed_files: 4, total_files: 4, processed_bytes: 310, total_bytes: 310 });
  assert.equal(a.$('#phase').textContent, 'Writing initial raw ratings');
  a.fixture.finish(result({ ratings: { rated: 2, locked: 1, camera: 0, computed: 1, kept: 1 } }));
  await settle();
  const done = a.$('#done');
  assert.equal(done.hidden, false);
  assert.match(done.textContent, /Import complete/);
  assert.match(done.textContent, /Rated 2 raws: 1 locked in camera, 0 camera ratings, 1 computed\. Kept 1 existing rating\./);
  assert.match(done.textContent, /All 2 photos and 1 clip are on the NAS and match the card\. You can format this card in the camera\./);
  assert.equal(a.$('#done-path-0').textContent, '\\\\NAS\\photos\\2026\\2026-11-06_Tokyo');
  assert.match(done.textContent, /Catalog: D:\\LrC Catalogs\\2026\\2026-11-06_Tokyo\\2026-11-06_Tokyo\.lrcat/);
  assert.ok(done.querySelector('[data-open="0"]'));
  assert.equal(a.$('#open-trip'), null);
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

test('Eject ejects the card on screen, checking it is still the same card', async t => {
  const a = await app(t);
  const eject = a.$('#eject');
  assert.equal(eject.textContent, 'Eject');
  assert.equal(eject.disabled, false);
  eject.click();
  assert.equal(eject.textContent, 'Ejecting…');
  assert.equal(eject.disabled, true);
  await settle();
  assert.deepEqual(a.last('eject_card'), { root: 'E:\\', expectedId: 'CARD-ID' });
  assert.equal(a.$('#toast').textContent, 'Ejected drive E:. You can take the card out.');
  assert.match(a.$('#desk').textContent, /Insert a camera card/);
});

test('Eject waits for an import; afterwards the done panel stays and says the card is ejected', async t => {
  const a = await app(t);
  a.$('#start').click();
  assert.equal(a.$('#eject').disabled, true);
  a.fixture.finish(result());
  await settle();
  assert.equal(a.$('#eject').disabled, false);
  a.$('#eject').click();
  await settle();
  assert.match(a.$('#done').textContent, /Import complete.*The card has been ejected\./);
  assert.equal(a.$('#eject').textContent, 'Ejected');
  assert.equal(a.$('#eject').disabled, true);
  // An import that ejected the card itself shows it the same way.
  const b = await app(t);
  b.$('#opt-eject').click();
  b.$('#start').click();
  b.fixture.finish(result({ ejected: true }));
  await settle();
  assert.equal(b.$('#eject').textContent, 'Ejected');
});

test('a failed eject says why, keeps the card on screen and can be tried again', async t => {
  const a = await app(t, true, { ejectError: "Windows couldn't safely eject the card (Access is denied). Close other apps using it and eject it in Explorer." });
  a.$('#eject').click();
  await settle();
  assert.match(a.$('#toast').textContent, /Close other apps using it/);
  assert.equal(a.$('#eject').textContent, 'Eject');
  assert.equal(a.$('#eject').disabled, false);
  assert.match(a.$('#desk').textContent, /Shoots on this card/);
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
  assert.match(a.$('.done-all').textContent, /Open as a trip.*One catalog for all 2 shoots/);
  assert.equal(a.$('#trip-name').value, '');
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
    health: { ...nvmeHealth, host_write_commands: 1042602, host_read_commands: 2316330 }, cardKey: 'sn:BF7F0763030400117918',
    // What Windows reveals about a healthy card on the NVMe bus: nothing worth a row.
    details: { write_protected: false, capacity_bytes: 240057409536, partition_table: 'MBR', partition_offset_bytes: 33554432, partition_type: '07',
      file_system: { name: 'exFAT', revision: '1.00', cluster_bytes: 262144, dirty: false, media_failure: false } },
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
  assert.doesNotMatch(a.$('#health-body').textContent, /commands/i);
  assert.equal(a.$('#card-details'), null);
  assert.match(a.$('.eyebrow').textContent, /Inserted card · drive E: · NVMe drive · recognized by its serial number/);
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

const z9Health = { serial: '602123456789', model: 'CEB-G256T', passed: true, critical_warning: 0, percentage_used: 1, available_spare: 100,
  data_written_bytes: 1.2e12, data_read_bytes: 3.4e12, power_cycles: 91, media_errors: 0 };

test('a Nikon Z 9 card reads as an NVMe drive: its own model, serial and health', async t => {
  const a = await app(t, true, {
    // A CFexpress Type B card from a Z 9, in a reader that shows the card itself.
    drive: { bus: 'NVMe', product: 'CEB-G256T', card_serial: '602123456789' },
    health: z9Health, cardKey: 'sn:602123456789',
    camera: { model: 'NIKON Z 9', serial: '3005326', shutter_count: 48213 },
    ledger: { cards: [
      { id: 'sn:602123456789', volume_id: 'CARD-ID', name: null, model: 'CEB-G256T', bus: 'NVMe', formats: 0, capacity_bytes: 240e9, filesystem: 'exFAT',
        first_seen: '2026-11-07T09:20:00', last_seen: '2026-11-07T09:20:00', insertions: 1, hardware_serial: '602123456789', health: [] }
    ] }
  });
  // The card is named and recognised by its own serial number, not the reader's.
  assert.equal(a.$('#card-name').textContent, 'CEB-G256T');
  assert.match(a.$('.eyebrow').textContent, /NVMe drive · recognized by its serial number/);
  assert.match(a.$('.card-title .chip').textContent, /CFexpress Type B · 240 GB · exFAT/);
  assert.match(a.$('.camera-tag').textContent, /NIKON Z 9/);
  assert.match(a.$('.camera-tag').textContent, /SN 3005326/);
  // Its health log is read on insertion, like any card the reader shows as NVMe.
  assert.match(a.$('#health-title').innerHTML, /Healthy/);
  assert.match(a.$('#health-body').textContent, /Read from the card when it was inserted/);
  const [card] = a.window.document.querySelectorAll('#cards .item');
  assert.match(card.textContent, /CEB-G256T/);
  assert.match(card.textContent, /CFexpress Type B/);
  assert.match(card.querySelector('.chips').textContent, /In reader/);
});

const bridgedHealth = { serial: 'P640GA20223532F1100', model: 'CHIPFANCIER CFE Platinum', firmware: 'ELFM20.2', passed: true,
  critical_warning: 0, percentage_used: 0, available_spare: 100, data_written_bytes: 7.35e12, data_read_bytes: 5.88e12,
  power_cycles: 1250, media_errors: 0, bridge: 'JMicron JMS583', device_type: null };

test('a Type B card behind a JMicron USB bridge is read after asking for admin rights, then known by its serial', async t => {
  const card = (id, overrides) => ({ id, volume_id: 'CARD-ID', name: null, model: null, bus: 'USB', formats: 0, capacity_bytes: 650e9,
    filesystem: 'exFAT', first_seen: '2026-11-07T09:20:00', last_seen: '2026-11-07T09:20:00', insertions: 1, hardware_serial: null, health: [], ...overrides });
  const known = { model: 'CHIPFANCIER CFE Platinum', hardware_serial: 'P640GA20223532F1100', first_seen: '2026-10-01T10:00:00' };
  const a = await app(t, true, {
    // Without admin rights the bridge can't say which card it is.
    drive: { bus: 'USB', product: 'CHIPFANCIER', card_serial: null, usb_id: '152D:0583', bridge: 'JMicron JMS583' },
    camera: { model: 'NIKON Z 9', serial: '3005326', shutter_count: 48213 },
    ledger: { cards: [
      // The card as inserted, new since it was formatted in the camera...
      card('CARD-ID'),
      // ...and the same card, read before.
      card('sn:P640GA20223532F1100', { ...known, volume_id: 'OLD-ID', last_seen: '2026-10-01T10:00:00',
        health: [{ at: '2026-10-01T10:00:00', health: bridgedHealth }] })
    ] },
    // The reading folds the inserted card into the card's own record.
    healthAnswer: { kind: 'read', health: bridgedHealth, card_key: 'sn:P640GA20223532F1100' },
    ledgerAfterHealth: { cards: [card('sn:P640GA20223532F1100', { ...known, formats: 1, insertions: 2,
      health: [{ at: '2026-10-01T10:00:00', health: bridgedHealth }, { at: '2026-11-07T09:21:00', health: bridgedHealth }] })] }
  });
  assert.match(a.$('.eyebrow').textContent,
    /Inserted card · drive E: · USB reader with a JMicron JMS583 bridge · recognized by its volume ID, which formatting changes/);
  // The bridge is USB-to-NVMe, so the card is a CFexpress card; the reader's name isn't the card's model.
  assert.match(a.$('.card-title .chip').textContent, /^CFexpress · 240 GB · exFAT/);
  assert.equal(a.$('#health-read').textContent, 'Read card health');

  a.$('#health-read').click();
  assert.match(a.$('#health-body').textContent, /choose Yes when asked for admin rights/);
  await settle();
  assert.equal(a.last('read_card_health').root, 'E:\\');
  assert.match(a.$('#health-title').innerHTML, /Healthy/);
  assert.match(a.$('#health-body').textContent, /Read just now through the reader’s JMicron JMS583 bridge\./);
  assert.match(a.$('#health-body').textContent, /Card serialP640GA20223532F1100/);
  assert.match(a.$('#health-body').textContent, /Card modelCHIPFANCIER CFE Platinum/);
  assert.match(a.$('#health-body').textContent, /FirmwareELFM20\.2/);
  // One card, in the reader, typed by its health log.
  const items = a.window.document.querySelectorAll('#cards .item');
  assert.equal(items.length, 1);
  assert.match(items[0].textContent, /CHIPFANCIER CFE Platinum/);
  assert.match(items[0].textContent, /SN P640GA20223532F1100/);
  assert.match(items[0].querySelector('.item-sub').textContent, /^CFexpress · 650 GB · exFAT/);
  assert.match(items[0].querySelector('.chips').textContent, /In reader/);
});

// The DJI Osmo Pocket 3's 256 GB card in a two-slot Genesys Logic USB reader.
const sdDetails = {
  reader: { name: 'USB Storage', usb_id: '05E3:0764', usb_version: '3.20', protocol: 'BOT', link: 'USB 5 Gbps', link_capable: 'USB 5 Gbps' },
  slot: 2, slots: 2, removable: true, write_protected: false, capacity_bytes: 255869321216, sector_bytes: 512, sd_class: 'SDXC',
  partition_table: 'MBR', disk_signature: '00000000', partition_offset_bytes: 32768, partition_type: '07',
  file_system: { name: 'exFAT', cluster_bytes: 131072, revision: '1.00', dirty: false, media_failure: false, percent_in_use: 81, data_offset_bytes: 7995392 }
};

test('an SD card in a USB reader shows what it reveals, and no health button', async t => {
  const a = await app(t, true, {
    drive: { bus: 'USB', product: 'MassStorageClass', card_serial: null, usb_id: '05E3:0764', bridge: null },
    details: sdDetails,
    camera: { model: 'PP-101', serial: '5WCQNBL6BB1A22', shutter_count: null },
    ledger: { cards: [
      { id: 'CARD-ID', volume_id: 'CARD-ID', name: null, model: null, bus: 'USB', sd_class: 'SDXC', formats: 0, capacity_bytes: 256e9, filesystem: 'exFAT',
        first_seen: '2026-11-07T04:52:00', last_seen: '2026-11-07T04:52:00', insertions: 1, hardware_serial: null, health: [] }
    ] }
  });
  assert.match(a.$('.card-title .chip').textContent, /^SDXC · 240 GB · exFAT/);
  // No health log to read, and no admin prompt to go with it.
  assert.equal(a.$('#health-read').hidden, true);
  assert.match(a.$('#health-body').textContent, /SD cards keep no health log of their own/);
  const details = a.$('#card-details').textContent;
  assert.match(details, /ReaderUSB Storage \(05E3:0764\)/);
  assert.match(details, /ConnectionUSB 5 Gbps · bulk-only/);
  assert.match(details, /Slot2 of 2/);
  assert.match(details, /SD classSDXC \(32 GB–2 TB\)/);
  // Size, lock switch, partition and file system facts aren't shown.
  assert.doesNotMatch(details, /Exact size|Lock switch|Write-protected|Partition|File system|Closed cleanly|Bad sectors/);
  assert.match(details, /known by its volume ID, which formatting changes/);
  assert.equal(a.window.document.querySelectorAll('#card-details .health-note.warn').length, 0);
  // The card list names it by its class too.
  assert.match(a.$('#cards .item .item-sub').textContent, /^SDXC · 256 GB · exFAT/);
});

test('card details warn about an unclean file system, a locked card and a slow port', async t => {
  const a = await app(t, true, {
    details: { ...sdDetails, write_protected: true,
      reader: { ...sdDetails.reader, link: 'USB 2.0 (480 Mbps)' },
      file_system: { ...sdDetails.file_system, dirty: true, media_failure: true } }
  });
  const details = a.$('#card-details').textContent;
  assert.match(details, /wasn’t closed cleanly/);
  assert.match(details, /sectors it couldn’t read or write/);
  assert.match(details, /The lock switch is on/);
  assert.match(details, /can run at USB 5 Gbps but is connected at USB 2\.0 \(480 Mbps\)/);
});

test('a Z 9 counts its electronic shutter, and the desk says so', async t => {
  const a = await app(t, true, {
    drive: { bus: 'NVMe', product: 'CEB-G256T', card_serial: '602123456789' },
    health: z9Health, cardKey: 'sn:602123456789',
    camera: { model: 'NIKON Z 9', serial: '3005326', shutter_count: 48213 },
    cameras: { shared: { path: '\\\\NAS\\photos\\.card-intake\\shutter-counts.json', synced: true, error: null, folders: 1 },
      cameras: [
        // ShutterCount from a Z 9 NEF: the electronic count, with MechanicalShutterCount 0.
        { model: 'NIKON Z 9', name: 'Z 9', serial: '3005326', firmware: '05.30', rated_shutter: null, rating_source: null,
          has_mechanical_shutter: false, shutter_reference: null, count: 48213, electronic_count: null,
          last_shot: '2026-11-07T09:15:22', days: [day('2026-11-07', 48100, 48213, '08:40:00', '09:15:22')] }
      ] }
  });
  await settle();
  const [item] = a.window.document.querySelectorAll('#cameras .item');
  assert.match(item.textContent, /Electronic shutter count48,213/);
  assert.match(item.textContent, /NIKON Z 9/);
  assert.match(item.textContent, /Firmware 05\.30/);
  // Nothing claims a mechanical count or a mechanical life for a body with neither.
  assert.doesNotMatch(item.textContent, /Mechanical|rated life/);
});

test('a card whose shutter count could not be read explains it beside other cameras', async t => {
  const a = await app(t, true, {
    camera: { model: 'ILCE-1', serial: '5021734', shutter_count: null,
      shutter_note: 'Needs ExifTool for Windows. Put exiftool.exe on PATH or set CARD_INTAKE_EXIFTOOL, then restart the app.' },
    cameras: { shared: { path: '\\\\NAS\\photos\\.card-intake\\shutter-counts.json', synced: true, error: null, folders: 1 },
      cameras: [
        { model: 'ILCE-7RM6', name: 'α7R VI', serial: '03506694', firmware: '1.01', rated_shutter: null, rating_source: null, shutter_reference: 500000, count: 894, electronic_count: null,
          last_shot: '2026-11-06T20:51:07', days: [day('2026-11-06', 699, 894, '17:21:17', '20:51:07')] }
      ] }
  });
  await settle();
  assert.match(a.$('#cameras').textContent, /ILCE-7RM6/);
  assert.match(a.$('#cameras').textContent, /Needs ExifTool for Windows/);
});

test('a Nikon Zf shows its mechanical count against Nikon’s rating, and its electronic count beside it', async t => {
  const a = await app(t, true, { cameras: {
    shared: { path: '\\\\NAS\\photos\\.card-intake\\shutter.sqlite', synced: true, error: null, folders: 3 },
    cameras: [
      // From the shared history after re-reading the Zf's folders.
      { model: 'NIKON Z f', name: 'Zf', serial: '8043077', firmware: '03.01', rated_shutter: 200000,
        rating_source: 'Nikon: mechanical shutter tested to 200,000 cycles (Z f product page, Nikon USA)',
        has_mechanical_shutter: true, shutter_reference: null, count: 28948, electronic_count: 34004,
        last_shot: '2026-10-29T18:40:00', days: [day('2026-10-01', 25829, 27310, '09:00:00', '18:00:00'), day('2026-10-29', 28900, 28948, '17:00:00', '18:40:00')] }
    ] } });
  await settle();
  // The inserted card's α7R VI isn't in My Cameras here, so it's listed first as new.
  const [zf] = a.window.document.querySelectorAll('#cameras .item:not(.new)');
  assert.match(zf.querySelector('.item-name').textContent, /NIKON Z fZf/);
  assert.match(zf.textContent, /Shutter count28,948/);
  assert.match(zf.querySelector('.electronic').textContent, /Electronic34,004/);
  assert.match(zf.textContent, /14\.5% of 200,000 rated life/);
  assert.match(zf.querySelector('.life-note').title, /^Nikon: mechanical shutter tested to 200,000 cycles/);
});

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

const a7r6 = (overrides = {}) => ({ model: 'ILCE-7RM6', name: 'α7R VI', serial: '03506694', internal_serial: '73 255 0 0 25 9', firmware: '1.01',
  rated_shutter: null, rating_source: null, has_mechanical_shutter: true, shutter_reference: 500000, count: 894, electronic_count: null,
  last_shot: '2026-11-06T20:51:07', days: [day('2026-11-06', 699, 894, '17:21:17', '20:51:07')], ...overrides });
const shared = { path: '\\\\NAS\\photos\\.card-intake\\shutter.sqlite', synced: true, error: null, folders: 1 };

test('a camera not in My Cameras shows as New Camera, and registering it keeps what was read', async t => {
  // The store's demo α7R VI: no serial number in its photos, only Sony's internal one.
  const demo = a7r6({ name: 'α7R VI', serial: null, internal_serial: '63 255 0 0 42 9', count: 3561,
    last_shot: '2026-06-10T23:18:09', days: [day('2026-06-10', 3480, 3561, '23:04:19', '23:18:09')] });
  const a = await app(t, true, {
    camera: { model: 'ILCE-7RM6', serial: null, internal_serial: '63 255 0 0 42 9', shutter_count: 3561 },
    cameras: { shared, cameras: [a7r6()], new_cameras: [demo] },
    camerasAfterRegister: { shared, cameras: [a7r6(), demo], new_cameras: [] }
  });
  await settle();
  assert.equal(a.$('#cams-h').textContent, 'My Cameras');
  assert.equal(a.$('#cards-h').textContent, 'My Cards');
  // On the desk, under the camera.
  assert.match(a.$('#camera-tag').textContent, /New Camera/);
  assert.equal(a.$('#desk-register-camera').textContent, 'Register to My Cameras');
  // In the list: first, with its serial line and the button below it.
  const [fresh, mine] = a.window.document.querySelectorAll('#cameras .item');
  assert.ok(fresh.classList.contains('new'));
  assert.match(fresh.querySelector('.chip.new').textContent, /New Camera/);
  assert.equal(fresh.querySelector('.item-id').textContent, 'No SN in its photos');
  assert.equal(fresh.querySelector('.item-id').nextElementSibling.querySelector('button').textContent, 'Register to My Cameras');
  assert.match(fresh.textContent, /Shutter count3,561/);
  assert.match(fresh.textContent, /not saved until it’s registered/);
  assert.ok(!mine.classList.contains('new'));
  assert.equal(fresh.querySelector('[data-remove-camera]'), null);
  assert.equal(mine.querySelector('[data-remove-camera]').textContent, 'Remove');
  assert.match(mine.textContent, /SN 03506694/);

  fresh.querySelector('[data-register-camera]').click();
  await settle();
  assert.deepEqual(a.last('register_camera'), { model: 'ILCE-7RM6', serial: null, internalSerial: '63 255 0 0 42 9' });
  assert.equal(a.window.document.querySelectorAll('#cameras .item.new').length, 0);
  assert.equal(a.window.document.querySelectorAll('#cameras .item').length, 2);
  assert.doesNotMatch(a.$('#camera-tag').textContent, /New Camera/);
  assert.equal(a.$('#desk-register-camera'), null);
});

test('a registered camera without a count yet says so instead of showing zero', async t => {
  const pocket = { model: 'PP-101', name: 'Osmo Pocket 3', serial: '5WCQNBL6BB1A22', internal_serial: null, firmware: null,
    rated_shutter: null, rating_source: null, has_mechanical_shutter: false, shutter_reference: null, count: null, electronic_count: null,
    last_shot: null, days: [] };
  const a = await app(t, true, {
    camera: { model: 'PP-101', serial: '5WCQNBL6BB1A22', shutter_count: null,
      shutter_note: 'The Osmo Pocket 3 has no mechanical shutter: its count is the electronic shutter’s, and ExifTool found none in this card’s photos.' },
    cameras: { shared, cameras: [pocket], new_cameras: [] }
  });
  await settle();
  const [item] = a.window.document.querySelectorAll('#cameras .item');
  assert.ok(!item.classList.contains('new'));
  assert.match(item.textContent, /ExifTool found none in this card’s photos/);
  assert.doesNotMatch(item.textContent, /Shutter count0|NaN/);
  assert.doesNotMatch(a.$('#camera-tag').textContent, /New Camera/);
});

test('a card not in My Cards shows as New Card; registering it lists it with the others', async t => {
  const card = (id, overrides) => ({ id, volume_id: id, name: null, model: null, bus: 'USB', sd_class: 'SDXC', formats: 0, capacity_bytes: 256e9,
    filesystem: 'exFAT', first_seen: '2026-11-07T05:33:46', last_seen: '2026-11-07T05:34:27', insertions: 1, hardware_serial: null, health: [], ...overrides });
  const a = await app(t, true, {
    cameras: { shared, cameras: [a7r6()], new_cameras: [] },
    ledger: { cards: [card('CARD-ID', { registered: false }), card('0B35-ECD9', { last_seen: '2026-11-07T04:52:00' })] }
  });
  await settle();
  const [fresh, mine] = a.window.document.querySelectorAll('#cards .item');
  assert.match(fresh.querySelector('.chips').textContent, /New Card/);
  assert.match(fresh.querySelector('.chips').textContent, /In reader/);
  assert.equal(fresh.querySelector('.item-head').nextElementSibling.querySelector('button').textContent, 'Register to My Cards');
  assert.doesNotMatch(mine.textContent, /New Card/);
  assert.equal(fresh.querySelector('[data-remove-card]'), null);
  assert.equal(mine.querySelector('[data-remove-card]').textContent, 'Remove');
  assert.match(a.$('#card-meta').textContent, /New Card/);
  // The inserted camera is registered, so the desk offers only the card.
  assert.equal(a.$('#desk-register-camera'), null);

  a.$('#desk-register-card').click();
  await settle();
  assert.deepEqual(a.last('register_card'), { cardKey: 'CARD-ID' });
  assert.equal(a.window.document.querySelectorAll('#cards .item.new').length, 0);
  assert.equal(a.window.document.querySelectorAll('#cards .item').length, 2);
  assert.doesNotMatch(a.$('#card-meta').textContent, /New Card/);
  assert.match(a.$('#card-meta').textContent, /^First seen .* · not imported by this app yet$/);
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

test('removing a camera needs two clicks and makes an inserted body new with no old history', async t => {
  const a = await app(t, true, {
    cameras: { shared, cameras: [a7r6()], new_cameras: [] },
    camerasAfterRegister: { shared, cameras: [a7r6({ count: null, days: [], last_shot: null })], new_cameras: [] }
  });
  a.window.confirm = () => { throw new Error('Native confirmation must not be used'); };
  const b = a.$('[data-remove-camera]');
  assert.equal(b.textContent, 'Remove');
  b.click();
  await settle();
  assert.equal(b.textContent, 'Remove? Click again');
  assert.equal(a.calls.filter(c => c.command === 'remove_camera').length, 0);
  b.click(); b.click();
  await settle();
  assert.equal(a.calls.filter(c => c.command === 'remove_camera').length, 1);
  assert.deepEqual(a.last('remove_camera'), { model: 'ILCE-7RM6', serial: '03506694', internalSerial: '73 255 0 0 25 9' });
  assert.equal(a.$('[data-remove-camera]'), null);
  assert.match(a.$('#camera-tag').textContent, /New Camera/);
  const fresh = a.$('#cameras .item.new');
  assert.ok(fresh.querySelector('[data-register-camera]'));
  assert.equal(fresh.querySelector('svg'), null);
  assert.doesNotMatch(fresh.textContent, /894/);
  fresh.querySelector('[data-register-camera]').click();
  await settle();
  assert.ok(a.$('[data-remove-camera]'));
  assert.equal(a.$('#cameras .item.new'), null);
});

test('removing a card preserves import history and sees a card still in the reader as new', async t => {
  const card = { id: 'CARD-ID', name: 'My SD', last_seen: '2026-11-07T12:00:00', first_seen: '2026-10-01T12:00:00', insertions: 9, registered: true };
  const imported = result();
  const a = await app(t, true, { ledger: { cards: [card], imports: [imported] } });
  const b = a.$('[data-remove-card]');
  b.click();
  await settle();
  assert.equal(b.textContent, 'Remove? Click again');
  assert.equal(a.calls.filter(c => c.command === 'remove_card').length, 0);
  b.click();
  await settle();
  assert.deepEqual(a.last('remove_card'), { cardKey: 'CARD-ID' });
  assert.equal(a.$('[data-remove-card]'), null);
  assert.match(a.$('#cards .item.new').textContent, /New Card/);
  assert.match(a.$('#card-meta').textContent, /New Card/);
  assert.equal(a.fixture.ledger.cards[0].insertions, 1);
  assert.deepEqual(a.fixture.ledger.imports, [imported]);
  assert.match(a.$('#history').textContent, /Tokyo/);
  a.$('[data-register-card]').click();
  await settle();
  assert.ok(a.$('[data-remove-card]'));
});

test('removal confirmation expires and errors allow a confirmed retry', async t => {
  const a = await app(t, true, {
    cameras: { shared, cameras: [a7r6()], new_cameras: [] },
    ledger: { cards: [{ id: 'OTHER', name: 'Other card', last_seen: '2026-11-07T12:00:00' }] },
    removeCameraError: 'Could not save the camera removal', removeCardError: 'Could not save the card removal'
  });
  const camera = a.$('[data-remove-camera]'), card = a.$('[data-remove-card]');
  camera.click();
  a.removalTimeouts.pop()();
  assert.equal(camera.textContent, 'Remove');
  camera.click();
  assert.equal(a.calls.filter(c => c.command === 'remove_camera').length, 0);
  camera.click();
  await settle();
  assert.equal(camera.disabled, false);
  assert.equal(camera.textContent, 'Remove');
  assert.match(a.$('#toast').textContent, /Could not save the camera removal/);
  card.click(); card.click();
  await settle();
  assert.equal(card.disabled, false);
  assert.equal(card.textContent, 'Remove');
  assert.match(a.$('#toast').textContent, /Could not save the card removal/);
  assert.ok(a.$('[data-remove-camera]'));
  assert.ok(a.$('[data-remove-card]'));
});

test('a single-photo preview cannot override the checked shutter history', async t => {
  const a = await app(t, true, {
    cameras: { shared, cameras: [a7r6()], new_cameras: [] },
    camera: { model: 'ILCE-7RM6', serial: '03506694', shutter_count: 4194304 }
  });
  assert.match(a.$('#cameras .item').textContent, /Shutter count894/);
  assert.doesNotMatch(a.$('#cameras .item').textContent, /4,194,304/);
});

test('without a connected share the shutter history stays on this computer', async t => {
  const a = await app(t);
  await settle();
  assert.match(a.$('#cams-note').textContent, /This computer only/);
  assert.equal(a.$('#lib-scan').disabled, true);
});

test('Lightroom: open a shoot or a trip from the done panel; Open, Reopen and Finish from the history', async t => {
  const a = await app(t, true, {
    catalogs: [
      { import_id: '2026-10-12T10:00:00', shoots: [0], name: '2026-11-06_Tokyo', trip: false, state: 'local' },
      { import_id: '2026-10-01T10:00:00', shoots: [0, 1], name: '2026-10-01_Fall', trip: true, state: 'nas' },
      { import_id: '2026-09-20T10:00:00', shoots: [0], name: '2026-09-20_A', trip: false, state: 'open' }
    ],
    ledger: { imports: [
      result({ started_at: '2026-10-12T10:00:00', finished_at: '2026-10-12T10:05:00' }),
      result({ started_at: '2026-10-01T10:00:00', finished_at: '2026-10-01T10:05:00', shoots: ['2026\\2026-10-01_A', '2026\\2026-10-02_B'] }),
      result({ started_at: '2026-09-20T10:00:00', finished_at: '2026-09-20T10:05:00', shoots: ['2026\\2026-09-20_A'] }),
      result({ started_at: '2026-09-01T10:00:00', status: 'cancelled' })
    ] }
  });
  await settle();
  const rows = a.window.document.querySelectorAll('#history tbody tr');
  const catalog = row => row.lastElementChild.textContent;
  assert.equal(catalog(rows[0]), 'On this PCOpenFinishDeliver');
  assert.equal(catalog(rows[1]), 'Trip 2026-10-01_FallFinished · on NASReopenDeliver');
  assert.equal(catalog(rows[2]), 'Open in LightroomDeliver');
  assert.equal(catalog(rows[3]), '—');
  rows[1].querySelector('[data-hopen]').click();
  await settle();
  assert.deepEqual(a.last('open_in_lightroom'), { importId: '2026-10-01T10:00:00', shoot: 0, trip: null });
  a.window.document.querySelector('#history [data-hfinish]').click();
  await settle();
  assert.deepEqual(a.last('finish_catalog'), { importId: '2026-10-12T10:00:00', shoot: 0 });
  assert.match(a.$('#toast').textContent, /2026-11-06_Tokyo is stored on the NAS in .*_catalog\. The copy on this computer was removed/);

  // The done panel: each shoot opens its own catalog, or all of them open as a trip.
  a.$('#start').click();
  a.fixture.finish(result({ shoots: ['2026\\2026-11-06_Tokyo', '2026\\2026-11-07_Tokyo'] }));
  await settle();
  assert.match(a.$('#done').textContent, /Catalog: D:\\LrC Catalogs\\2026\\2026-11-07_Tokyo\\2026-11-07_Tokyo\.lrcat/);
  a.$('[data-open="1"]').click();
  await settle();
  assert.deepEqual(a.last('open_in_lightroom'), { importId: '2026-11-06T22:00:00', shoot: 1, trip: null });
  assert.match(a.$('#toast').textContent, /Opening 2026-11-06_Tokyo in Lightroom Classic\. The Card Intake plug-in adds new photos/);
  assert.equal(a.$('#trip-name').value, 'Tokyo');
  a.type('#trip-name', '');
  a.$('#open-trip').click();
  assert.equal(a.$('#toast').textContent, 'Name the trip first.');
  a.type('#trip-name', 'Japan');
  a.$('#open-trip').click();
  await settle();
  assert.deepEqual(a.last('open_in_lightroom'), { importId: '2026-11-06T22:00:00', shoot: null, trip: 'Japan' });
  // Once the trip exists, the done panel says so.
  a.fixture.catalogs = [{ import_id: '2026-11-06T22:00:00', shoots: [0, 1], name: '2026-11-06_Japan', trip: true, state: 'open' }];
  a.fixture.launch = { catalog: 'x', name: '2026-11-06_Japan', already_open: true };
  a.$('[data-open="0"]').click();
  await settle();
  assert.match(a.$('#toast').textContent, /^2026-11-06_Japan is already open in Lightroom\. To add new files, choose Library → Plug-in Extras → Add new files from NAS\.$/);
  assert.match(a.$('#done').textContent, /Trip catalog: 2026-11-06_Japan/);
  assert.equal(a.$('#trip-name').value, 'Japan');
});

test('Open in Lightroom can be pressed during the import and opens when the import is verified', async t => {
  const a = await app(t);
  a.type('#name-0', 'Tokyo');
  a.$('#start').click();
  assert.equal(a.$('#arm-what'), null);
  a.$('#arm').click();
  assert.equal(a.$('#lr-arm').textContent, 'Lightroom opens 2026-11-06_Tokyo when the import is verified.Don’t open');
  assert.equal(a.calls.filter(c => c.command === 'open_in_lightroom').length, 0);
  a.fixture.finish(result());
  await settle();
  assert.deepEqual(a.last('open_in_lightroom'), { importId: '2026-11-06T22:00:00', shoot: 0, trip: null });

  // Several shoots: open the first, or all of them as a trip. A stopped import opens nothing.
  const b = await app(t, true, { groups: [cardDay('2026-11-06'), cardDay('2026-11-07')] });
  b.$('#start').click();
  assert.equal(b.$('#arm-what').textContent, '2026-11-06All as a trip');
  b.$('#arm-what').value = 'trip';
  b.$('#arm-what').dispatchEvent(new b.window.Event('change'));
  assert.equal(b.$('#arm-trip').hidden, false);
  b.$('#arm').click();
  assert.equal(b.$('#toast').textContent, 'Name the trip first.');
  b.type('#arm-trip', 'Japan');
  b.$('#arm').click();
  assert.match(b.$('#lr-arm').textContent, /^Lightroom opens the trip Japan when the import is verified\./);
  b.fixture.finish(result({ status: 'cancelled', shoots: [] }));
  await settle();
  assert.equal(b.calls.filter(c => c.command === 'open_in_lightroom').length, 0);
  assert.equal(b.$('#toast').textContent, "Lightroom wasn't opened: the import didn't finish.");
  b.$('#start').click();
  assert.equal(b.$('#arm').textContent, 'Open in Lightroom when done');
  b.$('#arm-what').value = 'trip';
  b.$('#arm-what').dispatchEvent(new b.window.Event('change'));
  b.type('#arm-trip', 'Japan');
  b.$('#arm').click();
  b.fixture.finish(result({ shoots: ['2026\\2026-11-06', '2026\\2026-11-07'] }));
  await settle();
  assert.deepEqual(b.last('open_in_lightroom'), { importId: '2026-11-06T22:00:00', shoot: null, trip: 'Japan' });
});

test('Lightroom setup installs the plug-in and a presets folder for every catalog', async t => {
  const a = await app(t);
  assert.match(a.$('#lr-plugin').textContent, /^The Card Intake plug-in is installed in Lightroom\. Restart Lightroom once/);
  assert.equal(a.$('#lr-note').textContent, 'Ready');
  a.$('#lr-presets').value = 'C:\\Profiles';
  a.$('#lr-save').click();
  await settle();
  assert.deepEqual(a.last('save_lightroom_settings').settings, {
    executable: 'C:\\Adobe\\Lightroom.exe', template: 'C:\\Empty.lrcat', catalog_root: 'D:\\LrC Catalogs', presets: 'C:\\Profiles'
  });
  assert.match(a.$('#lr-plugin').textContent, /98 presets and profiles are installed for every catalog\.$/);
  assert.equal(a.$('#lr-status').textContent, 'Saved.');
});
