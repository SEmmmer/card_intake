// Runs the real Lightroom plug-in in Fengari (Lua in JavaScript) against a mocked
// Lightroom SDK. The mock commits photos only when a write gate closes, as Lightroom
// does, so stacking a DNG on a raw added in the same gate would fail here too.
// A virtual Unix clock advances at each sleep; hooks model uploads and menu tasks.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { lua, lauxlib, lualib, to_luastring } = require('fengari');
const { JSDOM } = require('jsdom');

const dir = path.join(__dirname, '../../lrplugin/CardIntake.lrplugin');
const source = (name) => fs.readFileSync(path.join(dir, name), 'utf8');

const harness = String.raw`
folder = '/NAS/2026/2026-11-06_東京'
listing = '/Catalogs/2026/2026-11-06_東京/2026-11-06_東京.card-intake.txt'
heartbeat = listing:gsub('%.txt$', '.uploading')
files = { [listing] = '# Card Intake: shoot folders\n' .. folder .. '\n' }
listed = {}
inCatalog, pending, pendingOrder, added = {}, {}, {}, {}
gates, dialogs, bezels, smart, keywords = {}, {}, {}, {}, {}
logs, sleeps, progresses, gateAttempts, previewBatches = {}, {}, {}, {}, {}
now, progressDone = 1000, 0
prefs = { developPreset = 'DEV', dngDevelopPreset = '', metadataPreset = 'META' }
local inGate = false

function photo(path, meta)
  local p = { path = path, meta = meta or { pickStatus = 0, colorNameForLabel = 'none' }, keywords = {} }
  function p:getRawMetadata(key) if key == 'path' then return self.path end return self.meta[key] end
  function p:setRawMetadata(key, value) assert(inGate, 'metadata set outside a write gate'); self.meta[key] = value end
  function p:addKeyword(k) assert(inGate, 'keyword added outside a write gate'); table.insert(self.keywords, k.name) end
  return p
end

function file(name) return folder .. '/' .. name end
function have(name, meta) inCatalog[file(name)] = photo(file(name), meta); return inCatalog[file(name)] end
function addedPaths()
  local names = {}
  for _, p in ipairs(added) do table.insert(names, (p.path:gsub('^.*/', ''))) end
  return table.concat(names, ',')
end

catalog = {
  getPath = function() return '/Catalogs/2026/2026-11-06_東京/2026-11-06_東京.lrcat' end,
  findPhotoByPath = function(self, p) return inCatalog[p] end,
  addPhoto = function(self, p, stack, position, meta, dev)
    assert(inGate, 'addPhoto outside a write gate')
    if p == badPhoto then error('Unsupported file', 0) end
    if stack then assert(inCatalog[stack.path] == stack, 'stacked on a photo not in the catalog yet') end
    local ph = photo(p, importMetadata and importMetadata[p])
    ph.stack, ph.position, ph.metaPreset, ph.devPreset = stack, position, meta, dev
    pending[p] = ph
    table.insert(pendingOrder, ph)
    return ph
  end,
  withWriteAccessDo = function(self, name, fn, opts)
    assert(opts and opts.timeout, 'no timeout')
    table.insert(gateAttempts, name)
    if busy or busyGate == name then return 'aborted' end
    if errorGate == name then error('Lightroom write access failed', 0) end
    table.insert(gates, name)
    local previous = {}
    for p, ph in pairs(inCatalog) do
      previous[p] = {}
      for k, value in pairs(ph.meta) do previous[p][k] = value end
    end
    inGate = true
    local ok, problem = pcall(fn)
    inGate = false
    if rollbackGate == name then ok, problem = false, 'Write transaction aborted' end
    if not ok then
      for p, meta in pairs(previous) do inCatalog[p].meta = meta end
      pending, pendingOrder = {}, {}; error(problem, 0)
    end
    for _, ph in ipairs(pendingOrder) do inCatalog[ph.path] = ph; table.insert(added, ph) end
    pending, pendingOrder = {}, {}
    return 'executed'
  end,
  -- The plug-in makes no collections or keywords; these record any attempt.
  createSmartCollection = function(self, name) smart[name] = true end,
  createCollection = function(self, name) smart[name] = true end,
  createKeyword = function(self, name) table.insert(keywords, name); return { name = name } end,
  buildSmartPreviews = function(self, photos)
    assert(not inGate); previews = photos; table.insert(previewBatches, photos)
  end,
}

local modules = {
  LrXml = {
    parseXml = function(text)
      local function wrap(node)
        function node:name() return self.localName, self.namespace end
        function node:text() return self.content end
        function node:attributes() return self.attrs end
        function node:childCount() return #self.children end
        function node:childAtIndex(i) return self.children[i] end
        for _, child in ipairs(node.children) do wrap(child) end
        return node
      end
      return wrap(parseXml(text))
    end,
  },
  LrApplication = { activeCatalog = function() if onActive then return onActive() end return catalog end },
  LrDate = { currentTime = function() return now end, timeToPosixDate = function(t) return t end },
  LrLogger = function(name)
    assert(name == 'CardIntake')
    local function log(_, text) table.insert(logs, text) end
    return { enable = function(_, target) assert(target == 'logfile') end, info = log, warn = log, error = log }
  end,
  LrDialogs = {
    message = function(title, body) table.insert(dialogs, title .. ' | ' .. (body or '')) end,
    showBezel = function(text) table.insert(bezels, text) end,
  },
  LrFileUtils = {
    exists = function(p)
      if p == folder then return not missing and 'directory' or false end
      if directories and directories[p] then return 'directory' end
      return files[p] and 'file' or false
    end,
    readFile = function(p)
      if readError == p then error('File is temporarily unreadable', 0) end
      if unreadable == p then return nil end
      return files[p]
    end,
    files = function(d)
      local names = folderFiles and folderFiles[d] or (d == folder and listed)
      assert(names, d)
      local i = 0
      return function() i = i + 1; return names[i] and (d .. '/' .. names[i]) end
    end,
  },
  LrPathUtils = {
    removeExtension = function(p) return (p:gsub('%.[^%./]*$', '')) end,
    extension = function(p) return p:match('%.([^%./]+)$') or '' end,
    leafName = function(p) return (p:match('[^/]*$')) end,
    child = function(d, n) return d .. '/' .. n end,
  },
  LrPrefs = { prefsForPlugin = function() return prefs end },
  LrProgressScope = function(options)
    table.insert(progresses, options)
    return {
      setPortionComplete = function() end,
      setCaption = function(_, text) caption = text end,
      done = function() progressDone = progressDone + 1 end,
      isCanceled = function() return cancel or false end,
    }
  end,
  LrTasks = {
    pcall = pcall, startAsyncTask = function(fn) fn() end,
    sleep = function(seconds)
      now = now + seconds; table.insert(sleeps, seconds)
      assert(#sleeps < 100, 'loop never stopped')
      if onSleep then onSleep(seconds) end
    end,
  },
}
function import(name) return assert(modules[name], name) end
package.preload.Intake = function() return assert(load(INTAKE, '=Intake.lua'))() end
`;

