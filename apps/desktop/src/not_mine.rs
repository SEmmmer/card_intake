//! Local ownership choices. Borrowed cameras never add temporary shutter history.

use super::{
    AppState,
    settings::{NotMineCamera, NotMineCard, Settings},
};
use card_intake_core::shutter::{self, MyCameras};
use chrono::Utc;
use serde::Serialize;
use tauri::State;

#[derive(Serialize)]
pub struct Lists {
    not_mine_cards: Vec<NotMineCard>,
    not_mine_cameras: Vec<NotMineCamera>,
}

fn change(
    state: &AppState,
    edit: impl FnOnce(&mut Settings) -> Result<(), String>,
) -> Result<Lists, String> {
    let mut saved = state.settings.lock().map_err(|e| e.to_string())?;
    let mut updated = saved.clone();
    edit(&mut updated)?;
    updated
        .save(&state.settings_path)
        .map_err(|e| e.to_string())?;
    let lists = Lists {
        not_mine_cards: updated.not_mine_cards.clone(),
        not_mine_cameras: updated.not_mine_cameras.clone(),
    };
    *saved = updated;
    Ok(lists)
}

#[tauri::command]
pub fn mark_not_mine_card(card_key: String, state: State<'_, AppState>) -> Result<Lists, String> {
    let card = state
        .ledger
        .lock()
        .map_err(|e| e.to_string())?
        .cards
        .iter()
        .find(|c| c.id == card_key)
        .cloned()
        .ok_or("Insert the card to mark it Not mine.")?;
    if card.registered {
        return Err("Remove this card from My Cards first.".into());
    }
    change(&state, |s| {
        s.not_mine_cards.retain(|c| c.key != card_key);
        s.not_mine_cards.push(NotMineCard {
            key: card_key,
            model: card.model,
            capacity_bytes: card.capacity_bytes,
            sd_class: card.sd_class,
            marked_at: Utc::now(),
        });
        Ok(())
    })
}

#[tauri::command]
pub fn forget_not_mine_card(card_key: String, state: State<'_, AppState>) -> Result<Lists, String> {
    change(&state, |s| {
        s.not_mine_cards.retain(|c| c.key != card_key);
        Ok(())
    })
}

/// Health can replace a volume key with the card's own serial key. Keep the
/// local ownership choice attached to that same card.
pub fn align_card(
    state: &AppState,
    old: Option<&str>,
    card: &card_intake_core::ledger::CardRecord,
) -> Result<(), String> {
    let mut settings = state.settings.lock().map_err(|e| e.to_string())?;
    let mut updated = settings.clone();
    let Some(item) = updated
        .not_mine_cards
        .iter_mut()
        .find(|c| c.key == card.id || old.is_some_and(|old| c.key == old))
    else {
        return Ok(());
    };
    item.key = card.id.clone();
    item.model = card.model.clone().or(item.model.clone());
    item.capacity_bytes = card.capacity_bytes.or(item.capacity_bytes);
    item.sd_class = card.sd_class.clone().or(item.sd_class.clone());
    if updated != *settings {
        updated
            .save(&state.settings_path)
            .map_err(|e| e.to_string())?;
        *settings = updated;
    }
    Ok(())
}

fn camera(model: &str, serial: Option<&str>, internal: Option<&str>) -> MyCameras {
    let mut camera = MyCameras::default();
    camera.add(model, serial, internal);
    camera
}

#[tauri::command]
pub fn mark_not_mine_camera(
    model: String,
    serial: Option<String>,
    internal_serial: Option<String>,
    state: State<'_, AppState>,
) -> Result<Lists, String> {
    mark_camera_for(model, serial, internal_serial, &state)
}

pub(super) fn mark_camera_for(
    model: String,
    serial: Option<String>,
    internal_serial: Option<String>,
    state: &AppState,
) -> Result<Lists, String> {
    let model = model.trim().to_string();
    if model.is_empty() {
        return Err("A camera needs its model.".into());
    }
    if state.is_my_camera(&model, serial.as_deref(), internal_serial.as_deref()) {
        return Err("Remove this camera from My Cameras first.".into());
    }
    let matching = camera(&model, serial.as_deref(), internal_serial.as_deref());
    let lists = change(state, |s| {
        s.not_mine_cameras.retain(|c| {
            !matching.owns(&c.model, c.serial.as_deref(), c.internal_serial.as_deref())
        });
        s.not_mine_cameras.push(NotMineCamera {
            model: model.clone(),
            serial: serial.clone(),
            internal_serial: internal_serial.clone(),
            marked_at: Utc::now(),
        });
        Ok(())
    })?;
    state
        .new_cameras
        .lock()
        .map_err(|e| e.to_string())?
        .cameras
        .retain(|c| !matching.owns_camera(c));
    let mut ledger = state.ledger.lock().map_err(|e| e.to_string())?;
    ledger
        .cameras
        .retain(|c| !matching.owns(&c.model, c.serial.as_deref(), None));
    state.save_ledger(&ledger)?;
    Ok(lists)
}

#[tauri::command]
pub fn forget_not_mine_camera(
    model: String,
    serial: Option<String>,
    internal_serial: Option<String>,
    state: State<'_, AppState>,
) -> Result<Lists, String> {
    let matching = camera(&model, serial.as_deref(), internal_serial.as_deref());
    change(&state, |s| {
        s.not_mine_cameras.retain(|c| {
            !matching.owns(&c.model, c.serial.as_deref(), c.internal_serial.as_deref())
        });
        Ok(())
    })
}

/// Applied after every merge too: a NAS scan that started before the choice was
/// saved cannot put a borrowed camera back in the in-memory list.
pub fn filter(settings: &Settings, readings: &mut shutter::ShutterDb) {
    settings.drop_not_mine(readings);
}
