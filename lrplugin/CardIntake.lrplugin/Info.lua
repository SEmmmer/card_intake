--[[
Card Intake plugin for Lightroom Classic.

When Card Intake opens a new per-shoot catalog, it leaves a request file named
card-intake-import.txt next to the catalog. On startup this plugin adds every photo
from the folders listed in that file (the same as Import > Add), then deletes it.

Install once per computer: File > Plug-in Manager > Add, and pick this folder.
]]

return {
  LrSdkVersion = 12.0,
  LrSdkMinimumVersion = 10.0,
  LrToolkitIdentifier = "io.github.semmmer.cardintake",
  LrPluginName = "Card Intake",
  LrInitPlugin = "Init.lua",
  VERSION = { major = 0, minor = 1, revision = 0 },
}