function run(config, entry, verify) {
  const L = lauxlib.luaL_newstate();
  lualib.luaL_openlibs(L);
  lua.lua_pushstring(L, to_luastring(source('Intake.lua')));
  lua.lua_setglobal(L, to_luastring('INTAKE'));
  lua.lua_pushstring(L, to_luastring(source('AddNewFiles.lua')));
  lua.lua_setglobal(L, to_luastring('MENU'));
  lua.lua_pushstring(L, to_luastring(source('PluginShutdown.lua')));
  lua.lua_setglobal(L, to_luastring('SHUTDOWN'));
  lua.lua_pushcfunction(L, state => {
    const text = lua.lua_tojsstring(state, 1);
    let doc;
    try { doc = new JSDOM(text, { contentType: 'text/xml' }); }
    catch { return lauxlib.luaL_error(state, to_luastring('Invalid XML')); }
    const push = value => {
      if (typeof value === 'string') lua.lua_pushstring(state, to_luastring(value));
      else if (value === null) lua.lua_pushnil(state);
      else {
        lua.lua_newtable(state);
        for (const [key, item] of Object.entries(value)) {
          push(item);
          if (Array.isArray(value)) lua.lua_rawseti(state, -2, Number(key) + 1);
          else lua.lua_setfield(state, -2, to_luastring(key));
        }
      }
    };
    const node = n => ({
      localName: n.localName || '', namespace: n.namespaceURI || '', content: n.textContent || '',
      attrs: Array.from(n.attributes || [], a => ({ name: a.localName, namespace: a.namespaceURI || '', value: a.value })),
      children: Array.from(n.childNodes, node)
    });
    push(node(doc.window.document.documentElement));
    doc.window.close();
    return 1;
  });
  lua.lua_setglobal(L, to_luastring('parseXml'));
  const code = [harness, config, source(entry), verify].join('\n');
  const status = lauxlib.luaL_dostring(L, to_luastring(code));
  const error = status === lua.LUA_OK ? null : lua.lua_tojsstring(L, -1);
  lua.lua_close(L);
  assert.equal(error, null);
}

