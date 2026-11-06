//! What the operating system knows about a mounted volume: its ID, size, free space,
//! file system, the reader it sits in, and the drive's own health status.

use serde::Serialize;
use std::path::Path;

#[derive(Debug, Default, Clone, Serialize)]
pub struct VolumeInfo {
    /// The volume's ID: the Volume UUID on macOS, the volume serial number on Windows.
    /// Formatting the card changes it.
    pub id: Option<String>,
    /// The volume label. Cameras often leave it empty.
    pub name: Option<String>,
    pub filesystem: Option<String>,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    /// The reader's product name, e.g. "MRW-G3" (macOS only for now).
    pub reader: Option<String>,
    /// How it's connected, e.g. "USB" (macOS only for now).
    pub bus: Option<String>,
    /// The drive's health status as macOS reports it, e.g. "Verified" or "Not Supported".
    pub health: Option<String>,
    /// The device that holds the volume: "disk5" on macOS, "E:" on Windows.
    pub device: Option<String>,
}

#[cfg(target_os = "macos")]
pub fn volume_info(root: &Path) -> VolumeInfo {
    use std::ffi::OsStr;

    fn diskutil(target: &OsStr) -> Option<plist::Dictionary> {
        let out = std::process::Command::new("diskutil")
            .arg("info")
            .arg("-plist")
            .arg(target)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        plist::from_bytes(&out.stdout).ok()
    }
    fn text(d: &plist::Dictionary, key: &str) -> Option<String> {
        d.get(key)
            .and_then(|v| v.as_string())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }
    fn number(d: &plist::Dictionary, key: &str) -> Option<u64> {
        d.get(key).and_then(|v| v.as_unsigned_integer())
    }

    let Some(volume) = diskutil(root.as_os_str()) else {
        let (free, total) = crate::nas::space(root).unzip();
        return VolumeInfo {
            free_bytes: free,
            total_bytes: total,
            ..VolumeInfo::default()
        };
    };
    // The reader name and health status belong to the whole disk, not the partition.
    let whole = text(&volume, "ParentWholeDisk");
    let disk = whole.as_deref().and_then(|d| diskutil(OsStr::new(d)));
    VolumeInfo {
        id: text(&volume, "VolumeUUID"),
        name: text(&volume, "VolumeName"),
        filesystem: text(&volume, "FilesystemUserVisibleName"),
        total_bytes: number(&volume, "TotalSize"),
        free_bytes: number(&volume, "FreeSpace"),
        reader: disk.as_ref().and_then(|d| text(d, "MediaName")),
        bus: text(&volume, "BusProtocol"),
        health: disk.as_ref().and_then(|d| text(d, "SMARTStatus")),
        device: whole,
    }
}

#[cfg(windows)]
pub fn volume_info(root: &Path) -> VolumeInfo {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;

    fn text(buf: &[u16]) -> Option<String> {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let s = String::from_utf16_lossy(&buf[..end]);
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    }

    let wide: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    let (mut serial, mut max_len, mut flags) = (0u32, 0u32, 0u32);
    // SAFETY: every pointer refers to a live, correctly sized buffer or integer.
    let ok = unsafe {
        GetVolumeInformationW(
            wide.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            &mut serial,
            &mut max_len,
            &mut flags,
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    } != 0;
    let (free, total) = crate::nas::space(root).unzip();
    let drive: String = root.to_string_lossy().chars().take(2).collect();
    VolumeInfo {
        id: ok.then(|| format!("{:04X}-{:04X}", serial >> 16, serial & 0xFFFF)),
        name: if ok { text(&label) } else { None },
        filesystem: if ok { text(&fs) } else { None },
        total_bytes: total,
        free_bytes: free,
        reader: None,
        bus: None,
        health: None,
        device: drive.ends_with(':').then_some(drive),
    }
}

/// Linux and others: size and free space only for now.
#[cfg(not(any(target_os = "macos", windows)))]
pub fn volume_info(root: &Path) -> VolumeInfo {
    let (free, total) = crate::nas::space(root).unzip();
    VolumeInfo {
        free_bytes: free,
        total_bytes: total,
        ..VolumeInfo::default()
    }
}
