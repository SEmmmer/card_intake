-- Library > Plug-in Extras > Add new files from NAS: for a catalog that's already open
-- when a second card or DxO DNGs land in its shoot folders.
local LrApplication = import "LrApplication"
local LrTasks = import "LrTasks"
local Intake = require "Intake"

LrTasks.startAsyncTask(function()
  Intake.run(LrApplication.activeCatalog(), true)
end)