test('opening the catalog adds new raws in place with the presets, and nothing else', () => {
  run(String.raw`
    listed = { 'DSC0001.ARW', 'DSC0002.ARW', 'DSC0002.JPG', 'DSC0003.HIF', 'DSC0001.xmp', '._DSC0004.ARW', 'C0001.MP4' }
    have('DSC0002.ARW')
  `, 'Init.lua', String.raw`
    assert(addedPaths() == 'DSC0001.ARW', addedPaths())
    local p = added[1]
    assert(p.devPreset == 'DEV' and p.metaPreset == 'META' and p.stack == nil)
    assert(previews and #previews == 1 and previews[1] == p, 'smart previews')
    assert(next(smart) == nil, 'made a collection')
    assert(bezels[1] == 'Card Intake added 1 file', bezels[1])
    assert(#dialogs == 0, dialogs[1])
  `);
});

test('a DxO DNG is stacked on its raw and takes its rating, flag and label', () => {
  run(String.raw`
    prefs.dngDevelopPreset = 'DNGDEV'
    listed = { 'DSC0001.ARW', 'DSC0001_DxO_DeepPRIMEXD2s.dng', 'DSC0007.ARW', 'DSC0007_DxO.dng', 'DSC0009_DxO.dng' }
    raw1 = have('DSC0001.ARW', { rating = 3, pickStatus = 1, colorNameForLabel = 'red' })
  `, 'Init.lua', String.raw`
    assert(addedPaths() == 'DSC0007.ARW,DSC0001_DxO_DeepPRIMEXD2s.dng,DSC0007_DxO.dng,DSC0009_DxO.dng', addedPaths())
    local dng1, dng7, dng9 = added[2], added[3], added[4]
    assert(dng1.stack == raw1 and dng1.position == 'above' and dng1.devPreset == 'DNGDEV')
    assert(dng1.meta.rating == 3 and dng1.meta.pickStatus == 1 and dng1.meta.colorNameForLabel == 'red')
    -- No keyword or collection: the stack is all there is.
    assert(#raw1.keywords == 0 and #keywords == 0 and next(smart) == nil)
    -- A raw added in this run is in the catalog by the time its DNG is stacked on it.
    assert(dng7.stack == added[1] and #added[1].keywords == 0)
    assert(dng7.meta.colorNameForLabel == 'none')
    -- A DNG without its raw is added on its own.
    assert(dng9.stack == nil)
    assert(#gates == 3 and #previews == 4)
  `);
});

test('only newly added photos with Card Intake Pick requests become picks, preserving user flags', () => {
  run(String.raw`
    listed = { 'locked.ARW', 'locked.XMP', 'rejected.ARW', 'rejected.xmp', 'picked.NEF', 'picked.xmp',
      'plain.ARW', 'plain.xmp', 'other.ARW', 'other.xmp', 'bad.ARW', 'bad.xmp', 'old.ARW', 'old.xmp',
      'locked_DxO.dng' }
    local ns = 'https://github.com/semmmer/card_intake/ns/1.0/'
    local request = '<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:request="' .. ns .. '" request:Pick="True"/></rdf:RDF></x:xmpmeta>'
    files[file('locked.XMP')] = request
    files[file('rejected.xmp')] = request
    files[file('picked.xmp')] = '<r xmlns:c="' .. ns .. '"><c:Pick>True</c:Pick></r>'
    files[file('plain.xmp')] = '<r xmlns:c="' .. ns .. '" c:Pick="False"/>'
    files[file('other.xmp')] = '<r xmlns:c="urn:unrelated" c:Pick="True"/>'
    files[file('bad.xmp')] = '<broken'
    files[file('old.xmp')] = request
    old = have('old.ARW', { pickStatus = -1 })
    importMetadata = {
      [file('rejected.ARW')] = { pickStatus = -1 },
      [file('picked.NEF')] = { pickStatus = 1 },
    }
  `, 'Init.lua', String.raw`
    assert(inCatalog[file('locked.ARW')].meta.pickStatus == 1)
    assert(inCatalog[file('rejected.ARW')].meta.pickStatus == -1)
    assert(inCatalog[file('picked.NEF')].meta.pickStatus == 1)
    for _, name in ipairs({ 'plain.ARW', 'other.ARW', 'bad.ARW' }) do
      assert(inCatalog[file(name)].meta.pickStatus == 0, name)
    end
    assert(old.meta.pickStatus == -1)
    assert(inCatalog[file('locked_DxO.dng')].meta.pickStatus == 1)
    assert(#dialogs == 0, dialogs[1])
  `);
});

