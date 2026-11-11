-- Log entry before loading the worker, then wait briefly for the catalog/list and
-- add photos until the upload heartbeat ends. Catalog switches restart Lightroom.
local LrTasks = import "LrTasks"
local logger = import "LrLogger"("CardIntake")
logger:enable("logfile")
logger:info("Init.lua started; Card Intake 0.4.0")

LrTasks.startAsyncTask(function()
  local ok, problem = LrTasks.pcall(function()
    require("Intake").run(nil, false)
  end)
  if not ok then
    logger:error("Init task failed: " .. tostring(problem))
    import "LrDialogs".message("Card Intake couldn't add the new files", tostring(problem), "warning")
  end
end)
