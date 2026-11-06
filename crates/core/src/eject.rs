//! Best-effort safe ejection after a verified import. A failed lock never falls
//! through to dismounting media that another application is using.

use std::path::Path;

#[cfg(windows)]
pub fn eject_card(root: &Path) -> Result<(), String> {
    use std::fs::File;
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::System::{
        IO::DeviceIoControl,
        Ioctl::{FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, IOCTL_STORAGE_EJECT_MEDIA},
    };

    let text = root.to_string_lossy();
    let bytes = text.as_bytes();
    if bytes.len() != 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || !matches!(bytes[2], b'\\' | b'/')
    {
        return Err("Automatic ejection needs a card mounted at a drive root such as E:\\. Eject it in Explorer.".into());
    }
    let drive = &text[..2];
    if std::env::var("SystemDrive").is_ok_and(|system| system.eq_ignore_ascii_case(drive))
        || crate::volumes::camera_card(root).is_none()
    {
        return Err("This drive cannot be ejected as a camera card.".into());
    }
    let device = File::options()
        .read(true)
        .write(true)
        .share_mode(3)
        .open(format!(r"\\.\{drive}"))
        .map_err(|e| {
            format!("Windows couldn't open the card for ejection ({e}). Eject it in Explorer.")
        })?;
    for code in [
        FSCTL_LOCK_VOLUME,
        FSCTL_DISMOUNT_VOLUME,
        IOCTL_STORAGE_EJECT_MEDIA,
    ] {
        let mut returned = 0u32;
        // SAFETY: device owns a valid volume handle; these control codes take no
        // input/output buffers. Closing the handle also releases the volume lock.
        let ok = unsafe {
            DeviceIoControl(
                device.as_raw_handle(),
                code,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(format!(
                "Windows couldn't safely eject the card ({}). Close other apps using it and eject it in Explorer.",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn eject_card(_root: &Path) -> Result<(), String> {
    Err("Automatic ejection is currently available on Windows. Eject the card using your operating system.".into())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn refuses_to_eject_an_arbitrary_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert!(eject_card(dir.path()).is_err());
        assert!(eject_card(Path::new(r"\\NAS\photos")).is_err());
        assert!(eject_card(Path::new("E:")).is_err());
    }
}