test('a disconnected share or a busy Lightroom adds nothing and says why', () => {
  for (const config of ['missing = true', 'busy = true']) {
    run(`listed = { 'DSC0001.ARW' }; ${config}`, 'Init.lua', String.raw`
      assert(#added == 0 and inCatalog[file('DSC0001.ARW')] == nil)
      assert(#dialogs == 1 and dialogs[1]:match("^Card Intake couldn't add the new files"), dialogs[1])
    `);
  }
});

test('a file Lightroom refuses is reported and the others are added', () => {
  run(String.raw`listed = { 'DSC0001.ARW', 'DSC0002.ARW' }; badPhoto = file('DSC0001.ARW')`, 'Init.lua', String.raw`
    assert(addedPaths() == 'DSC0002.ARW')
    assert(dialogs[1]:match('^Card Intake added 1 file, 1 failed | DSC0001.ARW: Unsupported file'), dialogs[1])
  `);
});

test('other catalogs are left alone, and the menu command explains', () => {
  const config = `files[listing] = nil; listed = { 'DSC0001.ARW' }`;
  run(config, 'Init.lua', `assert(#added == 0 and #dialogs == 0 and #gates == 0)`);
  run(config, 'AddNewFiles.lua', String.raw`
    assert(#added == 0 and dialogs[1]:match("^This isn't a Card Intake catalog"), dialogs[1])
  `);
});

test('the menu command adds what arrived later and says when nothing is new', () => {
  run(String.raw`listed = { 'DSC0001.ARW' }; have('DSC0001.ARW')`, 'AddNewFiles.lua', String.raw`
    assert(#added == 0 and #gates == 0 and dialogs[1]:match('^Nothing new'), dialogs[1])
  `);
  run(String.raw`listed = { 'DSC0001.ARW' }; have('DSC0001.ARW')`, 'Init.lua', `assert(#dialogs == 0 and #bezels == 0)`);
  run(String.raw`listed = { 'DSC0001.ARW', 'DSC0002.ARW' }; have('DSC0001.ARW')`, 'AddNewFiles.lua', String.raw`
    assert(addedPaths() == 'DSC0002.ARW' and bezels[1] == 'Card Intake added 1 file')
  `);
});

test('fill-in imports successive arrivals once and ends with one scope and summary', () => {
  run(String.raw`
    files[heartbeat] = tostring(now) .. '\nignored app details'
    listed = { 'first.ARW' }
    onSleep = function()
      if #sleeps == 1 then table.insert(listed, 'second.ARW') end
      if #sleeps == 2 then table.insert(listed, 'third.ARW') end
      if #sleeps == 3 then files[heartbeat] = nil end
    end
  `, 'Init.lua', String.raw`
    assert(addedPaths() == 'first.ARW,second.ARW,third.ARW', addedPaths())
    assert(#progresses == 1 and progressDone == 1)
    assert(progresses[1].title:match('as they arrive'))
    assert(#bezels == 1 and bezels[1] == 'Card Intake added 3 files' and #dialogs == 0)
    assert(#previewBatches == 3)
    local log = table.concat(logs, '\n')
    assert(log:match('Upload stopped: missing heartbeat; running final pass'), log)
    assert(log:match('Loop stopped: missing heartbeat; passes=4 added=3 failed=0'), log)
  `);
});

test('a stale heartbeat ends the loop at the 45-second boundary, without deleting it', () => {
  run(String.raw`
    files[heartbeat] = tostring(now)
    listed = { 'first.ARW' }
  `, 'Init.lua', String.raw`
    assert(files[heartbeat] == '1000' and now == 1048, tostring(now))
    assert(#sleeps == 16 and #added == 1 and #bezels == 1 and #dialogs == 0)
    assert(progressDone == 1)
    assert(table.concat(logs, '\n'):match('Loop stopped: stale heartbeat %(48s old%)'))
  `);
});

test('missing shoot folders are skipped while uploading, then imported when created', () => {
  run(String.raw`
    missing = true
    files[heartbeat] = tostring(now)
    onSleep = function()
      if #sleeps == 1 then missing = false; listed = { 'late.ARW' } end
      if #sleeps == 2 then files[heartbeat] = nil end
    end
  `, 'Init.lua', String.raw`
    assert(addedPaths() == 'late.ARW' and #dialogs == 0)
    assert(#bezels == 1 and progressDone == 1)
    assert(table.concat(logs, '\n'):match('missing folders skipped=1'))
  `);
});

