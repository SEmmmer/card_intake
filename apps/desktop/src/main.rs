// Hide the console window in Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use card_intake_core::volumes::{self, CardVolume};

/// Camera cards that are mounted right now. The UI polls this to spot a new card.
#[tauri::command]
fn list_cards() -> Vec<CardVolume> {
    volumes::find_camera_cards()
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![list_cards])
        .run(tauri::generate_context!())
        .expect("Card Intake failed to start");
}
