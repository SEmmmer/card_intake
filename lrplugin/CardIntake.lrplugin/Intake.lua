--[[
Adds a Card Intake catalog's photos in place, including while a NAS upload runs.

Each pass re-reads <catalog>.card-intake.txt, imports new raws with the selected
presets and XMP Pick requests, stacks DxO DNGs, and builds Smart Previews. A fresh
<catalog>.card-intake.uploading heartbeat permits another pass after 3 seconds;
missing/stale heartbeats cause one final pass and stop. There is no idle watcher.
One session worker owns the progress indicator, retries busy write blocks during
uploads, and reports the aggregate result once. Diagnostics use CardIntake.log.
]]
local LrApplication = import "LrApplication"
local LrDate = import "LrDate"
local LrDialogs = import "LrDialogs"
local LrFileUtils = import "LrFileUtils"
local LrPathUtils = import "LrPathUtils"
local LrPrefs = import "LrPrefs"
local LrProgressScope = import "LrProgressScope"
local LrTasks = import "LrTasks"
local LrXml = import "LrXml"
local logger = import "LrLogger"("CardIntake")
logger:enable("logfile")

local Intake = {}
Intake.NAMESPACE = "https://github.com/semmmer/card_intake/ns/1.0/"
local running, stopping = false, false
local BUSY = "Lightroom is busy. Try Library > Plug-in Extras > Add new files from NAS in a moment."

-- Only our namespace can request a Pick; Lightroom doesn't import flags from XMP.
function Intake.pickRequested(text)
  if not text then return false end
  local ok, requested = LrTasks.pcall(function()
    local function visit(node)
      local name, namespace = node:name()
      if namespace == Intake.NAMESPACE and name == "Pick" and node:text() == "True" then return true end
      for _, attribute in pairs(node:attributes() or {}) do
        if attribute.namespace == Intake.NAMESPACE and attribute.name == "Pick" and attribute.value == "True" then return true end
      end
      for i = 1, node:childCount() or 0 do
        if visit(node:childAtIndex(i)) then return true end
      end
      return false
    end
    return visit(LrXml.parseXml(text))
  end)
  if not ok then logger:error("Pick sidecar XML: " .. tostring(requested)) end
  return ok and requested or false
end

local RAW = {
  arw = true, srf = true, sr2 = true, nef = true, nrw = true, cr2 = true, cr3 = true,
  crw = true, raf = true, orf = true, rw2 = true, pef = true, srw = true, dng = true,
  raw = true, ["3fr"] = true, iiq = true, x3f = true, rwl = true, erf = true, mos = true,
}

local function companion(catalog, suffix)
  return LrPathUtils.removeExtension(catalog:getPath()) .. suffix
end

-- Nil means another kind of catalog. An unreadable existing list can be retried.
function Intake.foldersFor(catalog)
  local listing = companion(catalog, ".card-intake.txt")
  if LrFileUtils.exists(listing) ~= "file" then return nil end
  local text = LrFileUtils.readFile(listing)
  if not text then error("Couldn't read the shoot folder list: " .. listing, 0) end
  text = text:gsub("^\239\187\191", "")
  local folders, seen = {}, {}
  for line in text:gmatch("[^\r\n]+") do
    if line:sub(1, 1) ~= "#" and not seen[line] then
      table.insert(folders, line)
      seen[line] = true
    end
  end
  return folders
end

-- Only the first line matters. Files being atomically replaced may be unreadable.
function Intake.uploadRunning(catalog)
  local ok, fresh, reason = LrTasks.pcall(function()
    local path = companion(catalog, ".card-intake.uploading")
    if LrFileUtils.exists(path) ~= "file" then return false, "missing heartbeat" end
    local text = LrFileUtils.readFile(path)
    if not text then return false, "unreadable heartbeat" end
    local first = text:gsub("^\239\187\191", ""):match("^[^\r\n]*")
    local heartbeat = tonumber(first:match("^%s*(%d+)%s*$"))
    if not heartbeat then return false, "invalid heartbeat" end
    local age = LrDate.timeToPosixDate(LrDate.currentTime()) - heartbeat
    if age > 45 then return false, string.format("stale heartbeat (%.0fs old)", age) end
    return true, "fresh heartbeat"
  end)
  if not ok then
    logger:error("Heartbeat read: " .. tostring(fresh))
    return false, "unreadable heartbeat"
  end
  return fresh, reason
