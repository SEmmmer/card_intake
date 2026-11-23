//! A message box for when the app can't open its window to say what's wrong.

/// Shows `text` in the system's own error box and waits for OK.
pub fn error(title: &str, text: &str) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
        let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
        let (text, title) = (wide(text), wide(title));
        // SAFETY: both strings end in NUL and outlive the call; the box has no owner.
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        // The text goes in as arguments, so nothing in it is read as AppleScript.
        let _ = std::process::Command::new("osascript")
            .args([
                "-e",
                "on run argv",
                "-e",
                "display alert (item 1 of argv) message (item 2 of argv) as critical",
                "-e",
                "end run",
                title,
                text,
            ])
            .status();
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = (title, text);
}