test('the final pass enforces missing-folder errors after an upload stops', () => {
  run(String.raw`
    missing = true; files[heartbeat] = tostring(now)
    onSleep = function() files[heartbeat] = nil end
  `, 'Init.lua', String.raw`
    assert(#added == 0 and #dialogs == 1 and dialogs[1]:match('Reconnect the photo share'), dialogs[1])
    assert(#sleeps == 1 and progressDone == 1)
    assert(table.concat(logs, '\n'):match('running final pass'))
  `);
});

test('temporary and AppleDouble files are ignored for every supported extension', () => {
  run(String.raw`
    listed = { '.card-intake-unfinished.ARW', '.card-intake-temp.dng', '.card-intake-temp_DxO.dng',
      '.card-intake-random.part', '._resource.ARW', '._resource_DxO.dng', 'complete.ARW' }
  `, 'Init.lua', String.raw`
    assert(addedPaths() == 'complete.ARW', addedPaths())
    assert(#bezels == 1 and #dialogs == 0)
    assert(table.concat(logs, '\n'):match('Scan: raws=1 new=1; DNGs=0 new=0'))
  `);
});

test('repeated photo failures appear once in the aggregate summary and log', () => {
  run(String.raw`
    listed = { 'bad.ARW', 'good.ARW' }; badPhoto = file('bad.ARW')
    files[heartbeat] = tostring(now)
    onSleep = function() if #sleeps == 3 then files[heartbeat] = nil end end
  `, 'Init.lua', String.raw`
    assert(addedPaths() == 'good.ARW' and #dialogs == 1 and #bezels == 0)
    assert(dialogs[1] == 'Card Intake added 1 file, 1 failed | bad.ARW: Unsupported file', dialogs[1])
    local count = 0
    for _, line in ipairs(logs) do if line == 'bad.ARW: Unsupported file' then count = count + 1 end end
    assert(count == 1, tostring(count))
  `);
});

test('the menu command joins an active startup loop instead of starting another', () => {
  run(String.raw`
    listed = { 'first.ARW' }; files[heartbeat] = tostring(now)
    onSleep = function()
      if #sleeps == 1 then assert(load(MENU, '=AddNewFiles.lua'))(); table.insert(listed, 'second.ARW') end
      if #sleeps == 2 then files[heartbeat] = nil end
    end
  `, 'Init.lua', String.raw`
    assert(addedPaths() == 'first.ARW,second.ARW', addedPaths())
    assert(#progresses == 1 and progressDone == 1)
    assert(#dialogs == 1 and dialogs[1]:match('^Card Intake is already adding photos'), dialogs[1])
    assert(#bezels == 1 and bezels[1] == 'Card Intake added 2 files')
  `);
});

test('busy write access is retried while uploading, then succeeds without a warning summary', () => {
  run(String.raw`
    listed = { 'first.ARW' }; busy = true; files[heartbeat] = tostring(now)
    onSleep = function()
      busy = false
      if #sleeps == 2 then files[heartbeat] = nil end
    end
  `, 'Init.lua', String.raw`
    assert(addedPaths() == 'first.ARW' and #gateAttempts == 2 and #gates == 1)
    assert(#dialogs == 0 and #bezels == 1)
    local log = table.concat(logs, '\n')
    assert(log:match('result=aborted') and log:match('result=executed'), log)
    assert(log:match('added=1 failed=0'), log)
  `);
});

test('busy later gates retain earlier imports and pending DxO metadata for the next pass', () => {
  for (const gate of ['Card Intake: add DxO DNGs', "Card Intake: give DxO DNGs their raw's rating"]) {
    run(`busyGate = ${JSON.stringify(gate)};` + String.raw`
      listed = { 'first.ARW', 'first_DxO.dng' }
      importMetadata = { [file('first.ARW')] = { rating = 4, pickStatus = 1, colorNameForLabel = 'red' } }
      files[heartbeat] = tostring(now)
      onSleep = function()
        busyGate = nil
        if #sleeps == 2 then files[heartbeat] = nil end
      end
    `, 'Init.lua', String.raw`
      assert(addedPaths() == 'first.ARW,first_DxO.dng', addedPaths())
      local dng = inCatalog[file('first_DxO.dng')]
      assert(dng.stack == inCatalog[file('first.ARW')])
      assert(dng.meta.rating == 4 and dng.meta.pickStatus == 1 and dng.meta.colorNameForLabel == 'red')
      assert(#dialogs == 0 and #bezels == 1 and bezels[1] == 'Card Intake added 2 files')
      assert(table.concat(logs, '\n'):match('added=2 failed=0'))
    `);
  }
});

