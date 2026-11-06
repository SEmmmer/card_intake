//! Everything the app can find out about a card and its reader without admin rights,
//! whatever the card: the reader's own USB name, protocol and link speed, the slot the
//! card is in, the card's exact size and lock switch, and how it's formatted.
//!
//! For an SD card in a USB reader this is most of what there is to know. The card's
//! own registers — the CID (maker, product name, serial number, manufacture date), the
//! CSD and the SD Status (speed classes) — don't pass through a USB reader's mass-storage
//! interface. Readers that can fetch them do it with undocumented vendor commands,
//! which the app doesn't send: an unknown command to a reader could as well rewrite its
//! firmware. SD cards keep no standard health log either. Only an SD host controller
//! (a laptop's built-in slot) shows the card itself.
//!
//! What any reader does pass on is the card's exact size, which SD cards count in
//! 512 KiB steps (so it tells SDHC from SDXC), and its file system, whose boot sector
//! says whether it was closed cleanly and whether its writer met bad sectors.

use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CardDetails {
    /// The USB card reader the card is in.
    pub reader: Option<Reader>,
    /// The card's slot in its reader, 1 for the first, and how many slots the reader
    /// has.
    pub slot: Option<u32>,
    pub slots: Option<u32>,
    /// The device says its medium can be removed, as SD readers do.
    pub removable: Option<bool>,
    /// Windows can't write to the card: an SD card's lock switch is on.
    pub write_protected: Option<bool>,
    /// The card's exact size.
    pub capacity_bytes: Option<u64>,
    pub sector_bytes: Option<u32>,
    /// "SDSC", "SDHC", "SDXC" or "SDUC" by size, when the card looks like an SD card.
    pub sd_class: Option<String>,
    /// "MBR", "GPT", or "none" when the file system starts at the card's first sector.
    pub partition_table: Option<String>,
    /// The MBR's disk signature in hex. Windows writes a random one; cameras usually
    /// leave it 00000000.
    pub disk_signature: Option<String>,
    /// Where the card's partition starts.
    pub partition_offset_bytes: Option<u64>,
    /// The MBR partition type in hex: "07" for exFAT, "0C" for FAT32.
    pub partition_type: Option<String>,
    pub file_system: Option<FileSystemDetails>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Reader {
    /// The name the reader gives itself over USB, e.g. "CFexpress Card Reader".
    pub name: Option<String>,
    /// Its USB vendor and product ID, e.g. "05E3:0764".
    pub usb_id: Option<String>,
    /// The USB version the reader declares, e.g. "3.20".
    pub usb_version: Option<String>,
    /// "UAS" (USB Attached SCSI, which queues commands) or "BOT" (bulk-only, older
    /// and slower).
    pub protocol: Option<String>,
    /// How fast the reader's USB link runs now, e.g. "USB 5 Gbps".
    pub link: Option<String>,
    /// The fastest link the reader supports, when Windows can tell. Faster than
    /// `link` means another port or cable would speed it up.
    pub link_capable: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct FileSystemDetails {
    /// "exFAT", "FAT32", "FAT16" or "FAT12"; otherwise what Windows calls it.
    pub name: String,
    pub cluster_bytes: Option<u64>,
    /// The exFAT revision, e.g. "1.00".
    pub revision: Option<String>,
    /// The file system wasn't closed cleanly: the card came out, or the camera lost
    /// power, while it was writing.
    pub dirty: Option<bool>,
    /// The file system's writer met sectors it couldn't read or write (exFAT's
    /// MediaFailure flag, FAT's hard-error bit).
    pub media_failure: Option<bool>,
    /// How full the file system says it is (exFAT), in percent.
    pub percent_in_use: Option<u8>,
    /// Where the data area starts, from the start of the file system.
    pub data_offset_bytes: Option<u64>,
}

/// Reads what the card and its reader reveal. Never asks for admin rights; facts
/// Windows doesn't give a normal user are left out.
pub fn card_details(root: &Path) -> CardDetails {
    platform::card_details(root)
}

/// SD cards count their capacity in 512 KiB steps (SDHC and larger).
const SD_STEP: u64 = 512 * 1024;
const GIB: u64 = 1 << 30;

/// The SD capacity class of a card this size: SDSC up to 2 GB, SDHC up to 32 GB, SDXC
/// up to 2 TB, SDUC beyond. `None` for a size above 2 GB that isn't a whole number of
/// 512 KiB steps, which no SD card has.
pub fn sd_class(capacity: u64) -> Option<&'static str> {
    if capacity == 0 {
        return None;
    }
    if capacity <= 2 * GIB {
        return Some("SDSC");
    }
    if !capacity.is_multiple_of(SD_STEP) {
        return None;
    }
    Some(if capacity <= 32 * GIB {
        "SDHC"
    } else if capacity <= 2048 * GIB {
        "SDXC"
    } else {
        "SDUC"
    })
}

/// What a file system's first sector says.
#[derive(Debug, Clone, PartialEq)]
pub struct BootSector {
    pub details: FileSystemDetails,
    /// FAT only: where the first FAT starts, from the start of the file system. Its
    /// second entry holds FAT's clean-shutdown and hard-error bits.
    pub fat_offset: Option<u64>,
}

/// Parses an exFAT or FAT12/16/32 boot sector.
pub fn parse_boot_sector(sector: &[u8]) -> Option<BootSector> {
    if sector.len() < 512 || sector[510..512] != [0x55, 0xAA] {
        return None;
    }
    let u16_at = |at: usize| u16::from_le_bytes([sector[at], sector[at + 1]]) as u64;
    let u32_at = |at: usize| u32::from_le_bytes(sector[at..at + 4].try_into().unwrap()) as u64;
    if &sector[3..11] == b"EXFAT   " {
        let sector_shift = sector[108] as u32;
        let cluster_shift = sector[109] as u32;
        if !(9..=12).contains(&sector_shift) || sector_shift + cluster_shift > 25 {
            return None;
        }
        let flags = u16_at(106);
        return Some(BootSector {
            details: FileSystemDetails {
                name: "exFAT".into(),
                cluster_bytes: Some(1 << (sector_shift + cluster_shift)),
                revision: Some(format!("{}.{:02}", sector[105], sector[104])),
                dirty: Some(flags & 0b010 != 0),
                media_failure: Some(flags & 0b100 != 0),
                percent_in_use: (sector[112] <= 100).then_some(sector[112]),
                data_offset_bytes: Some(u32_at(88) << sector_shift),
            },
            fat_offset: None,
        });
    }
    // FAT: the BIOS parameter block. The FAT type follows from the cluster count.
    let bytes_per_sector = u16_at(11);
    let sectors_per_cluster = sector[13] as u64;
    let reserved = u16_at(14);
    let fats = sector[16] as u64;
    if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096)
        || !sectors_per_cluster.is_power_of_two()
        || fats == 0
        || reserved == 0
    {
        return None;
    }
    let root_dir_sectors = (u16_at(17) * 32).div_ceil(bytes_per_sector);
    let fat_sectors = match u16_at(22) {
        0 => u32_at(36),
        n => n,
    };
    let total_sectors = match u16_at(19) {
        0 => u32_at(32),
        n => n,
    };
    let metadata = reserved + fats * fat_sectors + root_dir_sectors;
    let clusters = total_sectors.checked_sub(metadata)? / sectors_per_cluster;
    let name = match clusters {
        0..4085 => "FAT12",
        4085..65525 => "FAT16",
        _ => "FAT32",
    };
    Some(BootSector {
        details: FileSystemDetails {
            name: name.into(),
            cluster_bytes: Some(sectors_per_cluster * bytes_per_sector),
            data_offset_bytes: Some(metadata * bytes_per_sector),
            ..FileSystemDetails::default()
        },
        fat_offset: Some(reserved * bytes_per_sector),
    })
}

