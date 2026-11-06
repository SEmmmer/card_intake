const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { lua, lauxlib, lualib, to_luastring } = require('fengari');

const plugin = fs.readFileSync(path.join(__dirname, '../../lrplugin/CardIntake.lrplugin/Init.lua'), 'utf8');
const harness = String.raw`
local requestPath = '/Catalogs/Card Intake.card-intake-request.txt'
local statusPath = '/Catalogs/Card Intake.card-intake-status.txt'
local folder = '/NAS/東京'
local files, existing, calls, statuses = {}, {}, {}, {}
local missing, badPhoto, busy, onSleep
local paths = { folder .. '/a.ARW', folder .. '/b.JPG', folder .. '/b.xmp', folder .. '/video.mp4' }
local function queue(id) files[requestPath] = '# Card Intake v1\n# request-id ' .. id .. '\n' .. folder .. '\n' end
local catalog = {
  getPath = function() return '/Catalogs/Card Intake.lrcat' end,
  findPhotoByPath = function(self, file) return existing[file] end,
  addPhoto = function(self, file)
    if file == badPhoto then error('Unsupported photo') end
    table.insert(calls, file)
    existing[file] = { path = file }
    return existing[file]
  end,
  withWriteAccessDo = function(self, name, callback) if not busy then callback() end end,
}
local modules = {
  LrApplication = { activeCatalog = function() return catalog end },
  LrPathUtils = {
    extension = function(file) return file:match('%.([^%.]+)$') end,
    removeExtension = function(file) return file:gsub('%.[^%.]+$', '') end,
  },
  LrFileUtils = {
    readFile = function(file) return files[file] end,
    exists = function(file)
      if file == folder and not missing then return 'directory' end
      return files[file] and 'file' or false
    end,
    delete = function(file) files[file] = nil end,
    files = function(dir)
      assert(dir == folder)
      local i = 0
      return function() i = i + 1; return paths[i] end
    end,
  },
  LrTasks = {
    pcall = pcall,
    startAsyncTask = function(fn) task = fn end,
    sleep = function()
      table.insert(statuses, files[statusPath] or '')
      if onSleep then onSleep() else CardIntakeSession.stop = true end
    end,
  },
  LrLogger = function() return { enable = function() end, error = function(self, err) error(err) end } end,
}
function import(name) return assert(modules[name], name) end
io.open = function(file)
  files[file] = ''
  return { write = function(self, text) files[file] = files[file] .. text end, close = function() end }
end
`;

function run(config, verify) {
  const L = lauxlib.luaL_newstate();
  lualib.luaL_openlibs(L);
  const code = harness + '\n' + config + '\n' + plugin + '\ntask()\n' + verify;
  const status = lauxlib.luaL_dostring(L, to_luastring(code));
  assert.equal(status, lua.LUA_OK, lua.lua_tojsstring(L, -1));
  lua.lua_close(L);
}

test('Lightroom adds originals in place and skips catalogued photos without importing sidecars/videos', () => {
  run(`queue('one'); existing[paths[1]] = {}`, String.raw`
    assert(#calls == 1 and calls[1] == paths[2])
    assert(files[statusPath] == 'one\ncomplete\n1\n1\n0\n')
    assert(files[requestPath] == nil)
  `);
});

test('Lightroom retains requests on disconnected shares and write-access timeouts', () => {
  for (const config of ['missing = true', 'busy = true']) {
    run(`queue('two'); ${config}`, String.raw`
      assert(#calls == 0)
      assert(files[requestPath] ~= nil)
      assert(files[statusPath]:match('two\nfailed\n0\n0\n1\n'))
    `);
  }
});

test('Lightroom retries partial failure in an already-open catalog on a new request, preserving added photos', () => {
  run(String.raw`
    queue('first'); badPhoto = paths[2]
    onSleep = function()
      if #statuses == 1 then
        assert(files[requestPath] ~= nil)
        assert(statuses[1]:match('first\nfailed\n1\n0\n1\n'))
        badPhoto = nil
        queue('second')
      else CardIntakeSession.stop = true end
    end
  `, String.raw`
    assert(#calls == 2)
    assert(files[statusPath] == 'second\ncomplete\n1\n1\n0\n')
    assert(files[requestPath] == nil)
  `);
});

test('Lightroom ignores requests for another catalog', () => {
  run(String.raw`files['/Catalogs/Other.card-intake-request.txt'] = '# request-id wrong\n/NAS/東京\n'`, `
    assert(#calls == 0)
    assert(files[statusPath] == nil)
    assert(files['/Catalogs/Other.card-intake-request.txt'])
  `);
});
