-- Adds originals in place. Requests belong to one specific local catalog.
local LrApplication = import "LrApplication"
local LrFileUtils = import "LrFileUtils"
local LrPathUtils = import "LrPathUtils"
local LrTasks = import "LrTasks"

CardIntakeSession = { stop = false }
local session = CardIntakeSession
local EXTENSIONS = {
  arw = true, dng = true, jpg = true, jpeg = true, hif = true, heic = true,
  tif = true, tiff = true, cr2 = true, cr3 = true, nef = true, nrw = true,
  raf = true, orf = true, rw2 = true, pef = true, srw = true, raw = true,
}

local function readRequest(path)
  local text = LrFileUtils.readFile(path) or ""
  local id = text:match("# request%-id ([^\r\n]+)")
  local folders = {}
  for line in text:gmatch("[^\r\n]+") do
    if line:sub(1, 1) ~= "#" then table.insert(folders, line) end
  end
  return id, folders, text
end

local function writeStatus(path, id, status, added, skipped, failed, detail)
  local file, err = io.open(path, "w")
  if not file then error("Could not write Card Intake status: " .. tostring(err)) end
  file:write(id .. "\n" .. status .. "\n" .. added .. "\n" .. skipped .. "\n" .. failed .. "\n" .. (detail or ""))
  file:close()
end

local function consume(catalog, request, statusPath, id, folders, original)
  local added, skipped, failed = 0, 0, 0
  local errors = {}
  writeStatus(statusPath, id, "running", 0, 0, 0)
  local ok, err = LrTasks.pcall(function()
    local paths, seen = {}, {}
    for _, folder in ipairs(folders) do
      if LrFileUtils.exists(folder) ~= "directory" then
        error("Reconnect the photo share: " .. folder)
      end
      -- Only originals directly in the shoot folder, never video or working exports.
      for path in LrFileUtils.files(folder) do
        local ext = string.lower(LrPathUtils.extension(path) or "")
        if EXTENSIONS[ext] and not seen[path] then
          seen[path] = true
          table.insert(paths, path)
        end
      end
    end
    if #folders == 0 then error("The request contains no shoot folders.") end
    table.sort(paths)
    if #paths == 0 then return end
    local wrote = false
    catalog:withWriteAccessDo("Card Intake: add photos", function()
      wrote = true
      for _, path in ipairs(paths) do
        local success, problem = LrTasks.pcall(function()
          if catalog:findPhotoByPath(path) then
            skipped = skipped + 1
          else
            local photo = catalog:addPhoto(path)
            if not photo then error("Lightroom did not add " .. path) end
            added = added + 1
          end
        end)
        if not success then
          failed = failed + 1
          if #errors < 10 then table.insert(errors, path .. ": " .. tostring(problem)) end
        end
      end
    end, { timeout = 60 })
    if not wrote then error("Lightroom is busy. Open the shoot from Card Intake again to retry.") end
  end)
  if not ok then
    failed = failed + 1
    table.insert(errors, tostring(err))
  end
  writeStatus(statusPath, id, failed == 0 and "complete" or "failed", added, skipped, failed, table.concat(errors, "\n"))
  -- Keep failed requests. A new click issues a new ID and retries.
  -- Never delete a newer request that arrived while adding these photos.
  if failed == 0 and LrFileUtils.readFile(request) == original then LrFileUtils.delete(request) end
end

LrTasks.startAsyncTask(function()
  local catalog = LrApplication.activeCatalog()
  local catalogPath = catalog:getPath()
  local stem = LrPathUtils.removeExtension(catalogPath)
  local request = stem .. ".card-intake-request.txt"
  local statusPath = stem .. ".card-intake-status.txt"
  local lastId
  -- Also handles a request for the catalog already open in Lightroom.
  while not session.stop and CardIntakeSession == session do
    if LrApplication.activeCatalog():getPath() ~= catalogPath then return end
    if LrFileUtils.exists(request) then
      local ok, err = LrTasks.pcall(function()
        local id, folders, text = readRequest(request)
        if id and id ~= lastId then
          lastId = id
          consume(catalog, request, statusPath, id, folders, text)
        end
      end)
      if not ok then
        -- A write failure keeps the request; retry with a new request or plugin reload.
        local LrLogger = import "LrLogger"
        local logger = LrLogger("CardIntake")
        logger:enable("logfile")
        logger:error(tostring(err))
      end
    end
    LrTasks.sleep(2)
  end
end)
