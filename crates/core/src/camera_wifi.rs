//! A Sony camera's own Wi-Fi network: the SSID and password it writes to
//! `INFO/WIFI_INF.TXT` on its card, so a phone can join the camera.
//!
//! The password is encrypted with this computer's login: Windows DPAPI for the current
//! user, or the macOS Keychain. Only the SSID, the model and when it was read are kept
//! in the clear (`camera-wifi.json` in the app's local data folder). Nothing here goes
//! to the NAS or to another computer, and the card is never written to.
//!
//! One pair is kept per camera model, the model Sony puts after the SSID's colon
//! (`DIRECT-ab12:ILCE-7RM6`). A newer pair for the same model replaces the old one.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

/// Where the camera writes its Wi-Fi details, from the card's root.
pub const CARD_FILE: [&str; 2] = ["INFO", "WIFI_INF.TXT"];

/// Larger than any Wi-Fi details file: something else with the same name.
const MAX_FILE_BYTES: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiPair {
    pub ssid: String,
    pub password: String,
}

/// What the card's Wi-Fi file gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnCard {
    /// The card has no `INFO/WIFI_INF.TXT`.
    Missing,
    /// The file is there but couldn't be read or doesn't hold an SSID and password.
    Unreadable(String),
    Read(WifiPair),
}

/// Reads the card's Wi-Fi file. Never writes to the card.
pub fn read_card(root: &Path) -> OnCard {
    let path = CARD_FILE
        .iter()
        .fold(root.to_path_buf(), |p, part| p.join(part));
    let shown = CARD_FILE.join("\\");
    match std::fs::metadata(&path) {
        Ok(meta) if !meta.is_file() => return OnCard::Missing,
        Ok(meta) if meta.len() > MAX_FILE_BYTES => {
            return OnCard::Unreadable(format!(
                "{shown} on this card is too large to be the camera's Wi-Fi details."
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return OnCard::Missing,
        Err(e) => return OnCard::Unreadable(format!("Couldn't read {shown} on this card: {e}")),
    }
    match std::fs::read(&path) {
        Ok(bytes) => parse(&bytes).map(OnCard::Read).unwrap_or_else(|| {
            OnCard::Unreadable(format!(
                "{shown} is on this card, but the app didn't find an SSID and password in it."
            ))
        }),
        Err(e) => OnCard::Unreadable(format!("Couldn't read {shown} on this card: {e}")),
    }
}

/// The SSID and password in the file's text. Lines read `SSID:…` and `Password:…`
/// (also `ID`, `PW`, `Pass`, `Key`, with `:` or `=`). Without labels, the line that
/// starts with `DIRECT-` is the SSID and the next line the password.
pub fn parse(bytes: &[u8]) -> Option<WifiPair> {
    let text = decode(bytes)?;
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim_matches(|c: char| c.is_whitespace() || c == '\0'))
        .filter(|l| !l.is_empty())
        .collect();
    let mut ssid = None;
    let mut password = None;
    for line in &lines {
        // The SSID itself has a colon, so the label ends at the first separator.
        let Some(at) = line.find([':', '=']) else {
            continue;
        };
        let label: String = line[..at]
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        let value = line[at + 1..].trim();
        if value.is_empty() {
            continue;
        }
        let slot = match label.as_str() {
            "ssid" | "id" | "wifissid" | "networkname" => &mut ssid,
            "password" | "pw" | "pwd" | "pass" | "passwd" | "passphrase" | "key" | "networkkey"
            | "wifipassword" => &mut password,
            _ => continue,
        };
        slot.get_or_insert(value);
    }
    if ssid.is_none()
        && let Some(i) = lines.iter().position(|l| l.starts_with("DIRECT-"))
    {
        ssid = Some(lines[i]);
        if password.is_none() {
            password = lines.get(i + 1).copied();
        }
    }
    let (ssid, password) = (ssid?, password?);
    // Wi-Fi allows an SSID of up to 32 bytes and a WPA key of up to 64 characters.
    let clean = |s: &str| !s.chars().any(char::is_control);
    (ssid.len() <= 32 && password.chars().count() <= 64 && clean(ssid) && clean(password)).then(
        || WifiPair {
            ssid: ssid.to_string(),
            password: password.to_string(),
        },
    )
}

fn decode(bytes: &[u8]) -> Option<String> {
    let utf16 = |bytes: &[u8], read: fn([u8; 2]) -> u16| {
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| read([c[0], c[1]])).collect();
        String::from_utf16(&units).ok()
    };
    match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8(rest.to_vec()).ok(),
        _ => String::from_utf8(bytes.to_vec()).ok(),
    }
}

