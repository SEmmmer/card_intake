-- lightroom://io.github.semmmer.cardintake/add: Card Intake asks a catalog that is
-- already open to add what's new, since start-up only runs once per catalog. A loop
-- that is already running takes the request; nothing is shown when there's nothing new.
local LrTasks = import "LrTasks"
local logger = import "LrLogger"("CardIntake")
logger:enable("logfile")

return {
  URLHandler = function(url)
    logger:info("Asked by Card Intake: " .. tostring(url))
    LrTasks.startAsyncTask(function()
      local ok, problem = LrTasks.pcall(function()
        require("Intake").run(nil, false)
      end)
      if not ok then logger:error("URL task failed: " .. tostring(problem)) end
    end)
  end,
}