/// FAT's own flags, from the second entry of the first FAT: (dirty, media failure).
/// FAT12 has none.
pub fn parse_fat_flags(name: &str, fat: &[u8]) -> (Option<bool>, Option<bool>) {
    // A set bit means "clean" and "no hard error".
    let (entry, clean, no_error) = match name {
        "FAT32" if fat.len() >= 8 => (
            u32::from_le_bytes(fat[4..8].try_into().unwrap()),
            0x0800_0000,
            0x0400_0000,
        ),
        "FAT16" if fat.len() >= 4 => (u16::from_le_bytes([fat[2], fat[3]]) as u32, 0x8000, 0x4000),
        _ => return (None, None),
    };
    (Some(entry & clean == 0), Some(entry & no_error == 0))
}

/// The speed of a USB link, from the hub's connection information: `speed` is
/// Windows' USB_DEVICE_SPEED, `flags` the V2 flags (bit 0: at SuperSpeed or higher,
/// bit 2: at SuperSpeedPlus or higher).
pub fn link_name(speed: u8, flags: u32) -> Option<&'static str> {
    if flags & 0b100 != 0 {
        return Some("USB 10 Gbps or faster");
    }
    if flags & 0b001 != 0 {
        return Some("USB 5 Gbps");
    }
    match speed {
        0 => Some("USB 1.0 (1.5 Mbps)"),
        1 => Some("USB 1.1 (12 Mbps)"),
        2 => Some("USB 2.0 (480 Mbps)"),
        3 => Some("USB 5 Gbps"),
        _ => None,
    }
}