end

-- "DSC03518" for DSC03518_DxO.dng or DSC03518_DxO_DeepPRIMEXD2s.dng.
function Intake.dxoBase(name)
  local stem, ext = name:match("^(.*)%.([^%.]+)$")
  if not ext or ext:lower() ~= "dng" then return nil end
  return stem:match("^(.-)[_%-][Dd][Xx][Oo]")
end

local function list(folders, uploading)
  local raws, dngs, sidecars, seen, skipped = {}, {}, {}, {}, 0
  for _, folder in ipairs(folders) do
    if LrFileUtils.exists(folder) ~= "directory" then
      if not uploading then error("Reconnect the photo share: " .. folder .. " isn't available.", 0) end
      skipped = skipped + 1
    else
      for path in LrFileUtils.files(folder) do
        local name = LrPathUtils.leafName(path)
        -- Temporary raws and AppleDouble files are excluded before looking at extensions.
        if name:sub(1, 13) ~= ".card-intake-" and name:sub(1, 2) ~= "._" and not seen[path] then
          seen[path] = true
          local ext = string.lower(LrPathUtils.extension(path) or "")
          if ext == "xmp" then sidecars[string.lower(LrPathUtils.removeExtension(path))] = path end
          if RAW[ext] then
            local base = Intake.dxoBase(name)
            if base then
              table.insert(dngs, { path = path, stem = LrPathUtils.child(folder, base) })
            else
              table.insert(raws, path)
            end
          end
        end
      end
    end
  end
  table.sort(raws)
  table.sort(dngs, function(a, b) return a.path < b.path end)
  return raws, dngs, sidecars, skipped
end

local function preset(uuid)
  if uuid and uuid ~= "" then return uuid end
  return nil
end

local function stateForPass(state)
  state = state or {}
  state.metadata = state.metadata or {}
  state.failures = state.failures or {}
  state.logged = state.logged or {}
  return state
end

local function failure(state, key, message)
  message = tostring(message)
  state.failures[key] = message
  if state.logged[key] ~= message then
    logger:error(message)
    state.logged[key] = message
  end
end

