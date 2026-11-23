//! Best-effort safe ejection after a verified import. A failed lock never falls
//! through to dismounting media that another application is using.

use std::path::Path;

/// Why a path's volume can't be controlled as a camera card's.
#[cfg(windows)]
enum NotACard {
    NotADriveRoot,
    NotACameraCard,
    Unopened(std::io::Error),
}

/// Opens the volume of the camera card at a drive root (`E:\`) for control codes.
#[cfg(windows)]
fn open_card_volume(root: &Path) -> Result<std::fs::File, NotACard> {
    use std::os::windows::fs::OpenOptionsExt;

    let text = root.to_string_lossy();
    let bytes = text.as_bytes();
    if bytes.len() != 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || !matches!(bytes[2], b'\\' | b'/')
    {
        return Err(NotACard::NotADriveRoot);
    }
    let drive = &text[..2];
    if std::env::var("SystemDrive").is_ok_and(|system| system.eq_ignore_ascii_case(drive))
        || crate::volumes::camera_card(root).is_none()
    {
        return Err(NotACard::NotACameraCard);
    }
    std::fs::File::options()
        .read(true)
        .write(true)
        .share_mode(3)
        .open(format!(r"\\.\{drive}"))
        .map_err(NotACard::Unopened)
}

/// Sends control codes to a volume in order, stopping at the first that fails.
#[cfg(windows)]
fn control(device: &std::fs::File, codes: &[u32]) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;

    for &code in codes {
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
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn eject_card(root: &Path) -> Result<(), String> {
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, IOCTL_STORAGE_EJECT_MEDIA,
    };

    let device = open_card_volume(root).map_err(|why| match why {
        NotACard::NotADriveRoot => "Automatic ejection needs a card mounted at a drive root such as E:\\. Eject it in Explorer.".to_string(),
        NotACard::NotACameraCard => "This drive cannot be ejected as a camera card.".into(),
        NotACard::Unopened(e) => {
            format!("Windows couldn't open the card for ejection ({e}). Eject it in Explorer.")
        }
    })?;
    control(
        &device,
        &[
            FSCTL_LOCK_VOLUME,
            FSCTL_DISMOUNT_VOLUME,
            IOCTL_STORAGE_EJECT_MEDIA,
        ],
    )
    .map_err(|e| {
        format!(
            "Windows couldn't safely eject the card ({e}). Close other apps using it and eject it in Explorer."
        )
    })
}

#[cfg(not(windows))]
pub fn eject_card(_root: &Path) -> Result<(), String> {
    Err("Automatic ejection is currently available on Windows. Eject the card using your operating system.".into())
}

/// Makes Windows read the card in the reader afresh, without ejecting it: locks its
/// volume, which works only while no app has the card open, and dismounts it. The
/// next look at the drive mounts the file system that's on the card now. The error
/// says why it couldn't.
#[cfg(windows)]
pub fn remount_card(root: &Path) -> Result<(), String> {
    use windows_sys::Win32::System::Ioctl::{FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME};

    let device = open_card_volume(root).map_err(|why| match why {
        NotACard::NotADriveRoot => "it isn't mounted at a drive root".to_string(),
        NotACard::NotACameraCard => "it doesn't look like a camera card".into(),
        NotACard::Unopened(e) => format!("Windows couldn't open it: {e}"),
    })?;
    control(&device, &[FSCTL_LOCK_VOLUME, FSCTL_DISMOUNT_VOLUME])
        .map_err(|e| format!("another app may have the card open: {e}"))
}

#[cfg(not(windows))]
pub fn remount_card(_root: &Path) -> Result<(), String> {
    Err("only Windows can be asked to read a card again".into())
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

    #[test]
    fn remounts_only_a_camera_card_at_a_drive_root() {
        let dir = tempfile::tempdir().unwrap();
        assert!(remount_card(dir.path()).is_err());
        assert!(remount_card(Path::new(r"\\NAS\photos")).is_err());
        assert!(remount_card(Path::new("E:")).is_err());
        // The system drive has no DCIM and is never dismounted.
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        assert!(remount_card(Path::new(&format!("{system}\\"))).is_err());
    }
}
