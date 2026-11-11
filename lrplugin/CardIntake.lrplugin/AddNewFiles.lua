-- Library > Plug-in Extras > Add new files from NAS starts the same bounded
-- upload fill-in loop as start-up. An existing loop is shared, never duplicated.
local LrTasks = import "LrTasks"

LrTasks.startAsyncTask(function()
  local ok, problem = LrTasks.pcall(function()
    require("Intake").run(nil, true)
  end)
  if not ok then
    local logger = import "LrLogger"("CardIntake")
    logger:enable("logfile")
    logger:error("Menu task failed: " .. tostring(problem))
    import "LrDialogs".message("Card Intake couldn't add the new files", tostring(problem), "warning")
  end
end)
