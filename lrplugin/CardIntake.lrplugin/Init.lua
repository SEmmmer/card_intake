-- Runs when Lightroom Classic loads the plugin, which happens each time a catalog opens.
-- Untested draft: needs a run in Lightroom Classic before it is relied on.

local LrApplication = import "LrApplication"
local LrFileUtils = import "LrFileUtils"
local LrLogger = import "LrLogger"
local LrPathUtils = import "LrPathUtils"
local LrTasks = import "LrTasks"

local log = LrLogger("CardIntake")
log:enable("logfile")

-- One folder path per line. Lines starting with # are ignored.
local REQUEST_FILE = "card-intake-import.txt"

local EXTENSIONS = {
  arw = true, dng = true, jpg = true, jpeg = true,
  hif = true, heic = true, tif = true, tiff = true,
}

local function readFolders(path)
  local text = LrFileUtils.readFile(path) or ""
  local folders = {}
  for line in text:gmatch("[^\r\n]+") do
    local folder = line:match("^%s*(.-)%s*$")
    if folder ~= "" and not folder:match("^#") then
      table.insert(folders, folder)
    end
  end
  return folders
end

-- Only the shoot folder itself: _work, _final and _catalog are skipped.
local function photosIn(folder)
  local found = {}
  for path in LrFileUtils.files(folder) do
    local ext = string.lower(LrPathUtils.extension(path) or "")
    if EXTENSIONS[ext] then
      table.insert(found, path)
    end
  end
  table.sort(found)
  return found
end

LrTasks.startAsyncTask(function()
  local catalog = LrApplication.activeCatalog()
  local request = LrPathUtils.child(LrPathUtils.parent(catalog:getPath()), REQUEST_FILE)
  if not LrFileUtils.exists(request) then
    return
  end

  local folders = readFolders(request)
  local paths = {}
  for _, folder in ipairs(folders) do
    if LrFileUtils.exists(folder) == "directory" then
      for _, path in ipairs(photosIn(folder)) do
        table.insert(paths, path)
      end
    else
      log:warn("Folder not found: " .. folder)
    end
  end

  local added, failed = 0, 0
  if #paths > 0 then
    catalog:withWriteAccessDo("Card Intake: add photos", function()
      for _, path in ipairs(paths) do
        local ok, err = LrTasks.pcall(function() catalog:addPhoto(path) end)
        if ok then
          added = added + 1
        else
          failed = failed + 1
          log:warn("Could not add " .. path .. ": " .. tostring(err))
        end
      end
    end, { timeout = 60 })
  end

  LrFileUtils.delete(request)
  log:info(string.format("Added %d photos from %d folder(s); %d failed", added, #folders, failed))
end)
