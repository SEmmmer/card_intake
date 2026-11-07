// The demo (prototype/card-intake.html) is the app's own page with a demo backend, so
// it must be rebuilt whenever the page changes, and must work without the app.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const { JSDOM, VirtualConsole } = require('jsdom');
const { build, OUT } = require('../../prototype/build-demo.cjs');

test('the demo is up to date with the app page: run node prototype/build-demo.cjs after changing either', () => {
  assert.equal(fs.readFileSync(OUT, 'utf8').replace(/\r\n/g, '\n'), build());
});

async function demo(t) {
  const errors = [];
  const virtualConsole = new VirtualConsole();
  virtualConsole.on('error', e => errors.push(String(e)));
  virtualConsole.on('jsdomError', e => errors.push(String(e.message || e)));
  const dom = new JSDOM(build(), {
    runScripts: 'dangerously', url: 'http://localhost/', virtualConsole,
    beforeParse(window) { window.__DEMO_SPEED__ = 400; }
  });
  t.after(() => dom.window.close());
  const { window } = dom;
  const $ = selector => window.document.querySelector(selector);
  const until = async (check, what) => {
    for (let i = 0; i < 1000; i++) {
      const value = check();
      if (value) return value;
      await new Promise(resolve => window.setTimeout(resolve, 2));
    }
    throw new Error('Timed out waiting for ' + what);
  };
  return { window, $, until, errors };
}

test('the demo imports a card, opens Lightroom and finishes a catalog', async t => {
  const { window, $, until, errors } = await demo(t);
  assert.match($('#demo-chip').textContent, /Demo/);
  await until(() => $('#start') && !$('#start').disabled, 'the inserted card');
  assert.equal($('#card-name').textContent, 'CEA-G240T');
  assert.match($('.card-title .chip').textContent, /CFexpress Type A · 240 GB · exFAT/);
  assert.equal($('#card-details'), null);
  assert.match($('#note-0').textContent, /Existing folder · 123 of 199 already there/);
  await until(() => /Shutter count/.test($('#cameras').textContent) && !/Reading the shutter count/.test($('#cameras').textContent), 'shutter counts');
  assert.match($('#cameras').textContent, /1,620/);

  const name = $('#name-1');
  name.value = 'Rooftop';
  name.dispatchEvent(new window.Event('input'));
  $('#start').click();
  await until(() => $('#done') && !$('#done').hidden, 'the import');
  assert.match($('#done').textContent, /Import complete/);
  assert.match($('#done').textContent, /All 608 photos and 3 clips are on the NAS and match the card/);
  assert.match($('#done').textContent, /123 files were already on the NAS/);
  assert.match($('#done').textContent, /Rated 488 raws: 1 locked in camera, 5 camera ratings, 482 computed\. Kept 120 existing ratings\./);
  assert.match($('#history tbody tr').textContent, /Harbour-night\s*\+1 more/);

  // Yesterday's folder already has a catalog on this PC; today's gets one when it opens.
  $('[data-open="1"]').click();
  await until(() => !$('#toast').hidden && /Opening .*_Rooftop in Lightroom Classic/.test($('#toast').textContent), 'Lightroom');
  $('[data-open="0"]').click();
  await until(() => /Opening .*_Harbour-night/.test($('#toast').textContent), 'the second catalog');
  // Opening Harbour-night closed Rooftop, which can now be finished.
  const finish = await until(() => [...window.document.querySelectorAll('[data-hfinish]')].find(b => b.dataset.hfinish === '0:1'), 'Finish');
  finish.click();
  await until(() => /_Rooftop is stored on the NAS/.test($('#toast').textContent), 'Finish');

  $('#again').click();
  await until(() => $('#summary') && /already on the NAS/.test($('#summary').textContent), 'the card read again');
  assert.deepEqual(errors, []);
});

test('the demo shows a new SD card and a new camera, and registers and removes them', async t => {
  const { window, $, until, errors } = await demo(t);
  await until(() => $('#start'), 'the inserted card');
  assert.match($('#cards .item.new').textContent, /New Card/);
  [...window.document.querySelectorAll('.picker button')].find(b => b.textContent === 'F:').click();
  await until(() => $('#card-name') && $('#card-name').textContent === 'Untitled', 'the SD card');
  assert.equal($('#health-read').hidden, true);
  const details = $('#card-details').textContent;
  assert.match(details, /ReaderUSB Storage \(05E3:0764\)/);
  assert.doesNotMatch(details, /Exact size|Partition|File system|Closed cleanly|Bad sectors|Write-protected|Lock switch/);

  (await until(() => $('#desk-register-camera'), 'Register to My Cameras')).click();
  await until(() => !$('#desk-register-camera') && /PP-101/.test($('#cameras').textContent), 'the registered camera');
  $('#desk-register-card').click();
  await until(() => !$('#cards .item.new'), 'the registered card');

  const remove = [...window.document.querySelectorAll('[data-remove-camera]')].pop();
  remove.click();
  assert.equal(remove.textContent, 'Remove? Click again');
  assert.ok(remove.classList.contains('armed'));
  remove.click();
  await until(() => $('#desk-register-camera'), 'the camera to be new again');
  assert.deepEqual(errors, []);
});

test('the demo stops an import, then resumes it and skips what was copied', async t => {
  const { window, $, until, errors } = await demo(t);
  await until(() => $('#start') && !$('#start').disabled, 'the inserted card');
  $('#start').click();
  await until(() => $('#stop') && /Copying/.test($('#phase').textContent), 'copying');
  $('#stop').click();
  await until(() => $('#start') && $('#start').textContent === 'Resume import', 'the stop');
  assert.match($('#progress').textContent, /Stopped after \d+ of 611 files/);
  $('#start').click();
  await until(() => $('#done') && !$('#done').hidden, 'the resumed import');
  assert.match($('#done').textContent, /All 608 photos and 3 clips are on the NAS/);
  assert.match($('#history').textContent, /Stopped/);
  assert.deepEqual(errors, []);
});
