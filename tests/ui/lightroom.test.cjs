// Runs the real Lightroom plug-in in Fengari (Lua in JavaScript) against a mocked
// Lightroom SDK. The mock commits photos only when a write gate closes, as Lightroom
// does, so stacking a DNG on a raw added in the same gate would fail here too.
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
files = { [listing] = '# Card Intake: shoot folders\n' .. folder .. '\n' }
listed = {}
inCatalog, pending, added = {}, {}, {}
gates, dialogs, bezels, smart, keywords = {}, {}, {}, {}, {}
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
    table.insert(added, ph)
    return ph
  end,
  withWriteAccessDo = function(self, name, fn, opts)
    assert(opts and opts.timeout, 'no timeout')
    if busy then return 'aborted' end
    table.insert(gates, name)
    inGate = true; fn(); inGate = false
    for p, ph in pairs(pending) do inCatalog[p] = ph end
    pending = {}
    return 'executed'
  end,
  -- The plug-in makes no collections or keywords; these record any attempt.
  createSmartCollection = function(self, name) smart[name] = true end,
  createCollection = function(self, name) smart[name] = true end,
  createKeyword = function(self, name) table.insert(keywords, name); return { name = name } end,
  buildSmartPreviews = function(self, photos) assert(not inGate); previews = photos end,
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
  LrApplication = { activeCatalog = function() return catalog end },
  LrDialogs = {
    message = function(title, body) table.insert(dialogs, title .. ' | ' .. (body or '')) end,
    showBezel = function(text) table.insert(bezels, text) end,
  },
  LrFileUtils = {
    exists = function(p)
      if p == folder then return not missing and 'directory' or false end
      return files[p] and 'file' or false
    end,
    readFile = function(p) return files[p] end,
    files = function(d)
      assert(d == folder)
      local i = 0
      return function() i = i + 1; return listed[i] and file(listed[i]) end
    end,
  },
  LrPathUtils = {
    removeExtension = function(p) return (p:gsub('%.[^%./]*$', '')) end,
    extension = function(p) return p:match('%.([^%./]+)$') or '' end,
    leafName = function(p) return (p:match('[^/]*$')) end,
    child = function(d, n) return d .. '/' .. n end,
  },
  LrPrefs = { prefsForPlugin = function() return prefs end },
  LrProgressScope = function()
    return {
      setPortionComplete = function() end, setCaption = function() end, done = function() end,
      isCanceled = function() return false end,
    }
  end,
  LrTasks = { pcall = pcall, startAsyncTask = function(fn) fn() end },
}
function import(name) return assert(modules[name], name) end
package.preload.Intake = function() return assert(load(INTAKE, '=Intake.lua'))() end
`;

function run(config, entry, verify) {
  const L = lauxlib.luaL_newstate();
  lualib.luaL_openlibs(L);
  lua.lua_pushstring(L, to_luastring(source('Intake.lua')));
  lua.lua_setglobal(L, to_luastring('INTAKE'));
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