test('each pass re-reads the folder list, including folders appended during an upload', () => {
  run(String.raw`
    listed = { 'first.ARW' }; files[heartbeat] = tostring(now)
    local other = '/NAS/second shoot'
    directories, folderFiles = {}, { [folder] = listed, [other] = { 'second.ARW' } }
    onSleep = function()
      if #sleeps == 1 then files[listing] = files[listing] .. other .. '\n' end
      if #sleeps == 2 then directories[other] = true end
      if #sleeps == 3 then files[heartbeat] = nil end
    end
  `, 'Init.lua', String.raw`
    assert(addedPaths() == 'first.ARW,second.ARW', addedPaths())
    assert(added[2].path == '/NAS/second shoot/second.ARW')
    assert(#dialogs == 0 and #bezels == 1)
    local log = table.concat(logs, '\n')
    assert(log:match('Pass folders=2') and log:match('missing folders skipped=1'), log)
  `);
});

test('write exceptions preserve committed counts and retry rolled-back imports and DxO metadata', () => {
  for (const gate of [
    'Card Intake: add photos', 'Card Intake: add DxO DNGs',
    "Card Intake: give DxO DNGs their raw's rating",
  ]) {
    run(`rollbackGate = ${JSON.stringify(gate)};` + String.raw`
      listed = { 'first.ARW', 'first_DxO.dng' }; files[heartbeat] = tostring(now)
      importMetadata = { [file('first.ARW')] = { rating = 4, pickStatus = 1, colorNameForLabel = 'red' } }
      onSleep = function()
        rollbackGate = nil
        if #sleeps == 2 then files[heartbeat] = nil end
      end
    `, 'Init.lua', String.raw`
      assert(addedPaths() == 'first.ARW,first_DxO.dng', addedPaths())
      local dng = inCatalog[file('first_DxO.dng')]
      assert(dng.meta.rating == 4 and dng.meta.pickStatus == 1 and dng.meta.colorNameForLabel == 'red')
      assert(#dialogs == 0 and #bezels == 1 and bezels[1] == 'Card Intake added 2 files', bezels[1])
      assert(table.concat(logs, '\n'):match('Write failed: Write transaction aborted'))
    `);
  }
  run(String.raw`
    listed = { 'first.ARW' }; errorGate = 'Card Intake: add photos'; files[heartbeat] = tostring(now)
    onSleep = function() errorGate = nil; files[heartbeat] = nil end
  `, 'Init.lua', String.raw`
    assert(addedPaths() == 'first.ARW' and #dialogs == 0 and #bezels == 1)
    assert(table.concat(logs, '\n'):match('Write failed: Lightroom write access failed'))
  `);
});

test('unreadable, missing, malformed and old heartbeats do not keep an upload running', () => {
  for (const config of [
    'files[heartbeat] = nil',
    'files[heartbeat] = tostring(now); unreadable = heartbeat',
    'files[heartbeat] = tostring(now); readError = heartbeat',
    "files[heartbeat] = 'bad first line\\n1000'",
    "files[heartbeat] = '954'",
  ]) {
    run(`listed = { 'first.ARW' }; ${config}`, 'Init.lua', String.raw`
      assert(#added == 1 and #sleeps == 0 and progressDone == 1)
      assert(#bezels == 1 and #dialogs == 0)
    `);
  }
});

test('cancelling the whole-loop progress indicator stops waiting and cleans it up', () => {
  run(String.raw`
    listed = { 'first.ARW' }; files[heartbeat] = tostring(now)
    onSleep = function()
      files[heartbeat] = tostring(now)
      if #sleeps == 2 then cancel = true; table.insert(listed, 'after-cancel.ARW') end
    end
  `, 'Init.lua', String.raw`
    assert(addedPaths() == 'first.ARW' and #sleeps == 2)
    assert(#progresses == 1 and progressDone == 1 and #bezels == 1 and #dialogs == 0)
    assert(table.concat(logs, '\n'):match('Loop stopped: canceled'))
  `);
});

test('startup retries a not-ready catalog and folder list within a bounded wait', () => {
  run(String.raw`
    listed = { 'late.ARW' }; files[listing] = nil
    onActive = function()
      if #sleeps < 1 then return nil end
      if #sleeps < 2 then error('Catalog not ready yet', 0) end
      return catalog
    end
    onSleep = function()
      if #sleeps == 3 then files[listing] = folder .. '\n' end
    end
  `, 'Init.lua', String.raw`
    assert(addedPaths() == 'late.ARW' and #sleeps == 3)
    assert(#bezels == 1 and #dialogs == 0 and progressDone == 1)
    local log = table.concat(logs, '\n')
    assert(log:match('Init.lua started') and log:match('Catalog not ready yet') and log:match('Folder list found'), log)
  `);
});

