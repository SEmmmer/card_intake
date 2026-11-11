use super::*;

fn pair(ssid: &str, password: &str) -> WifiPair {
    WifiPair {
        ssid: ssid.into(),
        password: password.into(),
    }
}

#[test]
fn reads_labelled_lines_whatever_the_separator_and_line_ends() {
    let expected = pair("DIRECT-ab12:ILCE-7RM6", "kR7vMx2Q");
    for text in [
        "SSID:DIRECT-ab12:ILCE-7RM6\r\nPassword:kR7vMx2Q\r\n",
        "SSID: DIRECT-ab12:ILCE-7RM6\nPW: kR7vMx2Q\n",
        "ssid=DIRECT-ab12:ILCE-7RM6\npass=kR7vMx2Q",
        "[Wi-Fi]\nID : DIRECT-ab12:ILCE-7RM6\nPass Word : kR7vMx2Q\n",
    ] {
        assert_eq!(parse(text.as_bytes()), Some(expected.clone()), "{text:?}");
    }
}

#[test]
fn a_password_keeps_its_own_separators() {
    assert_eq!(
        parse(b"SSID:DIRECT-ab12:ILCE-7RM6\nPassword:a=b:c;d\n"),
        Some(pair("DIRECT-ab12:ILCE-7RM6", "a=b:c;d"))
    );
}

#[test]
fn reads_unlabelled_lines_and_utf16_text() {
    assert_eq!(
        parse(b"DIRECT-ab12:ILCE-7RM6\r\nkR7vMx2Q\r\n"),
        Some(pair("DIRECT-ab12:ILCE-7RM6", "kR7vMx2Q"))
    );
    let mut utf16 = vec![0xFF, 0xFE];
    for unit in "SSID:DIRECT-ab12:HDR-AS100V\r\nPassword:x1y2z3w4".encode_utf16() {
        utf16.extend(unit.to_le_bytes());
    }
    assert_eq!(
        parse(&utf16),
        Some(pair("DIRECT-ab12:HDR-AS100V", "x1y2z3w4"))
    );
    let mut bom = b"\xEF\xBB\xBF".to_vec();
    bom.extend(b"SSID:DIRECT-ab12:ILCE-7RM6\nPW:kR7vMx2Q");
    assert_eq!(parse(&bom), Some(pair("DIRECT-ab12:ILCE-7RM6", "kR7vMx2Q")));
}

#[test]
fn text_without_both_values_or_beyond_wi_fi_limits_is_not_a_pair() {
    assert_eq!(parse(b"SSID:DIRECT-ab12:ILCE-7RM6\n"), None);
    assert_eq!(parse(b"Password:kR7vMx2Q\n"), None);
    assert_eq!(parse(b"hello\nworld\n"), None);
    assert_eq!(parse(&[0xC3, 0x28]), None);
    let long = format!("SSID:DIRECT-{}\nPW:kR7vMx2Q", "x".repeat(40));
    assert_eq!(parse(long.as_bytes()), None);
}

#[test]
fn the_model_is_what_follows_the_colon() {
    assert_eq!(model_in_ssid("DIRECT-ab12:ILCE-7RM6"), Some("ILCE-7RM6"));
    assert_eq!(model_in_ssid("DIRECT-ab12: DSC-QX10 "), Some("DSC-QX10"));
    assert_eq!(model_in_ssid("DIRECT-ab12"), None);
    assert_eq!(model_in_ssid("DIRECT-ab12:"), None);
}

#[test]
fn reads_the_file_from_the_card_and_never_writes_to_it() {
    let card = tempfile::tempdir().unwrap();
    assert_eq!(read_card(card.path()), OnCard::Missing);
    std::fs::create_dir(card.path().join("INFO")).unwrap();
    let file = card.path().join("INFO/WIFI_INF.TXT");
    std::fs::write(&file, "nothing useful").unwrap();
    assert!(matches!(read_card(card.path()), OnCard::Unreadable(_)));
    std::fs::write(&file, "SSID:DIRECT-ab12:ILCE-7RM6\r\nPassword:kR7vMx2Q\r\n").unwrap();
    let before = std::fs::metadata(&file).unwrap().modified().unwrap();
    assert_eq!(
        read_card(card.path()),
        OnCard::Read(pair("DIRECT-ab12:ILCE-7RM6", "kR7vMx2Q"))
    );
    assert_eq!(
        std::fs::metadata(&file).unwrap().modified().unwrap(),
        before
    );
    std::fs::write(&file, vec![b'x'; 5000]).unwrap();
    assert!(matches!(read_card(card.path()), OnCard::Unreadable(_)));
}

#[test]
fn an_unreadable_store_file_refuses_to_save_over_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("camera-wifi.json");
    std::fs::write(&path, "not json").unwrap();
    let error = Store::open(&path).err().unwrap().to_string();
    let mut store = Store::unavailable(&path, error);
    assert!(
        store
            .put(
                "ILCE-7RM6",
                &pair("DIRECT-ab12:ILCE-7RM6", "kR7vMx2Q"),
                Utc::now()
            )
            .is_err()
    );
    assert!(store.forget("ILCE-7RM6").is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
}

// DPAPI works in any Windows session; the macOS Keychain may ask, so it isn't tested here.
#[cfg(windows)]
#[test]
fn saves_one_encrypted_pair_per_model_and_reveals_it_after_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("camera-wifi.json");
    let mut store = Store::open(&path).unwrap();
    let first = pair("DIRECT-ab12:ILCE-7RM6", "kR7vMx2Q");
    assert!(store.put("ILCE-7RM6", &first, Utc::now()).unwrap());
    assert!(!store.put("ILCE-7RM6", &first, Utc::now()).unwrap());

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("DIRECT-ab12:ILCE-7RM6") && text.contains("dpapi:"));
    assert!(
        !text.contains("kR7vMx2Q"),
        "the password is never in the file"
    );

    let store = Store::open(&path).unwrap();
    assert_eq!(store.reveal("ilce-7rm6").unwrap(), Some(first));
    assert_eq!(store.views().len(), 1);
    assert!(store.views()[0].saved);

    // SSID/PW Reset: the model's new pair replaces the old one.
    let mut store = store;
    let reset = pair("DIRECT-Zq90:ILCE-7RM6", "n3wPassw");
    assert!(store.put("ILCE-7RM6", &reset, Utc::now()).unwrap());
    assert!(
        store
            .put(
                "ILCE-1",
                &pair("DIRECT-cc33:ILCE-1", "alphaone"),
                Utc::now()
            )
            .unwrap()
    );
    let store = Store::open(&path).unwrap();
    assert_eq!(store.views().len(), 2);
    assert_eq!(store.reveal("ILCE-7RM6").unwrap(), Some(reset));

    let mut store = store;
    assert!(store.forget("ILCE-7RM6").unwrap());
    assert!(!store.forget("ILCE-7RM6").unwrap());
    let store = Store::open(&path).unwrap();
    assert_eq!(store.reveal("ILCE-7RM6").unwrap(), None);
    assert!(store.has("ILCE-1"));
}

#[cfg(windows)]
#[test]
fn a_blob_moved_to_another_model_does_not_decrypt() {
    let sealed = vault::seal("ILCE-7RM6", "kR7vMx2Q").unwrap();
    assert_eq!(vault::unseal("ILCE-7RM6", &sealed).unwrap(), "kR7vMx2Q");
    assert!(vault::unseal("ILCE-1", &sealed).is_err());
    assert!(vault::unseal("ILCE-7RM6", "dpapi:00ff").is_err());
    assert!(vault::unseal("ILCE-7RM6", "keychain").is_err());
}
