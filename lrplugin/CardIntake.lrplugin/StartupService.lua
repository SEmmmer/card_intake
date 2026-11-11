-- Requests eager plug-in initialization using Lightroom's export-service loader.
-- This helper does not export photos. Selecting it explains why Export is disabled.
local reason = "Card Intake adds photos when a catalog opens. To export photos, choose another Export To destination."

return {
  startDialog = function(settings)
    settings.LR_cantExportBecause = reason
  end,
  endDialog = function(settings)
    if settings.LR_cantExportBecause == reason then settings.LR_cantExportBecause = nil end
  end,
  sectionsForTopOfDialog = function(f)
    return { {
      title = "Card Intake start-up helper",
      f:static_text { title = reason, fill_horizontal = 1, height_in_lines = 3 },
    } }
  end,
}