-- Adds what's new in this pass; state retains pending DNG metadata across busy gates.
-- A busy gate is a result, not an exception, so successful earlier gates are counted.
function Intake.addNew(catalog, folders, progress, uploading, state)
  state = stateForPass(state)
  local prefs = LrPrefs.prefsForPlugin()
  local raws, dngs, sidecars, skipped = list(folders, uploading)
  local rawForStem, newRaws, newDngs = {}, {}, {}
  for _, path in ipairs(raws) do
    rawForStem[string.lower(LrPathUtils.removeExtension(path))] = path
    if not catalog:findPhotoByPath(path) then table.insert(newRaws, path) end
  end
  for _, dng in ipairs(dngs) do
    if not catalog:findPhotoByPath(dng.path) then table.insert(newDngs, dng) end
  end
  local scan = string.format("Scan: raws=%d new=%d; DNGs=%d new=%d; missing folders skipped=%d",
    #raws, #newRaws, #dngs, #newDngs, skipped)
  if scan ~= state.scan then logger:info(scan); state.scan = scan end
  local result = { added = 0, failed = 0, errors = {}, photos = {}, busy = false }
  local paths = {}
  local total, done = #newRaws + #newDngs, 0
  local function canceled() return stopping or (progress and progress:isCanceled()) end
  local function step(path)
    done = done + 1
    if progress then
      progress:setPortionComplete(done, total)
      progress:setCaption(LrPathUtils.leafName(path))
    end
  end
  local function fail(path, problem)
    result.failed = result.failed + 1
    local message = LrPathUtils.leafName(path) .. ": " .. tostring(problem)
    table.insert(result.errors, message)
    failure(state, path, message)
  end
  local function add(path, stackOn, developPreset)
    local ok, photo = LrTasks.pcall(function()
      return catalog:addPhoto(path, stackOn, stackOn and "above" or nil,
        preset(prefs.metadataPreset), preset(developPreset))
    end)
    if ok and photo then
      result.added = result.added + 1
      table.insert(result.photos, photo)
      paths[photo] = path
      state.failures[path] = nil
      return photo
    end
    fail(path, photo or "Lightroom didn't add it.")
  end
  local function write(name, fn)
    local before = #result.photos
    -- During an upload the next pass retries, so don't wait long. Otherwise this is
    -- the only try: give Lightroom time to finish what it's doing at start-up.
    local ok, status = LrTasks.pcall(function()
      return catalog:withWriteAccessDo(name, fn, { timeout = uploading and 5 or 60 })
    end)
    logger:info("Write: " .. name .. " result=" .. tostring(status))
    if not ok or status ~= "executed" then
      -- A callback error aborts the transaction. Only committed gates count, and
      -- a later gate's failure must retain the earlier gates' successful imports.
      for i = #result.photos, before + 1, -1 do
        state.metadata[paths[result.photos[i]]] = nil
        table.remove(result.photos, i)
        result.added = result.added - 1
      end
      result.busy = true
      result.writeProblem = not ok and tostring(status) or BUSY
      if not ok then logger:error("Write failed: " .. tostring(status)) else logger:warn(BUSY) end
      return false
    end
    return true
  end

  local rawsReady = true
  if #newRaws > 0 then
    rawsReady = write("Card Intake: add photos", function()
      for _, path in ipairs(newRaws) do
        if canceled() then break end
        local photo = add(path, nil, prefs.developPreset)
        local sidecar = sidecars[string.lower(LrPathUtils.removeExtension(path))]
        if photo and sidecar then
          local ok, problem = LrTasks.pcall(function()
            if Intake.pickRequested(LrFileUtils.readFile(sidecar)) then
              local pick = photo:getRawMetadata("pickStatus")
              if not pick or pick == 0 then photo:setRawMetadata("pickStatus", 1) end
            end
          end)
          if not ok then fail(path, problem) end
        end
        step(path)
      end
    end)
  end

  -- Commit raws first; addPhoto's stack target must already be in the catalog.
  if rawsReady and #newDngs > 0 and not canceled() then
    write("Card Intake: add DxO DNGs", function()
      for _, dng in ipairs(newDngs) do
        if canceled() then break end
        local rawPath = rawForStem[string.lower(dng.stem)]
        local raw = rawPath and catalog:findPhotoByPath(rawPath)
        local photo = add(dng.path, raw, prefs.dngDevelopPreset)
        if photo and raw then state.metadata[dng.path] = { dng = photo, raw = raw } end
        step(dng.path)
      end
    end)
  end

  if next(state.metadata) and not canceled() then
    local values = {}
    for path, pair in pairs(state.metadata) do
      local ok, problem = LrTasks.pcall(function()
        table.insert(values, { path = path, dng = pair.dng,
          rating = pair.raw:getRawMetadata("rating"), pick = pair.raw:getRawMetadata("pickStatus"),
          label = pair.raw:getRawMetadata("colorNameForLabel") })
      end)
      if not ok then fail(path, problem) end
    end
    if #values > 0 then
      local succeeded = {}
      local executed = write("Card Intake: give DxO DNGs their raw's rating", function()
        for _, pair in ipairs(values) do
          if canceled() then break end
          local ok, problem = LrTasks.pcall(function()
            if pair.rating then pair.dng:setRawMetadata("rating", pair.rating) end
            if pair.pick and pair.pick ~= 0 then pair.dng:setRawMetadata("pickStatus", pair.pick) end
            if pair.label and pair.label ~= "" and pair.label ~= "none" then
              pair.dng:setRawMetadata("colorNameForLabel", pair.label)
            end
          end)
          if ok then
            table.insert(succeeded, pair.path)
          else
            fail(pair.path, problem)
          end
        end
      end)
      if executed then
        for _, path in ipairs(succeeded) do state.metadata[path] = nil; state.failures[path] = nil end
      end
    end
  end

  -- A later busy gate must not lose the previews for photos committed earlier.
  if #result.photos > 0 and not canceled() then
    local ok, problem = LrTasks.pcall(function() catalog:buildSmartPreviews(result.photos) end)
    if not ok then failure(state, "previews", "Smart Previews: " .. tostring(problem)) end
  end
  return result
end

-- Startup can precede activeCatalog() or the app's atomic list write. No watcher
-- remains for a plain catalog: after at most 30 seconds it returns silently.
local function ready(catalog, fromMenu)
  local lastCatalog, lastProblem, lastPath
  for attempt = 0, 15 do
    if stopping then return nil, nil end
    local ok, candidate, folders = LrTasks.pcall(function()
      local c = catalog or LrApplication.activeCatalog()
      local path = c and c:getPath()
      if not path or path == "" then return nil end
      if path ~= lastPath then logger:info("Active catalog: " .. path); lastPath = path end
      return c, Intake.foldersFor(c)
    end)
    if ok then
      if candidate then
        lastCatalog = candidate
      end
      lastProblem = nil
      if folders then
        logger:info("Folder list found: " .. companion(candidate, ".card-intake.txt") .. "; folders=" .. #folders)
        return candidate, folders
      end
      if candidate and fromMenu then break end
    else
      if lastProblem ~= tostring(candidate) then logger:error("Startup readiness: " .. tostring(candidate)) end
      lastProblem = tostring(candidate)
    end
    if attempt < 15 then
      if attempt == 0 then logger:info("Waiting for catalog/folder list; retrying for up to 30s") end
      LrTasks.sleep(2)
    end
  end
  if lastProblem then error(lastProblem, 0) end
  if not lastCatalog then error("Lightroom's active catalog isn't ready after 30 seconds.", 0) end
  logger:info("Folder list not found: " .. companion(lastCatalog, ".card-intake.txt") .. "; no import")
  return lastCatalog, nil
end

-- Unloading a plug-in or switching catalogs must not wait for an ongoing upload.
function Intake.stop()
  stopping = true
  logger:info("Plug-in shutdown requested")
end

local function explainOtherCatalog()
  LrDialogs.message("This isn't a Card Intake catalog",
    "Open the shoot from Card Intake: it makes the catalog and lists the shoot folders next to it.", "info")
end

-- One worker, one progress scope, one summary. Every yieldable call is protected
-- with LrTasks.pcall, including readiness, passes, sleeping, and SDK cleanup.
function Intake.run(catalog, fromMenu)
  if stopping then return nil end
  if running then
    logger:info("Another request joined the running loop")
    if fromMenu then
      local active = LrApplication.activeCatalog()
      if active and LrFileUtils.exists(companion(active, ".card-intake.txt")) ~= "file" then
        -- A startup readiness wait in a plain catalog is not an import loop.
        explainOtherCatalog()
      else
        LrDialogs.message("Card Intake is already adding photos",
          "Photos are already being added as they arrive. Use the progress indicator to cancel.", "info")
      end
    end
    return nil
  end
  running = true
  local progress, isIntake, fatal, canceled, passes = nil, false, false, false, 0
  local summary = { added = 0, failed = 0, errors = {}, photos = {} }
  local state = stateForPass()
  local stopReason = "no upload"
  local ok, problem = LrTasks.pcall(function()
    local folders
    catalog, folders = ready(catalog, fromMenu)
    if stopping then stopReason = "plug-in unloaded"; return end
    if not folders then
      if fromMenu then explainOtherCatalog() end
      return
    end
    isIntake = true
    local path = catalog:getPath()
    progress = LrProgressScope { title = "Card Intake: adding photos as they arrive" }
    local uploading, reason = Intake.uploadRunning(catalog)
    local sawUpload, finalPass = uploading, false
    logger:info("Loop started; " .. reason)
    while true do
      if stopping then stopReason = "plug-in unloaded"; break end
      if progress:isCanceled() then canceled = true; stopReason = "canceled"; break end
      local active = LrApplication.activeCatalog()
      if not active or active:getPath() ~= path then stopReason = "catalog changed"; break end
      passes = passes + 1
      local passOk, result = LrTasks.pcall(function()
        -- Always re-read: the app can append another shoot during this upload.
        local current = Intake.foldersFor(catalog)
        if not current then error("The shoot folder list disappeared: " .. companion(catalog, ".card-intake.txt"), 0) end
        local key = table.concat(current, "\n")
        if state.folders ~= key then logger:info("Pass folders=" .. #current); state.folders = key end
        return Intake.addNew(catalog, current, progress, uploading, state)
      end)
      if passOk then
        state.failures.pass = nil
        summary.added = summary.added + result.added
        for _, photo in ipairs(result.photos) do table.insert(summary.photos, photo) end
      else
        failure(state, "pass", tostring(result))
      end
      if progress:isCanceled() then canceled = true; stopReason = "canceled"; break end
      local stillUploading, why = Intake.uploadRunning(catalog)
      if finalPass or (not stillUploading and not sawUpload) then
        if not passOk then fatal = true end
        if passOk and result.busy then failure(state, "busy", result.writeProblem or BUSY); fatal = true end
        stopReason = why
        break
      end
      if stillUploading then
        sawUpload = true
        uploading = true
        progress:setCaption("Waiting for photos from Card Intake; added " .. summary.added .. " so far")
        LrTasks.sleep(3)
        -- Freshness must be checked again after sleeping: a stale/missing marker
        -- cannot make a missing folder silently acceptable on the final pass.
        uploading, why = Intake.uploadRunning(catalog)
        if not uploading then
          logger:info("Upload stopped: " .. why .. "; running final pass")
          finalPass = true
        end
      else
        uploading = false
        finalPass = true
        logger:info("Upload stopped: " .. why .. "; running final pass")
      end
    end
  end)
  if not ok then failure(state, "run", tostring(problem)); fatal = true end
  if progress then
    local closed, closeProblem = LrTasks.pcall(function() progress:done() end)
    if not closed then failure(state, "progress", tostring(closeProblem)); fatal = true end
  end
  running = false
  for _, message in pairs(state.failures) do table.insert(summary.errors, message) end
  table.sort(summary.errors)
  summary.failed = #summary.errors
  if isIntake or not ok then
    logger:info(string.format("Loop stopped: %s; passes=%d added=%d failed=%d", stopReason, passes, summary.added, summary.failed))
  end
  local files = summary.added == 1 and "1 file" or (summary.added .. " files")
  if stopping then return summary end
  if summary.failed > 0 then
    local title = fatal and summary.added == 0 and "Card Intake couldn't add the new files"
      or ("Card Intake added " .. files .. ", " .. summary.failed .. " failed")
    LrDialogs.message(title, table.concat(summary.errors, "\n"), "warning")
  elseif summary.added > 0 then
    LrDialogs.showBezel("Card Intake added " .. files)
  elseif isIntake and fromMenu and not canceled then
    LrDialogs.message("Nothing new", "Every raw photo and DxO DNG in this catalog's shoot folders is already in it.", "info")
  end
  if fatal and summary.added == 0 then return nil end
  if not isIntake then return nil end
  return summary
end

return Intake
