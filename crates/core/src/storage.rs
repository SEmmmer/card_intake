//! What the operating system's storage stack says about the drive holding a card:
//! how it's connected, and — when the card itself is on the bus rather than behind a
//! USB bridge — the card's own serial number and health log.
//!
//! A CFexpress card in a PCIe, Thunderbolt or USB4 reader (e.g. a Sony MRW-G3 on a
//! USB4 port) is a plain NVMe drive to Windows. Its Identify data carries a serial
//! number that formatting never changes, and its health log can be read with
//! `IOCTL_STORAGE_QUERY_PROPERTY`, without smartctl. Behind a USB bridge, the serial
//! Windows reports is the reader's, so it can't identify the card.

use crate::health::CardHealth;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DriveIdentity {
    /// How the drive is connected: "NVMe", "USB", "SD", ...
    pub bus: Option<String>,
    /// The drive's product name, e.g. "CEA-G240T".
    pub product: Option<String>,
    /// The card's own serial number. Only set when the card itself is on the bus
    /// (NVMe, SD host controller), never for the reader behind a USB bridge.
    pub card_serial: Option<String>,
}

/// NVMe counts data in units of 1000 × 512 bytes.
const DATA_UNIT_BYTES: u64 = 512_000;

/// Parses an NVMe SMART / Health Information log page (log identifier 02h, 512 bytes).
pub fn parse_nvme_health_log(log: &[u8]) -> Option<CardHealth> {
    if log.len() < 192 {
        return None;
    }
    let u16_at = |at: usize| u16::from_le_bytes([log[at], log[at + 1]]) as u64;
    // The counters are 128-bit; anything past 64 bits is saturated.
    let u128_at = |at: usize| {
        let low = u64::from_le_bytes(log[at..at + 8].try_into().unwrap());
        let high = u64::from_le_bytes(log[at + 8..at + 16].try_into().unwrap());
        if high == 0 { low } else { u64::MAX }
    };
    let critical_warning = log[0] as u64;
    let kelvin = u16_at(1);
    Some(CardHealth {
        passed: Some(critical_warning == 0),
        critical_warning: Some(critical_warning),
        temperature_c: kelvin.checked_sub(273).filter(|_| kelvin != 0),
        available_spare: Some(log[3] as u64),
        available_spare_threshold: Some(log[4] as u64),
        percentage_used: Some(log[5] as u64),
        data_read_bytes: u128_at(32).checked_mul(DATA_UNIT_BYTES),
        data_written_bytes: u128_at(48).checked_mul(DATA_UNIT_BYTES),
        host_read_commands: Some(u128_at(64)),
        host_write_commands: Some(u128_at(80)),
        power_cycles: Some(u128_at(112)),
        power_on_hours: Some(u128_at(128)),
        unsafe_shutdowns: Some(u128_at(144)),
        media_errors: Some(u128_at(160)),
        error_log_entries: Some(u128_at(176)),
        ..CardHealth::default()
    })
}

/// Serial number, model and firmware from an NVMe Identify Controller structure.
pub fn parse_nvme_identify(data: &[u8]) -> Option<(String, String, String)> {
    if data.len() < 72 {
        return None;
    }
    let text = |range: std::ops::Range<usize>| {
        String::from_utf8_lossy(&data[range])
            .trim_matches(|c: char| c == '\0' || c.is_whitespace())
            .to_string()
    };
    let serial = text(4..24);
    (!serial.is_empty()).then(|| (serial, text(24..64), text(64..72)))
}

pub fn identify(root: &Path) -> DriveIdentity {
    platform::identify(root)
}

