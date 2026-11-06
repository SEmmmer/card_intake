--[[
Adds a Card Intake catalog's new photos from its shoot folders on the NAS.

Card Intake writes <catalog>.card-intake.txt next to each catalog it makes, listing
the catalog's shoot folders. Raw originals in them are added in place, with the
develop and metadata presets chosen in Plug-in Manager; JPEG, HEIF and video stay
out. A DxO PureRAW DNG (DSC03518_DxO.dng) is stacked on top of its raw and takes
over the raw's rating, flag and colour label. The raw gets a keyword that is never
exported, so the "To edit" collection shows the DNG and hides the raw it replaced.
]]
local LrDialogs = import "LrDialogs"
local LrFileUtils = import "LrFileUtils"
local LrPathUtils = import "LrPathUtils"
local LrPrefs = import "LrPrefs"
local LrProgressScope = import "LrProgressScope"
local LrTasks = import "LrTasks"

local Intake = {}

Intake.REPLACED = "CardIntakeReplacedByDxO"
Intake.TO_EDIT = "To edit"

local RAW = {
  arw = true, srf = true, sr2 = true, nef = true, nrw = true, cr2 = true, cr3 = true,
  crw = true, raf = true, orf = true, rw2 = true, pef = true, srw = true, dng = true,
  raw = true, ["3fr"] = true, iiq = true, x3f = true, rwl = true, erf = true, mos = true,
}

-- The shoot folders Card Intake listed for this catalog, or nil for other catalogs.
function Intake.foldersFor(catalog)
  local listing = LrPathUtils.removeExtension(catalog:getPath()) .. ".card-intake.txt"
  if LrFileUtils.exists(listing) ~= "file" then return nil end
  local folders = {}
  for line in (LrFileUtils.readFile(listing) or ""):gmatch("[^\r\n]+") do
    if line:sub(1, 1) ~= "#" then table.insert(folders, line) end
  end
  return folders
end

-- "DSC03518" for DSC03518_DxO.dng or DSC03518_DxO_DeepPRIMEXD2s.dng; nil for other files.
function Intake.dxoBase(name)
  local stem, ext = name:match("^(.*)%.([^%.]+)$")
  if not ext or ext:lower() ~= "dng" then return nil end
  return stem:match("^(.-)[_%-][Dd][Xx][Oo]")
end

-- Raw photos and DxO DNGs directly in the shoot folders.
local function list(folders)
  local raws, dngs = {}, {}
  for _, folder in ipairs(folders) do
    if LrFileUtils.exists(folder) ~= "directory" then
      error("Reconnect the photo share: " .. folder .. " isn't available.", 0)
    end
    for path in LrFileUtils.files(folder) do
      local name = LrPathUtils.leafName(path)
      local ext = string.lower(LrPathUtils.extension(path) or "")
      if RAW[ext] and name:sub(1, 2) ~= "._" then
        local base = Intake.dxoBase(name)
        if base then
          table.insert(dngs, { path = path, stem = LrPathUtils.child(folder, base) })
        else
          table.insert(raws, path)
        end
      end
    end
  end
  table.sort(raws)
  table.sort(dngs, function(a, b) return a.path < b.path end)
  return raws, dngs
end

local function preset(uuid)
  if uuid and uuid ~= "" then return uuid end
  return nil
end

