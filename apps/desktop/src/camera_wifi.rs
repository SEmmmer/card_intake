//! A Sony camera's own Wi-Fi network, read from `INFO\WIFI_INF.TXT` on its card.
//!
//! Kept like shutter counts. The pair is saved, encrypted with this computer's login,
//! when a camera in My Cameras is the model the SSID names. Otherwise it's held in
//! memory until a camera of that model is registered. Pairs from a Not mine card or
//! camera are never read. The page gets the password only when you click Show.

use super::AppState;
use card_intake_core::camera_wifi::{OnCard, WifiPair, WifiView, model_in_ssid};
use card_intake_core::shutter::MyCameras;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::sync::Mutex;
use std::time::Duration;
use tauri::State;

/// A pair read from a card while no camera of its model is in My Cameras.
pub struct Held {
    model: String,
    pair: WifiPair,
    read_at: DateTime<Utc>,
}

/// What the inserted card's Wi-Fi file gave, for the card's report. Never the password.
#[derive(Debug, Serialize, PartialEq)]
pub struct CardWifi {
    pub model: Option<String>,
    pub ssid: Option<String>,
    /// `saved`, `unchanged`, `held`, `no_model`, `unreadable` or `not_saved`.
    pub outcome: &'static str,
    pub note: Option<String>,
}

impl CardWifi {
    fn failed(outcome: &'static str, note: String) -> Self {
        Self {
            model: None,
            ssid: None,
            outcome,
            note: Some(note),
        }
    }
}

