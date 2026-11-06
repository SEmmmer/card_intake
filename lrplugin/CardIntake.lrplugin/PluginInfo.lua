-- Plug-in Manager panel: the presets applied to photos Card Intake adds.
local LrApplication = import "LrApplication"
local LrPrefs = import "LrPrefs"
local LrView = import "LrView"

local function developItems()
  local items = { { title = "None", value = "" } }
  for _, folder in ipairs(LrApplication.developPresetFolders()) do
    for _, preset in ipairs(folder:getDevelopPresets()) do
      table.insert(items, { title = folder:getName() .. " / " .. preset:getName(), value = preset:getUuid() })
    end
  end
  return items
end

local function metadataItems()
  local items, names, presets = { { title = "None", value = "" } }, {}, LrApplication.metadataPresets()
  for name in pairs(presets) do table.insert(names, name) end
  table.sort(names)
  for _, name in ipairs(names) do table.insert(items, { title = name, value = presets[name] }) end
  return items
end

return {
  sectionsForTopOfDialog = function(f, _)
    local prefs = LrPrefs.prefsForPlugin()
    for _, key in ipairs({ "developPreset", "dngDevelopPreset", "metadataPreset" }) do
      if prefs[key] == nil then prefs[key] = "" end
    end
    local bind, share = LrView.bind, LrView.share
    local develop = developItems()
    local function choice(label, key, items)
      return f:row {
        f:static_text { title = label, alignment = "right", width = share "label" },
        f:popup_menu { items = items, value = bind(key), width_in_chars = 45 },
      }
    end
    return {
      {
        title = "Card Intake",
        bind_to_object = prefs,
        f:static_text {
          title = "When a catalog made by Card Intake opens, or from Library > Plug-in Extras > Add new files from NAS, "
            .. "new raw photos and DxO DNGs in its shoot folders are added with these presets.",
          fill_horizontal = 1, height_in_lines = 2,
        },
        choice("Develop preset for raw photos:", "developPreset", develop),
        choice("Develop preset for DxO DNGs:", "dngDevelopPreset", develop),
        choice("Metadata preset:", "metadataPreset", metadataItems()),
      },
    }
  end,
}
