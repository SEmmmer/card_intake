--[[
Card Intake plug-in for Lightroom Classic.

Card Intake installs this in Lightroom's Modules folder, so Lightroom loads it by
itself. When a catalog Card Intake made opens, the plug-in adds new raw photos and
DxO PureRAW DNGs from the catalog's shoot folders on the NAS, in place. Choose the
presets it applies in File > Plug-in Manager > Card Intake.
]]

return {
  LrSdkVersion = 13.0,
  -- addPhoto applies presets from Lightroom Classic 12.5.
  LrSdkMinimumVersion = 12.5,
  LrToolkitIdentifier = "io.github.semmmer.cardintake",
  LrPluginName = "Card Intake",
  LrInitPlugin = "Init.lua",
  -- Without this, Lightroom loads the plug-in (and runs Init.lua) only when one of its
  -- features is first used, e.g. in Plug-in Manager, so new photos waited until then.
  LrForceInitPlugin = true,
  LrPluginInfoProvider = "PluginInfo.lua",
  LrLibraryMenuItems = {
    { title = "Add new files from NAS", file = "AddNewFiles.lua" },
  },
  VERSION = { major = 0, minor = 3, revision = 1 },
}