/// The fastest link a USB device supports: from the V2 flags (bit 1: SuperSpeed
/// capable, bit 3: SuperSpeedPlus capable), or from its declared USB version.
pub fn link_capable(flags: u32, bcd_usb: u16) -> Option<&'static str> {
    if flags & 0b1000 != 0 {
        Some("USB 10 Gbps or faster")
    } else if flags & 0b0010 != 0 {
        Some("USB 5 Gbps")
    } else if (0x0200..0x0300).contains(&bcd_usb) {
        Some("USB 2.0 (480 Mbps)")
    } else {
        None
    }
}

/// A USB version from a device descriptor's bcdUSB, e.g. 0x0320 → "3.20".
pub fn usb_version(bcd_usb: u16) -> Option<String> {
    (bcd_usb != 0).then(|| format!("{:x}.{:02x}", bcd_usb >> 8, bcd_usb & 0xFF))
}

#[cfg(windows)]
mod platform {
    use super::*;
    use crate::storage::platform::{
        BUS_MMC, BUS_NVME, BUS_SD, BUS_USB, UsbReader, drive_number, drive_path, ioctl, open,
        open_drive, u32_at, usb_reader, wide_text,
    };
    use crate::storage::{nvme_bridge, platform::descriptor};
    use std::fs::File;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{FileExt, OpenOptionsExt};
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Child, CM_Get_DevNode_PropertyW,
        CM_Get_Device_IDW, CM_Get_Device_Interface_ListW, CM_Get_Parent, CM_Get_Sibling,
        CR_SUCCESS,
    };
    use windows_sys::Win32::Devices::Properties::{
        DEVPKEY_Device_Address, DEVPKEY_Device_BusReportedDeviceDesc, DEVPKEY_Device_Service,
        DEVPROPKEY,
    };
    use windows_sys::Win32::Devices::Usb::{
        GUID_DEVINTERFACE_USB_HUB, IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX,
        IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX_V2,
    };
    use windows_sys::Win32::Foundation::ERROR_WRITE_PROTECT;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceW;
    use windows_sys::Win32::Storage::IscsiDisc::IOCTL_SCSI_GET_ADDRESS;
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_IS_VOLUME_DIRTY, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_GET_DRIVE_LAYOUT_EX,
        IOCTL_DISK_GET_PARTITION_INFO_EX, IOCTL_DISK_IS_WRITABLE, PARTITION_STYLE_GPT,
        PARTITION_STYLE_MBR,
    };

    pub fn card_details(root: &Path) -> CardDetails {
        let mut details = CardDetails::default();
        let drive: String = root.to_string_lossy().chars().take(2).collect();
        let Ok(number) = drive_number(root) else {
            return details;
        };
        let Ok(disk) = open_drive(&drive_path(number)) else {
            return details;
        };
        let device = descriptor(&disk).ok();
        let bus = device.as_ref().map(|d| d.bus);
        details.removable = device.as_ref().map(|d| d.removable);

        let mut geometry = [0u8; 256];
        if ioctl(&disk, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut geometry).is_ok() {
            details.sector_bytes = Some(u32_at(&geometry, 20));
            details.capacity_bytes = Some(u64::from_le_bytes(geometry[24..32].try_into().unwrap()));
        }
        details.write_protected = match ioctl(&disk, IOCTL_DISK_IS_WRITABLE, &[], &mut []) {
            Ok(_) => Some(false),
            Err(e) if e.raw_os_error() == Some(ERROR_WRITE_PROTECT as i32) => Some(true),
            Err(_) => None,
        };
        let mut address = [0u8; 8];
        if ioctl(&disk, IOCTL_SCSI_GET_ADDRESS, &[], &mut address).is_ok() {
            details.slot = Some(address[7] as u32 + 1);
        }
        // An empty slot has no size, and Windows answers layout queries for it with zeros.
        let has_medium = details.capacity_bytes.is_some_and(|c| c > 0);
        if has_medium {
            read_layout(&disk, &drive, &mut details);
        }

        let mut nvme = bus == Some(BUS_NVME);
        if bus == Some(BUS_USB)
            && let Some(usb) = usb_reader(number)
        {
            nvme |= nvme_bridge(usb.vendor, usb.product).is_some();
            details.slots = child_count(usb.devinst);
            details.reader = Some(reader(&usb));
        }
        // An SD host controller shows the card itself; behind USB an SD card is a
        // removable medium sized in SD's steps.
        let sd =
            matches!(bus, Some(BUS_SD | BUS_MMC)) || (details.removable == Some(true) && !nvme);
        if sd {
            details.sd_class = details
                .capacity_bytes
                .and_then(sd_class)
                .map(str::to_string);
        }
        if has_medium {
            details.file_system = file_system(root, &drive, details.sector_bytes);
        }
        details
    }

    /// The partition table, and where the card's partition starts.
    fn read_layout(disk: &File, drive: &str, details: &mut CardDetails) {
        let mut layout = vec![0u8; 8192];
        if ioctl(disk, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, &[], &mut layout).is_ok() {
            let style = u32_at(&layout, 0) as i32;
            details.partition_table = Some(
                match style {
                    PARTITION_STYLE_MBR => "MBR",
                    PARTITION_STYLE_GPT => "GPT",
                    _ => "none",
                }
                .into(),
            );
            if style == PARTITION_STYLE_MBR {
                details.disk_signature = Some(format!("{:08X}", u32_at(&layout, 8)));
            }
        }
        // PARTITION_INFORMATION_EX: style, offset, length, number, two flags, then the
        // MBR type byte at 32.
        let mut partition = [0u8; 144];
        if let Ok(volume) = open(&format!(r"\\.\{drive}"), 0)
            && ioctl(
                &volume,
                IOCTL_DISK_GET_PARTITION_INFO_EX,
                &[],
                &mut partition,
            )
            .is_ok()
        {
            let offset = u64::from_le_bytes(partition[8..16].try_into().unwrap());
            details.partition_offset_bytes = Some(offset);
            if u32_at(&partition, 0) as i32 == PARTITION_STYLE_MBR {
                details.partition_type = Some(format!("{:02X}", partition[32]));
            }
            if offset == 0 {
                details.partition_table = Some("none".into());
                details.disk_signature = None;
            }
        }
    }

    /// A sector-aligned buffer for raw reads.
    #[repr(C, align(4096))]
    struct Sector([u8; 4096]);

    /// The file system's boot sector facts. Windows lets a normal user read the first
    /// sectors of a removable card's volume; for a fixed disk only the cluster size
    /// and Windows' own dirty flag are left.
    fn file_system(
        root: &Path,
        drive: &str,
        sector_bytes: Option<u32>,
    ) -> Option<FileSystemDetails> {
        let len = sector_bytes.unwrap_or(512).clamp(512, 4096) as usize;
        let volume = File::options()
            .read(true)
            .share_mode(3)
            .open(format!(r"\\.\{drive}"));
        let mut buffer = Box::new(Sector([0; 4096]));
        let boot = volume.as_ref().ok().and_then(|v| {
            v.seek_read(&mut buffer.0[..len], 0).ok()?;
            parse_boot_sector(&buffer.0[..len])
        });
        if let Some(mut boot) = boot {
            if let (Some(offset), Ok(v)) = (boot.fat_offset, &volume)
                && v.seek_read(&mut buffer.0[..len], offset).is_ok()
            {
                let (dirty, failure) = parse_fat_flags(&boot.details.name, &buffer.0[..len]);
                boot.details.dirty = dirty;
                boot.details.media_failure = failure;
            }
            return Some(boot.details);
        }
        let name = crate::volume_info::volume_info(root).filesystem?;
        Some(FileSystemDetails {
            name,
            cluster_bytes: cluster_bytes(root),
            dirty: windows_dirty(drive),
            ..FileSystemDetails::default()
        })
    }

    fn cluster_bytes(root: &Path) -> Option<u64> {
        let wide: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
        let (mut per_cluster, mut per_sector, mut free, mut total) = (0, 0, 0, 0);
        // SAFETY: a NUL-terminated path and plain out-parameters.
        let ok = unsafe {
            GetDiskFreeSpaceW(
                wide.as_ptr(),
                &mut per_cluster,
                &mut per_sector,
                &mut free,
                &mut total,
            )
        };
        (ok != 0).then(|| per_cluster as u64 * per_sector as u64)
    }

    /// Whether Windows has the volume marked for a check.
    fn windows_dirty(drive: &str) -> Option<bool> {
        let volume = open(&format!(r"\\.\{drive}"), 0).ok()?;
        let mut flags = [0u8; 4];
        ioctl(&volume, FSCTL_IS_VOLUME_DIRTY, &[], &mut flags).ok()?;
        Some(u32_at(&flags, 0) & 1 != 0)
    }

    fn reader(usb: &UsbReader) -> Reader {
        let mut reader = Reader {
            name: property_text(usb.devinst, &DEVPKEY_Device_BusReportedDeviceDesc),
            usb_id: Some(format!("{:04X}:{:04X}", usb.vendor, usb.product)),
            protocol: property_text(usb.devinst, &DEVPKEY_Device_Service).map(|service| {
                match service.to_ascii_uppercase().as_str() {
                    "UASPSTOR" => "UAS".into(),
                    "USBSTOR" => "BOT".into(),
                    _ => service,
                }
            }),
            ..Reader::default()
        };
        if let Some((speed, flags, bcd_usb)) = connection(usb.devinst) {
            reader.usb_version = usb_version(bcd_usb);
            reader.link = link_name(speed, flags).map(str::to_string);
            reader.link_capable = link_capable(flags, bcd_usb).map(str::to_string);
        }
        reader
    }

    /// How the reader is connected to its hub: (speed, V2 flags, bcdUSB).
    fn connection(devinst: u32) -> Option<(u8, u32, u16)> {
        let port = property_u32(devinst, &DEVPKEY_Device_Address)?;
        let mut hub = 0;
        let mut hub_id = [0u16; 400];
        // SAFETY: plain out-parameters, with the buffer's real length.
        unsafe {
            if CM_Get_Parent(&mut hub, devinst, 0) != CR_SUCCESS
                || CM_Get_Device_IDW(hub, hub_id.as_mut_ptr(), hub_id.len() as u32, 0) != CR_SUCCESS
            {
                return None;
            }
        }
        let mut paths = vec![0u16; 4096];
        // SAFETY: `hub_id` is NUL-terminated, and the buffer's real length is passed.
        let listed = unsafe {
            CM_Get_Device_Interface_ListW(
                &GUID_DEVINTERFACE_USB_HUB,
                hub_id.as_ptr(),
                paths.as_mut_ptr(),
                paths.len() as u32,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if listed != CR_SUCCESS {
            return None;
        }
        let hub = open(&wide_text(&paths), 0).ok()?;

        // USB_NODE_CONNECTION_INFORMATION_EX, packed: the port, then the device
        // descriptor (bcdUSB at 6), then the speed at 23.
        let mut info = [0u8; 512];
        info[..4].copy_from_slice(&port.to_le_bytes());
        let input = info;
        ioctl(
            &hub,
            IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX,
            &input,
            &mut info,
        )
        .ok()?;
        let bcd_usb = u16::from_le_bytes([info[6], info[7]]);
        let speed = info[23];

        // USB_NODE_CONNECTION_INFORMATION_EX_V2: port, length, the USB versions asked
        // about (1.1, 2.0 and 3.x), then the flags.
        let mut v2 = [0u8; 16];
        v2[..4].copy_from_slice(&port.to_le_bytes());
        v2[4..8].copy_from_slice(&16u32.to_le_bytes());
        v2[8..12].copy_from_slice(&0b111u32.to_le_bytes());
        let input = v2;
        let flags = ioctl(
            &hub,
            IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX_V2,
            &input,
            &mut v2,
        )
        .map_or(0, |_| u32_at(&v2, 12));
        Some((speed, flags, bcd_usb))
    }

    /// How many devices hang off the reader: one per slot.
    fn child_count(devinst: u32) -> Option<u32> {
        let mut child = 0;
        // SAFETY: plain out-parameters.
        if unsafe { CM_Get_Child(&mut child, devinst, 0) } != CR_SUCCESS {
            return None;
        }
        let mut count = 1;
        // SAFETY: as above.
        while unsafe { CM_Get_Sibling(&mut child, child, 0) } == CR_SUCCESS {
            count += 1;
        }
        Some(count)
    }

    fn property(devinst: u32, key: &DEVPROPKEY, buffer: &mut [u8]) -> Option<usize> {
        let mut kind = 0;
        let mut size = buffer.len() as u32;
        // SAFETY: the buffer's real length is passed and updated.
        let found = unsafe {
            CM_Get_DevNode_PropertyW(devinst, key, &mut kind, buffer.as_mut_ptr(), &mut size, 0)
        };
        (found == CR_SUCCESS).then_some(size as usize)
    }

    fn property_text(devinst: u32, key: &DEVPROPKEY) -> Option<String> {
        let mut buffer = [0u8; 512];
        let size = property(devinst, key, &mut buffer)?;
        let wide: Vec<u16> = buffer[..size]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let text = wide_text(&wide).trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    fn property_u32(devinst: u32, key: &DEVPROPKEY) -> Option<u32> {
        let mut buffer = [0u8; 4];
        (property(devinst, key, &mut buffer)? == 4).then(|| u32::from_le_bytes(buffer))
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub fn card_details(_root: &Path) -> CardDetails {
        CardDetails::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_sd_capacity_classes_by_size_and_rejects_sizes_no_sd_card_has() {
        // The DJI Osmo Pocket 3's 256 GB card: 488,032 steps of 512 KiB.
        assert_eq!(sd_class(255_869_321_216), Some("SDXC"));
        assert_eq!(sd_class(31_914_983_424), Some("SDHC"));
        assert_eq!(sd_class(1_977_614_336), Some("SDSC"));
        assert_eq!(sd_class(4 * 1024 * GIB), Some("SDUC"));
        // A CFexpress card's NVMe size isn't counted in SD's steps.
        assert_eq!(sd_class(650_136_969_216), None);
        assert_eq!(sd_class(0), None);
    }

    /// The boot sector of the DJI Osmo Pocket 3's 256 GB card, as read from it.
    fn exfat_boot(flags: u16) -> Vec<u8> {
        let mut s = vec![0u8; 512];
        s[..3].copy_from_slice(&[0xEB, 0x76, 0x90]);
        s[3..11].copy_from_slice(b"EXFAT   ");
        s[72..80].copy_from_slice(&499_744_704u64.to_le_bytes());
        s[80..84].copy_from_slice(&128u32.to_le_bytes());
        s[84..88].copy_from_slice(&15_488u32.to_le_bytes());
        s[88..92].copy_from_slice(&15_616u32.to_le_bytes());
        s[92..96].copy_from_slice(&1_952_066u32.to_le_bytes());
        s[100..104].copy_from_slice(&0x0B35_ECD9u32.to_le_bytes());
        s[104] = 0;
        s[105] = 1;
        s[106..108].copy_from_slice(&flags.to_le_bytes());
        s[108] = 9;
        s[109] = 8;
        s[110] = 1;
        s[112] = 81;
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }

    #[test]
    fn reads_an_exfat_boot_sector() {
        let boot = parse_boot_sector(&exfat_boot(0)).unwrap();
        assert_eq!(boot.fat_offset, None);
        let fs = boot.details;
        assert_eq!(fs.name, "exFAT");
        assert_eq!(fs.cluster_bytes, Some(128 * 1024));
        assert_eq!(fs.revision.as_deref(), Some("1.00"));
        assert_eq!(fs.dirty, Some(false));
        assert_eq!(fs.media_failure, Some(false));
        assert_eq!(fs.percent_in_use, Some(81));
        assert_eq!(fs.data_offset_bytes, Some(15_616 * 512));

        // Pulled out while writing, and bad sectors met.
        let fs = parse_boot_sector(&exfat_boot(0b110)).unwrap().details;
        assert_eq!((fs.dirty, fs.media_failure), (Some(true), Some(true)));
        // 0xFF: the writer didn't say how full it is.
        let mut unknown = exfat_boot(0);
        unknown[112] = 0xFF;
        assert_eq!(
            parse_boot_sector(&unknown).unwrap().details.percent_in_use,
            None
        );
    }

    fn fat32_boot() -> Vec<u8> {
        let mut s = vec![0u8; 512];
        s[11..13].copy_from_slice(&512u16.to_le_bytes());
        s[13] = 64; // 32 KiB clusters
        s[14..16].copy_from_slice(&32u16.to_le_bytes());
        s[16] = 2;
        s[32..36].copy_from_slice(&62_333_952u32.to_le_bytes());
        s[36..40].copy_from_slice(&7_607u32.to_le_bytes());
        s[82..90].copy_from_slice(b"FAT32   ");
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }

    #[test]
    fn reads_a_fat32_boot_sector_and_its_clean_and_error_bits() {
        let boot = parse_boot_sector(&fat32_boot()).unwrap();
        assert_eq!(boot.details.name, "FAT32");
        assert_eq!(boot.details.cluster_bytes, Some(32 * 1024));
        assert_eq!(boot.fat_offset, Some(32 * 512));
        assert_eq!(boot.details.data_offset_bytes, Some((32 + 2 * 7_607) * 512));

        let mut fat = vec![0u8; 512];
        fat[4..8].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
        assert_eq!(parse_fat_flags("FAT32", &fat), (Some(false), Some(false)));
        fat[4..8].copy_from_slice(&(0x0FFF_FFFFu32 & !0x0800_0000).to_le_bytes());
        assert_eq!(parse_fat_flags("FAT32", &fat), (Some(true), Some(false)));
        fat[2..4].copy_from_slice(&0x3FFFu16.to_le_bytes());
        assert_eq!(parse_fat_flags("FAT16", &fat), (Some(true), Some(true)));
        assert_eq!(parse_fat_flags("FAT12", &fat), (None, None));
    }

    #[test]
    fn rejects_sectors_that_are_not_boot_sectors() {
        assert!(parse_boot_sector(&[0u8; 512]).is_none());
        let mut no_signature = exfat_boot(0);
        no_signature[511] = 0;
        assert!(parse_boot_sector(&no_signature).is_none());
        let mut nonsense = fat32_boot();
        nonsense[11..13].copy_from_slice(&700u16.to_le_bytes());
        assert!(parse_boot_sector(&nonsense).is_none());
    }

    #[test]
    fn names_usb_links_and_what_the_reader_could_do() {
        assert_eq!(link_name(3, 0b0001), Some("USB 5 Gbps"));
        assert_eq!(link_name(3, 0b0101), Some("USB 10 Gbps or faster"));
        assert_eq!(link_name(2, 0), Some("USB 2.0 (480 Mbps)"));
        // A 5 Gbps reader in a USB 2.0 port.
        assert_eq!(link_name(2, 0b0010), Some("USB 2.0 (480 Mbps)"));
        assert_eq!(link_capable(0b0010, 0x0320), Some("USB 5 Gbps"));
        assert_eq!(link_capable(0b1010, 0x0320), Some("USB 10 Gbps or faster"));
        assert_eq!(link_capable(0, 0x0200), Some("USB 2.0 (480 Mbps)"));
        assert_eq!(usb_version(0x0320).as_deref(), Some("3.20"));
        assert_eq!(usb_version(0x0210).as_deref(), Some("2.10"));
        assert_eq!(usb_version(0), None);
    }
}