fn same_model(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

impl AppState {
    fn my_cameras_now(&self) -> MyCameras {
        self.shutter
            .lock()
            .ok()
            .and_then(|store| store.my_cameras().ok())
            .unwrap_or_default()
    }

    /// Saves or holds what the card's file gave. `card_model` is the camera of the
    /// card's newest photo, used when the SSID names no model.
    pub fn take_card_wifi(
        &self,
        found: OnCard,
        card_model: Option<&str>,
        now: DateTime<Utc>,
    ) -> Option<CardWifi> {
        let pair = match found {
            OnCard::Missing => return None,
            OnCard::Unreadable(reason) => return Some(CardWifi::failed("unreadable", reason)),
            OnCard::Read(pair) => pair,
        };
        let Some(model) = model_in_ssid(&pair.ssid).or(card_model).map(str::to_string) else {
            return Some(CardWifi {
                model: None,
                ssid: Some(pair.ssid),
                outcome: "no_model",
                note: None,
            });
        };
        let ssid = Some(pair.ssid.clone());
        let outcome = if self.my_cameras_now().has_model(&model) {
            self.drop_held_wifi(&model);
            let saved = self
                .wifi
                .lock()
                .map_err(|e| e.to_string())
                .and_then(|mut store| store.put(&model, &pair, now).map_err(|e| e.to_string()));
            match saved {
                Ok(true) => "saved",
                Ok(false) => "unchanged",
                Err(e) => {
                    return Some(CardWifi {
                        model: Some(model),
                        ssid,
                        outcome: "not_saved",
                        note: Some(format!("Couldn't save the camera's Wi-Fi password: {e}")),
                    });
                }
            }
        } else {
            let Ok(mut held) = self.held_wifi.lock() else {
                return None;
            };
            let same = held
                .iter()
                .any(|h| same_model(&h.model, &model) && h.pair == pair);
            if !same {
                held.retain(|h| !same_model(&h.model, &model));
                held.push(Held {
                    model: model.clone(),
                    pair,
                    read_at: now,
                });
            }
            if same { "unchanged" } else { "held" }
        };
        Some(CardWifi {
            model: Some(model),
            ssid,
            outcome,
            note: None,
        })
    }

    /// Saves the held pairs of models now in My Cameras, registered here or on another
    /// computer. A pair that can't be saved stays held.
    pub fn settle_wifi(&self, mine: &MyCameras) {
        let (Ok(mut held), Ok(mut store)) = (self.held_wifi.lock(), self.wifi.lock()) else {
            return;
        };
        held.retain(|h| {
            if !mine.has_model(&h.model) {
                return true;
            }
            match store.put(&h.model, &h.pair, h.read_at) {
                Ok(_) => false,
                Err(e) => {
                    eprintln!("Couldn't save the Wi-Fi password of {}: {e}", h.model);
                    true
                }
            }
        });
    }

    /// Saved and held pairs, without passwords. A held pair is newer than a saved one
    /// of the same model.
    pub fn wifi_views(&self) -> Vec<WifiView> {
        let held: Vec<WifiView> = self
            .held_wifi
            .lock()
            .map(|held| {
                held.iter()
                    .map(|h| WifiView {
                        model: h.model.clone(),
                        ssid: h.pair.ssid.clone(),
                        read_at: h.read_at,
                        saved: false,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut views: Vec<WifiView> = self
            .wifi
            .lock()
            .map(|store| store.views())
            .unwrap_or_default()
            .into_iter()
            .filter(|v| !held.iter().any(|h| same_model(&h.model, &v.model)))
            .collect();
        views.extend(held);
        views.sort_by(|a, b| a.model.cmp(&b.model));
        views
    }

    /// Deletes the pair of a model no camera in My Cameras has any more.
    pub fn drop_wifi_unless_mine(&self, model: &str, mine: &MyCameras) -> Result<(), String> {
        if mine.has_model(model) {
            return Ok(());
        }
        self.held_wifi
            .lock()
            .map_err(|e| e.to_string())?
            .retain(|h| !same_model(&h.model, model));
        self.wifi
            .lock()
            .map_err(|e| e.to_string())?
            .forget(model)
            .map(|_| ())
            .map_err(|e| format!("Couldn't delete the camera's Wi-Fi password: {e}"))
    }

    /// Drops a held pair: its camera was marked Not mine.
    pub fn drop_held_wifi(&self, model: &str) {
        if let Ok(mut held) = self.held_wifi.lock() {
            held.retain(|h| !same_model(&h.model, model));
        }
    }

    fn wifi_pair(&self, model: &str) -> Result<WifiPair, String> {
        let held = self.held_wifi.lock().map_err(|e| e.to_string())?;
        if let Some(h) = held.iter().find(|h| same_model(&h.model, model)) {
            return Ok(h.pair.clone());
        }
        drop(held);
        self.wifi
            .lock()
            .map_err(|e| e.to_string())?
            .reveal(model)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("No Wi-Fi password is saved for the {model}."))
    }
}

#[tauri::command]
pub fn camera_wifi(state: State<'_, AppState>) -> Vec<WifiView> {
    state.wifi_views()
}

#[derive(Serialize)]
pub struct Revealed {
    ssid: String,
    password: String,
    /// A Wi-Fi QR code a phone's camera can join from, as SVG.
    qr_svg: String,
}

/// The model's SSID and password, and a QR code to join it. Only on Show.
#[tauri::command]
pub fn reveal_camera_wifi(model: String, state: State<'_, AppState>) -> Result<Revealed, String> {
    let pair = state.wifi_pair(&model)?;
    Ok(Revealed {
        qr_svg: qr_svg(&pair)?,
        ssid: pair.ssid,
        password: pair.password,
    })
}

/// Copies the model's password without sending it to the page.
#[tauri::command]
pub fn copy_camera_wifi(model: String, state: State<'_, AppState>) -> Result<(), String> {
    copy_secret(state.wifi_pair(&model)?.password)
}

#[tauri::command]
pub fn forget_camera_wifi(
    model: String,
    state: State<'_, AppState>,
) -> Result<Vec<WifiView>, String> {
    state.drop_held_wifi(&model);
    state
        .wifi
        .lock()
        .map_err(|e| e.to_string())?
        .forget(&model)
        .map_err(|e| format!("Couldn't delete the camera's Wi-Fi password: {e}"))?;
    Ok(state.wifi_views())
}

/// The text of a standard Wi-Fi QR code (`WIFI:T:WPA;S:…;P:…;;`).
fn qr_text(pair: &WifiPair) -> String {
    let escape = |s: &str| {
        s.chars().fold(String::new(), |mut out, c| {
            if matches!(c, '\\' | ';' | ',' | ':' | '"') {
                out.push('\\');
            }
            out.push(c);
            out
        })
    };
    format!(
        "WIFI:T:WPA;S:{};P:{};;",
        escape(&pair.ssid),
        escape(&pair.password)
    )
}

fn qr_svg(pair: &WifiPair) -> Result<String, String> {
    use qrcode::render::svg;
    let code = qrcode::QrCode::new(qr_text(pair).as_bytes()).map_err(|e| e.to_string())?;
    let svg = code
        .render::<svg::Color>()
        .min_dimensions(176, 176)
        .quiet_zone(true)
        .build();
    // The page puts it inline; HTML has no use for the XML declaration.
    Ok(match svg.find("<svg") {
        Some(at) => svg[at..].to_string(),
        None => svg,
    })
}

const CLEAR_AFTER: Duration = Duration::from_secs(30);

/// The password last copied, until the clipboard is cleared.
static COPIED: Mutex<Option<String>> = Mutex::new(None);

/// Puts the password on the clipboard, out of Windows' clipboard history and cloud
/// clipboard (and macOS clipboard managers that honour it), and clears it after 30
/// seconds if it's still there.
fn copy_secret(secret: String) -> Result<(), String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| format!("Couldn't open the clipboard: {e}"))?;
    let set = clipboard.set();
    #[cfg(windows)]
    let set = arboard::SetExtWindows::exclude_from_monitoring(set);
    #[cfg(target_os = "macos")]
    let set = arboard::SetExtApple::exclude_from_history(set);
    set.text(secret.as_str())
        .map_err(|e| format!("Couldn't copy the password: {e}"))?;
    if let Ok(mut copied) = COPIED.lock() {
        *copied = Some(secret.clone());
    }
    std::thread::spawn(move || {
        std::thread::sleep(CLEAR_AFTER);
        clear_if_still(&secret);
    });
    Ok(())
}

fn clear_if_still(secret: &str) {
    let Ok(mut copied) = COPIED.lock() else {
        return;
    };
    if copied.as_deref() != Some(secret) {
        return;
    }
    *copied = None;
    if let Ok(mut clipboard) = arboard::Clipboard::new()
        && clipboard.get_text().is_ok_and(|text| text == secret)
    {
        let _ = clipboard.clear();
    }
}

/// Clears a copied password still on the clipboard, as the app closes.
pub fn clear_clipboard_now() {
    let copied = COPIED.lock().ok().and_then(|c| c.clone());
    if let Some(secret) = copied {
        clear_if_still(&secret);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use card_intake_core::shutter::ShutterDb;

    fn pair(ssid: &str, password: &str) -> WifiPair {
        WifiPair {
            ssid: ssid.into(),
            password: password.into(),
        }
    }

    fn read(state: &AppState, found: WifiPair, card_model: Option<&str>) -> CardWifi {
        state
            .take_card_wifi(OnCard::Read(found), card_model, Utc::now())
            .unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn a_pair_is_held_until_its_model_is_registered_then_saved_encrypted() {
        // An environment override must not send this fixture to a real share.
        if std::env::var_os("CARD_INTAKE_NAS").is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let state = crate::tests::local_state(dir.path());
        let file = dir.path().join("camera-wifi.json");
        let first = pair("DIRECT-ab12:ILCE-7RM6", "kR7vMx2Q");

        let got = read(&state, first.clone(), Some("ILCE-7RM6"));
        assert_eq!(
            (got.outcome, got.model.as_deref()),
            ("held", Some("ILCE-7RM6"))
        );
        assert_eq!(read(&state, first.clone(), None).outcome, "unchanged");
        assert!(!file.exists(), "a held pair is never written");
        assert!(!state.wifi_views()[0].saved);
        assert_eq!(state.wifi_pair("ILCE-7RM6").unwrap(), first);

        // Registering any body of the model saves it.
        state
            .shutter
            .lock()
            .unwrap()
            .register("ILCE-7RM6", Some("03506694"), None)
            .unwrap();
        let response = state.sync_shutter(&ShutterDb::default()).unwrap();
        assert_eq!(response.wifi.len(), 1);
        assert!(response.wifi[0].saved);
        assert!(state.held_wifi.lock().unwrap().is_empty());
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("DIRECT-ab12:ILCE-7RM6") && !text.contains("kR7vMx2Q"));

        // Read again: nothing new. After SSID/PW Reset: the new pair replaces it.
        assert_eq!(read(&state, first, None).outcome, "unchanged");
        let reset = pair("DIRECT-Zq90:ILCE-7RM6", "n3wPassw");
        assert_eq!(read(&state, reset.clone(), None).outcome, "saved");
        assert_eq!(state.wifi_views().len(), 1);
        assert_eq!(state.wifi_pair("ILCE-7RM6").unwrap(), reset);

        // The last camera of the model takes its Wi-Fi with it.
        let mut mine = card_intake_core::shutter::MyCameras::default();
        mine.add("ILCE-7RM6", Some("03506694"), None);
        state.drop_wifi_unless_mine("ILCE-7RM6", &mine).unwrap();
        assert_eq!(state.wifi_views().len(), 1);
        state
            .drop_wifi_unless_mine("ILCE-7RM6", &Default::default())
            .unwrap();
        assert!(state.wifi_views().is_empty());
        assert!(state.wifi_pair("ILCE-7RM6").is_err());
    }

    #[test]
    fn an_ssid_without_a_model_falls_back_to_the_cards_camera() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::tests::local_state(dir.path());
        let got = read(&state, pair("DIRECT-ab12", "kR7vMx2Q"), Some("ILCE-1"));
        assert_eq!(
            (got.outcome, got.model.as_deref()),
            ("held", Some("ILCE-1"))
        );
        let got = read(&state, pair("DIRECT-cd34", "kR7vMx2Q"), None);
        assert_eq!((got.outcome, got.model), ("no_model", None));
        assert_eq!(
            state
                .take_card_wifi(OnCard::Unreadable("bad".into()), None, Utc::now())
                .unwrap()
                .outcome,
            "unreadable"
        );
        assert_eq!(
            state.take_card_wifi(OnCard::Missing, None, Utc::now()),
            None
        );
    }

    #[test]
    fn marking_a_camera_not_mine_drops_its_held_pair() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::tests::local_state(dir.path());
        read(&state, pair("DIRECT-ab12:ILCE-7M4", "kR7vMx2Q"), None);
        crate::not_mine::mark_camera_for("ILCE-7M4".into(), Some("0451234".into()), None, &state)
            .unwrap();
        assert!(state.wifi_views().is_empty());
    }

    #[test]
    fn qr_text_escapes_wi_fi_specials() {
        let pair = WifiPair {
            ssid: "DIRECT-ab12:ILCE-7RM6".into(),
            password: r#"a;b,c"d\e"#.into(),
        };
        assert_eq!(
            qr_text(&pair),
            r#"WIFI:T:WPA;S:DIRECT-ab12\:ILCE-7RM6;P:a\;b\,c\"d\\e;;"#
        );
        let svg = qr_svg(&pair).unwrap();
        assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
        assert!(!svg.contains("ILCE") && !svg.contains("a;b"));
    }
}