test('unloading stops a worker despite a fresh heartbeat and suppresses shutdown UI', () => {
  run(String.raw`
    listed = { 'first.ARW' }; files[heartbeat] = tostring(now)
    onSleep = function()
      files[heartbeat] = tostring(now)
      assert(load(SHUTDOWN, '=PluginShutdown.lua'))()
    end
  `, 'Init.lua', String.raw`
    assert(addedPaths() == 'first.ARW' and #sleeps == 1)
    assert(progressDone == 1 and #dialogs == 0 and #bezels == 0)
    assert(table.concat(logs, '\n'):match('Loop stopped: plug%-in unloaded'))
  `);
});

test('readiness errors are logged, shown once after the bound, and release the loop guard', () => {
  run(String.raw`
    listed = { 'first.ARW' }; readError = listing
  `, 'Init.lua', String.raw`
    assert(#sleeps == 15 and now == 1030)
    assert(#dialogs == 1 and dialogs[1]:match('File is temporarily unreadable'), dialogs[1])
    assert(#progresses == 0 and #added == 0)
    readError = nil
    assert(load(MENU, '=AddNewFiles.lua'))()
    assert(addedPaths() == 'first.ARW' and #bezels == 1 and #dialogs == 1)
    assert(progressDone == 1)
  `);
});

test('a transient folder-list read error recovers during an upload and is not a final failure', () => {
  run(String.raw`
    listed = { 'first.ARW' }; files[heartbeat] = tostring(now)
    onSleep = function()
      if #sleeps == 1 then readError = listing end
      if #sleeps == 2 then readError = nil; table.insert(listed, 'second.ARW') end
      if #sleeps == 3 then files[heartbeat] = nil end
    end
  `, 'Init.lua', String.raw`
    assert(addedPaths() == 'first.ARW,second.ARW', addedPaths())
    assert(#dialogs == 0 and #bezels == 1 and bezels[1] == 'Card Intake added 2 files')
    local log = table.concat(logs, '\n')
    assert(log:match('File is temporarily unreadable') and log:match('added=2 failed=0'), log)
  `);
});

test('startup exceptions before the worker loads are logged and shown once', () => {
  run(String.raw`
    package.preload.Intake = function() error('Worker could not be loaded', 0) end
  `, 'Init.lua', String.raw`
    assert(#dialogs == 1 and dialogs[1]:match('Worker could not be loaded'), dialogs[1])
    local log = table.concat(logs, '\n')
    assert(log:match('Init.lua started') and log:match('Init task failed: Worker could not be loaded'), log)
  `);
});

test('an idle plain catalog makes no changes, progress indicator, or notifications', () => {
  run(String.raw`
    files[listing] = nil; listed = { 'unrelated.ARW' }
    files[heartbeat] = tostring(now)
  `, 'Init.lua', String.raw`
    assert(#added == 0 and #gates == 0 and #progresses == 0 and #dialogs == 0 and #bezels == 0)
    assert(#sleeps == 15 and now == 1030, 'plain-catalog readiness wait must be bounded')
    assert(table.concat(logs, '\n'):match('Folder list not found: .*; no import'))
  `);
});

test('the menu still explains a plain catalog during the startup readiness wait', () => {
  run(String.raw`
    files[listing] = nil
    onSleep = function() if #sleeps == 1 then assert(load(MENU, '=AddNewFiles.lua'))() end end
  `, 'Init.lua', String.raw`
    assert(#dialogs == 1 and dialogs[1]:match("^This isn't a Card Intake catalog"), dialogs[1])
    assert(#progresses == 0 and #added == 0 and #bezels == 0 and #sleeps == 15)
  `);
});

test('a disabled export helper explains itself and clears its own export restriction', () => {
  const L = lauxlib.luaL_newstate();
  lualib.luaL_openlibs(L);
  const code = source('StartupService.lua').replace(/^return /m, 'service = ') + String.raw`
    local settings = {}
    service.startDialog(settings)
    assert(settings.LR_cantExportBecause:match('choose another Export To destination'))
    local factory = { static_text = function(_, value) return value end }
    local sections = service.sectionsForTopOfDialog(factory)
    assert(sections[1][1].title == settings.LR_cantExportBecause)
    service.endDialog(settings)
    assert(settings.LR_cantExportBecause == nil)
    settings.LR_cantExportBecause = 'Another restriction'
    service.endDialog(settings)
    assert(settings.LR_cantExportBecause == 'Another restriction')
  `;
  const status = lauxlib.luaL_dostring(L, to_luastring(code));
  const error = status === lua.LUA_OK ? null : lua.lua_tojsstring(L, -1);
  lua.lua_close(L);
  assert.equal(error, null);
});

