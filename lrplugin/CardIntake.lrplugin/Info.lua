--[[
Card Intake plugin for Lightroom Classic.

When Card Intake opens a new per-shoot catalog, it leaves a request file named
<catalog-name>.card-intake-request.txt next to the catalog. This plugin adds photos
in place (Import > Add), skips catalogued photos, and reports the result to the app.

Install once per computer: File > Plug-in Manager > Add, and pick this folder.
]]

return {
  LrSdkVersion = 12.0,
  LrSdkMinimumVersion = 10.0,
  LrToolkitIdentifier = "io.github.semmmer.cardintake",
  LrPluginName = "Card Intake",
  LrInitPlugin = "Init.lua",
  LrShutdownPlugin = "Shutdown.lua",
  VERSION = { major = 0, minor = 2, revision = 0 },
}