-- Adds what's new in `folders`. Returns { added, failed, errors, photos }.
function Intake.addNew(catalog, folders, progress)
  local prefs = LrPrefs.prefsForPlugin()
  local raws, dngs = list(folders)
  local rawForStem, newRaws, newDngs = {}, {}, {}
  for _, path in ipairs(raws) do
    rawForStem[string.lower(LrPathUtils.removeExtension(path))] = path
    if not catalog:findPhotoByPath(path) then table.insert(newRaws, path) end
  end
  for _, dng in ipairs(dngs) do
    if not catalog:findPhotoByPath(dng.path) then table.insert(newDngs, dng) end
  end
  local result = { added = 0, failed = 0, errors = {}, photos = {} }
  local total, done = #newRaws + #newDngs, 0
  if total == 0 then return result end

  local function canceled() return progress and progress:isCanceled() end
  local function step(path)
    done = done + 1
    if progress then
      progress:setPortionComplete(done, total)
      progress:setCaption(LrPathUtils.leafName(path))
    end
  end
  local function fail(path, problem)
    result.failed = result.failed + 1
    if #result.errors < 5 then
      table.insert(result.errors, LrPathUtils.leafName(path) .. ": " .. tostring(problem))
    end
  end
  local function add(path, stackOn, developPreset)
    local ok, photo = LrTasks.pcall(function()
      return catalog:addPhoto(path, stackOn, stackOn and "above" or nil,
        preset(prefs.metadataPreset), preset(developPreset))
    end)
    if ok and photo then
      result.added = result.added + 1
      table.insert(result.photos, photo)
      return photo
    end
    fail(path, photo or "Lightroom didn't add it.")
  end
  local function write(name, fn)
    local status = catalog:withWriteAccessDo(name, fn, { timeout = 60 })
    if status ~= "executed" then
      error("Lightroom is busy. Try Library > Plug-in Extras > Add new files from NAS in a moment.", 0)
    end
  end

  -- Raws first: a DNG is stacked on its raw, which must already be in the catalog.
  write("Card Intake: add photos", function()
    catalog:createSmartCollection(Intake.TO_EDIT, {
      combine = "intersect",
      { criteria = "keywords", operation = "noneOf", value = Intake.REPLACED, value2 = "" },
    }, nil, true)
    for _, path in ipairs(newRaws) do
      if canceled() then break end
      add(path, nil, prefs.developPreset)
      step(path)
    end
  end)

  local stacked = {}
  if #newDngs > 0 and not canceled() then
    write("Card Intake: add DxO DNGs", function()
      for _, dng in ipairs(newDngs) do
        if canceled() then break end
        local rawPath = rawForStem[string.lower(dng.stem)]
        local raw = rawPath and catalog:findPhotoByPath(rawPath)
        local photo = add(dng.path, raw, prefs.dngDevelopPreset)
        if photo and raw then table.insert(stacked, { dng = photo, raw = raw }) end
        step(dng.path)
      end
    end)
  end

  if #stacked > 0 then
    for _, pair in ipairs(stacked) do
      pair.rating = pair.raw:getRawMetadata("rating")
      pair.pick = pair.raw:getRawMetadata("pickStatus")
      pair.label = pair.raw:getRawMetadata("colorNameForLabel")
    end
    write("Card Intake: hide raws replaced by DxO", function()
      local keyword = catalog:createKeyword(Intake.REPLACED, {}, false, nil, true)
      for _, pair in ipairs(stacked) do
        local ok, problem = LrTasks.pcall(function()
          if pair.rating then pair.dng:setRawMetadata("rating", pair.rating) end
          if pair.pick and pair.pick ~= 0 then pair.dng:setRawMetadata("pickStatus", pair.pick) end
          if pair.label and pair.label ~= "" and pair.label ~= "none" then
            pair.dng:setRawMetadata("colorNameForLabel", pair.label)
          end
          pair.raw:addKeyword(keyword)
        end)
        if not ok then fail(pair.dng:getRawMetadata("path"), problem) end
      end
    end)
  end

  -- Smart Previews let you edit without the NAS, and Finish keeps them with the catalog.
  if #result.photos > 0 and not canceled() then
    catalog:buildSmartPreviews(result.photos)
  end
  return result
end

-- Adds what's new for a Card Intake catalog and says what happened. `fromMenu` is true
-- when you asked for it, so "nothing new" and "not a Card Intake catalog" are shown too.
function Intake.run(catalog, fromMenu)
  local folders = Intake.foldersFor(catalog)
  if not folders then
    if fromMenu then
      LrDialogs.message("This isn't a Card Intake catalog",
        "Open the shoot from Card Intake: it makes the catalog and lists the shoot folders next to it.", "info")
    end
    return nil
  end
  local progress = LrProgressScope { title = "Card Intake: adding new files from the NAS" }
  local ok, result = LrTasks.pcall(Intake.addNew, catalog, folders, progress)
  progress:done()
  if not ok then
    LrDialogs.message("Card Intake couldn't add the new files", tostring(result), "warning")
    return nil
  end
  local files = result.added == 1 and "1 file" or (result.added .. " files")
  if result.failed > 0 then
    LrDialogs.message("Card Intake added " .. files .. ", " .. result.failed .. " failed",
      table.concat(result.errors, "\n"), "warning")
  elseif result.added > 0 then
    LrDialogs.showBezel("Card Intake added " .. files)
  elseif fromMenu then
    LrDialogs.message("Nothing new",
      "Every raw photo and DxO DNG in this catalog's shoot folders is already in it.", "info")
  end
  return result
end

return Intake
