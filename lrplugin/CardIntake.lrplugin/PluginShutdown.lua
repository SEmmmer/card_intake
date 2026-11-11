-- Stop an upload fill-in worker when Lightroom unloads the plug-in or exits.
-- Shutdown never opens a dialog; the existing worker releases its progress scope.
local LrTasks = import "LrTasks"
local ok, problem = LrTasks.pcall(function() require("Intake").stop() end)
if not ok then
  local logger = import "LrLogger"("CardIntake")
  logger:enable("logfile")
  logger:error("Shutdown failed: " .. tostring(problem))
end