/// The model Sony names after the SSID's colon: `DIRECT-ab12:ILCE-7RM6` → `ILCE-7RM6`.
pub fn model_in_ssid(ssid: &str) -> Option<&str> {
    let (_, model) = ssid.split_once(':')?;
    let model = model.trim();
    (!model.is_empty()).then_some(model)
}

/// A saved pair, without its password.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WifiView {
    pub model: String,
    pub ssid: String,
    pub read_at: DateTime<Utc>,
    /// False while it's kept only in memory, until a camera of the model is registered.
    pub saved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    model: String,
    ssid: String,
    read_at: DateTime<Utc>,
    /// The password as `vault::seal` returned it.
    sealed: String,
}

#[derive(Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    cameras: Vec<Entry>,
}

const FILE_VERSION: u32 = 1;

/// This computer's saved camera Wi-Fi details.
pub struct Store {
    path: PathBuf,
    entries: Vec<Entry>,
    /// Why the file couldn't be read. Nothing is saved over it then.
    unavailable: Option<String>,
}

impl Store {
    pub fn open(path: &Path) -> io::Result<Self> {
        let entries = match std::fs::read_to_string(path) {
            Ok(text) => {
                let file: StoreFile = serde_json::from_str(&text).map_err(io::Error::other)?;
                if file.version > FILE_VERSION {
                    return Err(io::Error::other(format!(
                        "{} was written by a newer Card Intake.",
                        path.display()
                    )));
                }
                file.cameras
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            path: path.to_path_buf(),
            entries,
            unavailable: None,
        })
    }

    /// A store that saves nothing, for when the file couldn't be read.
    pub fn unavailable(path: &Path, reason: String) -> Self {
        Self {
            path: path.to_path_buf(),
            entries: Vec::new(),
            unavailable: Some(reason),
        }
    }

    fn usable(&self) -> io::Result<()> {
        match &self.unavailable {
            Some(reason) => Err(io::Error::other(reason.clone())),
            None => Ok(()),
        }
    }

    pub fn views(&self) -> Vec<WifiView> {
        let mut views: Vec<_> = self
            .entries
            .iter()
            .map(|e| WifiView {
                model: e.model.clone(),
                ssid: e.ssid.clone(),
                read_at: e.read_at,
                saved: true,
            })
            .collect();
        views.sort_by(|a, b| a.model.cmp(&b.model));
        views
    }

    fn find(&self, model: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|e| e.model.eq_ignore_ascii_case(model))
    }

    pub fn has(&self, model: &str) -> bool {
        self.find(model).is_some()
    }

    /// Saves the model's pair in place of any older one. False when it was already saved.
    pub fn put(
        &mut self,
        model: &str,
        pair: &WifiPair,
        read_at: DateTime<Utc>,
    ) -> io::Result<bool> {
        self.usable()?;
        if let Some(old) = self.find(model)
            && old.ssid == pair.ssid
            && vault::unseal(&old.model, &old.sealed).is_ok_and(|p| p == pair.password)
        {
            return Ok(false);
        }
        let sealed = vault::seal(model, &pair.password)?;
        let mut entries: Vec<Entry> = self
            .entries
            .iter()
            .filter(|e| !e.model.eq_ignore_ascii_case(model))
            .cloned()
            .collect();
        entries.push(Entry {
            model: model.to_string(),
            ssid: pair.ssid.clone(),
            read_at,
            sealed,
        });
        self.write(&entries)?;
        self.entries = entries;
        Ok(true)
    }

    /// Decrypts the model's pair.
    pub fn reveal(&self, model: &str) -> io::Result<Option<WifiPair>> {
        let Some(entry) = self.find(model) else {
            return Ok(None);
        };
        Ok(Some(WifiPair {
            ssid: entry.ssid.clone(),
            password: vault::unseal(&entry.model, &entry.sealed)?,
        }))
    }

    /// Deletes the model's pair. False when there was none.
    pub fn forget(&mut self, model: &str) -> io::Result<bool> {
        self.usable()?;
        if !self.has(model) {
            return Ok(false);
        }
        let entries: Vec<Entry> = self
            .entries
            .iter()
            .filter(|e| !e.model.eq_ignore_ascii_case(model))
            .cloned()
            .collect();
        self.write(&entries)?;
        self.entries = entries;
        vault::discard(model)?;
        Ok(true)
    }

    fn write(&self, entries: &[Entry]) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = StoreFile {
            version: FILE_VERSION,
            cameras: entries.to_vec(),
        };
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_string_pretty(&file).map_err(io::Error::other)?,
        )?;
        std::fs::rename(tmp, &self.path)
    }
}

