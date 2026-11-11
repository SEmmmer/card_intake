--[[
Card Intake plug-in for Lightroom Classic.

Card Intake installs this in Lightroom's Modules folder. At catalog start-up it
adds raw photos and DxO DNGs in place, then fills in while Card Intake's upload
heartbeat is fresh. Presets are chosen in File > Plug-in Manager > Card Intake.

Lightroom Classic 15.6 only runs Init.lua at start-up for a plug-in that offers an
export service (tested on the real program: with LrForceInitPlugin and a menu item
alone, nothing ran until Plug-in Manager was opened). So the plug-in offers one that
exports nothing: it is a start-up helper, not an export destination. For a catalog
that is already open, Card Intake opens lightroom://io.github.semmmer.cardintake/add,
which UrlHandler.lua answers.
]]

return {
  LrSdkVersion = 13.0,
  -- addPhoto applies presets from Lightroom Classic 12.5.
  LrSdkMinimumVersion = 12.5,
  LrToolkitIdentifier = "io.github.semmmer.cardintake",
  LrPluginName = "Card Intake",
  LrInitPlugin = "Init.lua",
  LrShutdownPlugin = "PluginShutdown.lua",
  -- Kept for Lightroom versions that honour it; 15.6 needs the export service below.
  LrForceInitPlugin = true,
  LrExportServiceProvider = {
    title = "Card Intake (start-up helper)", file = "StartupService.lua",
  },
  URLHandler = "UrlHandler.lua",
  LrPluginInfoProvider = "PluginInfo.lua",
  LrLibraryMenuItems = {
    { title = "Add new files from NAS", file = "AddNewFiles.lua" },
  },
  VERSION = { major = 0, minor = 4, revision = 0 },
}