/// Reads the NVMe health log straight from the drive. Works when the card is a native
/// NVMe drive (PCIe, Thunderbolt or USB4 reader); fails behind a USB bridge.
pub fn read_nvme_health(root: &Path) -> Result<CardHealth, String> {
    platform::read_nvme_health(root)
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::fs::File;
    use std::io;
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_QUERY_PROPERTY, NVMeDataTypeIdentify,
        NVMeDataTypeLogPage, ProtocolTypeNvme, StorageAdapterProtocolSpecificProperty,
        StorageDeviceProperty, StorageDeviceProtocolSpecificProperty,
    };

    const BUS_USB: u32 = 7;
    const BUS_SD: u32 = 12;
    const BUS_MMC: u32 = 13;
    const BUS_NVME: u32 = 17;

    fn bus_name(bus: u32) -> &'static str {
        match bus {
            1 => "SCSI",
            3 => "ATA",
            BUS_USB => "USB",
            10 => "SAS",
            11 => "SATA",
            BUS_SD => "SD",
            BUS_MMC => "MMC",
            BUS_NVME => "NVMe",
            19 => "UFS",
            _ => "other",
        }
    }

    /// Opens a device for queries only. Storage property queries need no read or
    /// write access, so this works without admin rights.
    fn open(path: &str, access: u32) -> io::Result<File> {
        File::options().access_mode(access).share_mode(3).open(path)
    }

    fn ioctl(file: &File, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<usize> {
        let mut returned = 0u32;
        // SAFETY: the handle is valid for the life of `file`; the buffers are live and
        // their lengths are passed alongside them.
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                code,
                input.as_ptr().cast(),
                input.len() as u32,
                output.as_mut_ptr().cast(),
                output.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(returned as usize)
        }
    }

    fn u32_at(buf: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(buf[at..at + 4].try_into().unwrap())
    }

    /// `\\.\PhysicalDriveN` for the drive holding a volume such as `E:\`.
    fn physical_drive(root: &Path) -> io::Result<String> {
        let text = root.to_string_lossy();
        let drive: String = text.chars().take(2).collect();
        if !drive.ends_with(':') {
            return Err(io::Error::other("not a drive letter"));
        }
        let volume = open(&format!(r"\\.\{drive}"), 0)?;
        let mut number = [0u8; 12];
        ioctl(&volume, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut number)?;
        Ok(format!(r"\\.\PhysicalDrive{}", u32_at(&number, 4)))
    }

    struct Descriptor {
        bus: u32,
        product: Option<String>,
        serial: Option<String>,
    }

    fn descriptor(device: &File) -> io::Result<Descriptor> {
        let mut query = [0u8; 12];
        query[0..4].copy_from_slice(&(StorageDeviceProperty as u32).to_le_bytes());
        let mut out = vec![0u8; 1024];
        let len = ioctl(device, IOCTL_STORAGE_QUERY_PROPERTY, &query, &mut out)?;
        if len < 36 {
            return Err(io::Error::other("short device descriptor"));
        }
        let text = |offset: u32| {
            let start = offset as usize;
            if start == 0 || start >= len {
                return None;
            }
            let end = out[start..len]
                .iter()
                .position(|&b| b == 0)
                .map_or(len, |p| start + p);
            let s = String::from_utf8_lossy(&out[start..end]).trim().to_string();
            (!s.is_empty()).then_some(s)
        };
        Ok(Descriptor {
            bus: u32_at(&out, 28),
            product: text(u32_at(&out, 16)),
            serial: text(u32_at(&out, 24)),
        })
    }

    /// An NVMe protocol-specific query: Identify (CNS) or Get Log Page.
    fn nvme_query(
        device: &File,
        property: i32,
        data_type: i32,
        request: u32,
        length: usize,
    ) -> io::Result<Vec<u8>> {
        // STORAGE_PROPERTY_QUERY header (8 bytes), then STORAGE_PROTOCOL_SPECIFIC_DATA
        // (40 bytes), then the data itself.
        const HEADER: usize = 8;
        const SPECIFIC: usize = 40;
        let mut buf = vec![0u8; HEADER + SPECIFIC + length];
        let mut put = |at: usize, v: u32| buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put(0, property as u32);
        put(4, 0); // PropertyStandardQuery
        put(8, ProtocolTypeNvme as u32);
        put(12, data_type as u32);
        put(16, request);
        put(20, 0);
        put(24, SPECIFIC as u32); // data follows the protocol-specific header
        put(28, length as u32);
        let input = buf.clone();
        let returned = ioctl(device, IOCTL_STORAGE_QUERY_PROPERTY, &input, &mut buf)?;
        // The answer is a STORAGE_PROTOCOL_DATA_DESCRIPTOR: Version, Size, then the
        // protocol-specific header with the data's offset and length.
        if returned < HEADER + SPECIFIC {
            return Err(io::Error::other("short NVMe answer"));
        }
        let offset = u32_at(&buf, 8 + 16) as usize;
        let len = u32_at(&buf, 8 + 20) as usize;
        let start = 8 + offset;
        if len == 0 || start + len > buf.len() {
            return Err(io::Error::other("the drive returned no NVMe data"));
        }
        Ok(buf[start..start + len].to_vec())
    }

    /// Opens the physical drive for queries, falling back to read access, which some
    /// drivers want for protocol queries (and which needs admin rights).
    fn open_drive(path: &str) -> io::Result<File> {
        open(path, 0)
    }

    pub fn identify(root: &Path) -> DriveIdentity {
        let Ok(path) = physical_drive(root) else {
            return DriveIdentity::default();
        };
        let Ok(device) = open_drive(&path) else {
            return DriveIdentity::default();
        };
        let Ok(d) = descriptor(&device) else {
            return DriveIdentity::default();
        };
        let card_serial = match d.bus {
            // The NVMe serial number from Identify Controller; Windows' own "serial"
            // for NVMe is the namespace EUI-64, which is also unique but less familiar.
            BUS_NVME => nvme_query(
                &device,
                StorageAdapterProtocolSpecificProperty,
                NVMeDataTypeIdentify,
                1, // CNS 01h: Identify Controller
                4096,
            )
            .ok()
            .and_then(|data| parse_nvme_identify(&data))
            .map(|(serial, _, _)| serial)
            .or(d.serial.clone()),
            // An SD host controller reports the card's own product serial (from its CID).
            BUS_SD | BUS_MMC => d.serial.clone(),
            // Behind USB the serial is the reader's.
            _ => None,
        };
        DriveIdentity {
            bus: Some(bus_name(d.bus).to_string()),
            product: d.product,
            card_serial: card_serial.map(|s| s.trim_end_matches('.').to_string()),
        }
    }

    pub fn read_nvme_health(root: &Path) -> Result<CardHealth, String> {
        let path =
            physical_drive(root).map_err(|e| format!("Couldn't find the card's drive ({e})."))?;
        let device =
            open_drive(&path).map_err(|e| format!("Couldn't open the card's drive ({e})."))?;
        let d =
            descriptor(&device).map_err(|e| format!("Couldn't query the card's drive ({e})."))?;
        if d.bus != BUS_NVME {
            return Err(format!(
                "the card is on the {} bus, not NVMe",
                bus_name(d.bus)
            ));
        }
        let log = nvme_query(
            &device,
            StorageDeviceProtocolSpecificProperty,
            NVMeDataTypeLogPage,
            2,
            512,
        )
        .map_err(|e| format!("The card didn't return its health log ({e})."))?;
        let mut health =
            parse_nvme_health_log(&log).ok_or("The card's health log was too short.")?;
        if let Some((serial, model, firmware)) = nvme_query(
            &device,
            StorageAdapterProtocolSpecificProperty,
            NVMeDataTypeIdentify,
            1,
            4096,
        )
        .ok()
        .and_then(|data| parse_nvme_identify(&data))
        {
            health.serial = Some(serial);
            health.model = (!model.is_empty()).then_some(model);
            health.firmware = (!firmware.is_empty()).then_some(firmware);
        } else {
            health.model = d.product;
        }
        Ok(health)
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub fn identify(_root: &Path) -> DriveIdentity {
        DriveIdentity::default()
    }

    pub fn read_nvme_health(_root: &Path) -> Result<CardHealth, String> {
        Err("reading the health log directly is built for Windows only".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_nvme_health_log() {
        let mut log = [0u8; 512];
        log[0] = 0; // critical warning
        log[1..3].copy_from_slice(&314u16.to_le_bytes()); // 41 °C
        log[3] = 100;
        log[4] = 10;
        log[5] = 2;
        log[32..40].copy_from_slice(&18_164_062u64.to_le_bytes());
        log[48..56].copy_from_slice(&8_984_375u64.to_le_bytes());
        log[112..120].copy_from_slice(&386u64.to_le_bytes());
        log[144..152].copy_from_slice(&12u64.to_le_bytes());
        let h = parse_nvme_health_log(&log).unwrap();
        assert_eq!(h.passed, Some(true));
        assert_eq!(h.temperature_c, Some(41));
        assert_eq!(h.percentage_used, Some(2));
        assert_eq!(h.available_spare, Some(100));
        assert_eq!(h.data_written_bytes, Some(8_984_375 * 512_000));
        assert_eq!(h.data_read_bytes, Some(18_164_062 * 512_000));
        assert_eq!(h.power_cycles, Some(386));
        assert_eq!(h.unsafe_shutdowns, Some(12));
        assert_eq!(h.media_errors, Some(0));
    }

    #[test]
    fn a_critical_warning_fails_the_card_and_short_logs_are_rejected() {
        let mut log = [0u8; 512];
        log[0] = 0x04;
        assert_eq!(parse_nvme_health_log(&log).unwrap().passed, Some(false));
        assert!(parse_nvme_health_log(&log[..100]).is_none());
    }

    #[test]
    fn parses_identify_controller_strings() {
        let mut data = [0u8; 4096];
        data[4..24].copy_from_slice(b"ABC123456           ");
        data[24..33].copy_from_slice(b"CEA-G240T");
        data[33..64].fill(b' ');
        data[64..72].copy_from_slice(b"X2J0R100");
        let (serial, model, fw) = parse_nvme_identify(&data).unwrap();
        assert_eq!(
            (serial.as_str(), model.as_str(), fw.as_str()),
            ("ABC123456", "CEA-G240T", "X2J0R100")
        );
        assert!(parse_nvme_identify(&[0u8; 4096]).is_none());
    }
}