/// Encrypts passwords with this computer's login.
#[cfg(windows)]
mod vault {
    use std::io;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    const PREFIX: &str = "dpapi:";

    /// Ties a blob to its model, so one can't be moved to another camera's entry.
    fn entropy(model: &str) -> Vec<u8> {
        format!("Card Intake camera Wi-Fi/{}", model.to_ascii_uppercase()).into_bytes()
    }

    fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            pbData: bytes.as_ptr() as *mut u8,
        }
    }

    /// Copies out a buffer DPAPI allocated, and frees it.
    fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        if out.pbData.is_null() {
            return Vec::new();
        }
        // SAFETY: DPAPI returned `cbData` bytes at `pbData`, freed with LocalFree.
        let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        unsafe { LocalFree(out.pbData.cast()) };
        bytes
    }

    pub fn seal(model: &str, secret: &str) -> io::Result<String> {
        let entropy = entropy(model);
        let (input, extra) = (blob(secret.as_bytes()), blob(&entropy));
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // SAFETY: the input blobs point at live buffers; DPAPI fills `out`.
        let ok = unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
                &extra,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(format!("{PREFIX}{}", super::hex(&take(out))))
    }

    pub fn unseal(model: &str, sealed: &str) -> io::Result<String> {
        let data = sealed
            .strip_prefix(PREFIX)
            .and_then(super::unhex)
            .ok_or_else(|| io::Error::other("The saved password wasn't encrypted on Windows."))?;
        let entropy = entropy(model);
        let (input, extra) = (blob(&data), blob(&entropy));
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // SAFETY: as in `seal`.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                &extra,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            let e = io::Error::last_os_error();
            return Err(io::Error::other(format!(
                "Windows couldn't decrypt the saved password ({e}). It can only be opened by \
                 the Windows user who saved it, on this computer."
            )));
        }
        String::from_utf8(take(out)).map_err(io::Error::other)
    }

    /// DPAPI keeps nothing outside the blob.
    pub fn discard(_model: &str) -> io::Result<()> {
        Ok(())
    }
}

/// Keeps passwords in the login keychain.
#[cfg(target_os = "macos")]
mod vault {
    use security_framework::passwords;
    use std::io;

    const SERVICE: &str = "Card Intake camera Wi-Fi";
    const SEALED: &str = "keychain";
    const NOT_FOUND: i32 = -25300; // errSecItemNotFound

    fn account(model: &str) -> String {
        model.to_ascii_uppercase()
    }

    pub fn seal(model: &str, secret: &str) -> io::Result<String> {
        passwords::set_generic_password(SERVICE, &account(model), secret.as_bytes())
            .map_err(io::Error::other)?;
        Ok(SEALED.into())
    }

    pub fn unseal(model: &str, sealed: &str) -> io::Result<String> {
        if sealed != SEALED {
            return Err(io::Error::other(
                "The saved password isn't in the Keychain.",
            ));
        }
        let bytes =
            passwords::get_generic_password(SERVICE, &account(model)).map_err(io::Error::other)?;
        String::from_utf8(bytes).map_err(io::Error::other)
    }

    pub fn discard(model: &str) -> io::Result<()> {
        match passwords::delete_generic_password(SERVICE, &account(model)) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == NOT_FOUND => Ok(()),
            Err(e) => Err(io::Error::other(e)),
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod vault {
    use std::io;

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "Camera Wi-Fi passwords are saved only on Windows and macOS.",
        )
    }

    pub fn seal(_model: &str, _secret: &str) -> io::Result<String> {
        Err(unsupported())
    }

    pub fn unseal(_model: &str, _sealed: &str) -> io::Result<String> {
        Err(unsupported())
    }

    pub fn discard(_model: &str) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(windows)]
fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests;
