-- When a catalog Card Intake made opens, add what's new in its shoot folders.
-- Lightroom restarts when it switches catalogs, so this runs for every catalog opened.
local LrApplication = import "LrApplication"
local LrTasks = import "LrTasks"
local Intake = require "Intake"

LrTasks.startAsyncTask(function()
  Intake.run(LrApplication.activeCatalog(), false)
end)