test('the manifest requests eager initialization and keeps the Library command', () => {
  const L = lauxlib.luaL_newstate();
  lualib.luaL_openlibs(L);
  // This checks the manifest, not Lightroom's loader: real launches must prove startup.
  const code = source('Info.lua').replace(/^return /m, 'info = ') + String.raw`
    assert(info.LrForceInitPlugin == true, 'LrForceInitPlugin')
    assert(info.LrInitPlugin == 'Init.lua')
    assert(info.LrShutdownPlugin == 'PluginShutdown.lua')
    assert(info.LrExportServiceProvider.file == 'StartupService.lua')
    assert(info.URLHandler == 'UrlHandler.lua')
    assert(info.LrLibraryMenuItems[1].file == 'AddNewFiles.lua')
    assert(info.VERSION.major == 0 and info.VERSION.minor == 4)
  `;
  const status = lauxlib.luaL_dostring(L, to_luastring(code));
  const error = status === lua.LUA_OK ? null : lua.lua_tojsstring(L, -1);
  lua.lua_close(L);
  assert.equal(error, null);
});

test('the URL Card Intake opens for an already-open catalog starts the same worker', () => {
  const L = lauxlib.luaL_newstate();
  lualib.luaL_openlibs(L);
  const code = String.raw`
    local runs, logged = {}, {}
    local logger = { enable = function() end, info = function(_, m) table.insert(logged, m) end, error = function(_, m) error(m) end }
    local modules = {
      LrTasks = { startAsyncTask = function(fn) fn() end, pcall = pcall },
      LrLogger = function() return logger end,
    }
    function import(name) return assert(modules[name], name) end
    package.preload.Intake = function() return { run = function(catalog, fromMenu) table.insert(runs, { catalog, fromMenu }) end } end
  ` + source('UrlHandler.lua').replace(/^return /m, 'handler = ') + String.raw`
    handler.URLHandler('lightroom://io.github.semmmer.cardintake/add')
    assert(#runs == 1 and runs[1][1] == nil and runs[1][2] == false, 'one quiet run for the active catalog')
    assert(logged[1]:match('lightroom://io.github.semmmer.cardintake/add'))
  `;
  const status = lauxlib.luaL_dostring(L, to_luastring(code));
  const error = status === lua.LUA_OK ? null : lua.lua_tojsstring(L, -1);
  lua.lua_close(L);
  assert.equal(error, null);
});

test('Plug-in Manager lists your develop and metadata presets', () => {
  const L = lauxlib.luaL_newstate();
  lualib.luaL_openlibs(L);
  const code = String.raw`
    prefs = {}
    local function view(kind) return function(_, t) t = t or {}; t.kind = kind; return t end end
    local f = { row = view('row'), static_text = view('text'), popup_menu = view('popup') }
    local folder = {
      getName = function() return 'FroPack2' end,
      getDevelopPresets = function() return {
        { getName = function() return 'Apollo' end, getUuid = function() return 'U1' end },
      } end,
    }
    local modules = {
      LrApplication = {
        developPresetFolders = function() return { folder } end,
        metadataPresets = function() return { ['© Emmmer'] = 'M1', Basic = 'M2' } end,
      },
      LrPrefs = { prefsForPlugin = function() return prefs end },
      LrView = { bind = function(key) return 'bind:' .. key end, share = function(key) return key end },
    }
    function import(name) return assert(modules[name], name) end
  ` + source('PluginInfo.lua').replace(/^return /m, 'info = ') + String.raw`
    local section = info.sectionsForTopOfDialog(f, {})[1]
    assert(section.bind_to_object == prefs and prefs.developPreset == '' and prefs.metadataPreset == '')
    local develop, metadata = section[2][2], section[4][2]
    assert(develop.value == 'bind:developPreset' and section[3][2].value == 'bind:dngDevelopPreset')
    assert(develop.items[1].value == '' and develop.items[2].title == 'FroPack2 / Apollo' and develop.items[2].value == 'U1')
    assert(metadata.items[2].title == 'Basic' and metadata.items[3].value == 'M1')
  `;
  const status = lauxlib.luaL_dostring(L, to_luastring(code));
  const error = status === lua.LUA_OK ? null : lua.lua_tojsstring(L, -1);
  lua.lua_close(L);
  assert.equal(error, null);
});
