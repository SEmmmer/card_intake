//! What the operating system's storage stack says about the drive holding a card:
//! how it's connected, and — when the app can reach the card itself — the card's own
//! serial number and health log.
//!
//! A CFexpress card in a PCIe, Thunderbolt or USB4 reader (e.g. a Sony MRW-G3 on a
//! USB4 port, or a Nikon Z 9's Type B card in a Type B reader) is a plain NVMe drive to
//! Windows, whatever its maker. Its Identify data carries the card's model and a serial
//! number that formatting never changes, and its health log can be read with
//! `IOCTL_STORAGE_QUERY_PROPERTY`, without smartctl or admin rights.
//!
//! Behind a USB bridge Windows sees only the reader, and the serial it reports is the
//! reader's. Some bridge chips pass NVMe commands on to the card inside vendor SCSI
//! commands, which is how smartctl and CrystalDiskInfo read such cards. The app speaks
//! the JMicron JMS583's (in CFexpress Type B readers such as the CHIPFANCIER CFCR-II)
//! itself. Windows passes raw SCSI commands only to a drive opened for writing, so this
//! needs admin rights, although the commands only read from the card.

use crate::health::CardHealth;
use serde::Serialize;
use std::fmt;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DriveIdentity {
    /// How the drive is connected: "NVMe", "USB", "SD", ...
    pub bus: Option<String>,
    /// The drive's product name. When the app reaches the card itself, the card's own
    /// model, e.g. "CEA-G240T" or a Nikon Z 9's Type B card.
    pub product: Option<String>,
    /// The card's own serial number. Only set when the app reaches the card itself (on
    /// the NVMe bus, in an SD host controller, or through the reader's USB bridge), never
    /// the reader's serial.
    pub card_serial: Option<String>,
    /// The USB reader's vendor and product ID, e.g. "152D:0583".
    pub usb_id: Option<String>,
    /// The reader's USB-to-NVMe bridge chip when the app can talk to the card through
    /// it, e.g. "JMicron JMS583". The card is then a CFexpress card; reading its model,
    /// serial number and health log through the bridge takes admin rights.
    pub bridge: Option<String>,
}

/// Why the card's NVMe health log couldn't be read without asking for anything.
#[derive(Debug, Clone, PartialEq)]
pub enum NvmeError {
    /// The card is behind a USB bridge the app talks through, which takes admin rights.
    NeedsAdmin { bridge: String },
    /// The card's NVMe commands can't be reached from here, or the card didn't answer.
    Failed(String),
}

impl fmt::Display for NvmeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeedsAdmin { bridge } => write!(
                f,
                "Reading the card through the reader's {bridge} bridge needs admin rights."
            ),
            Self::Failed(reason) => f.write_str(reason),
        }
    }
}

/// NVMe counts data in units of 1000 × 512 bytes.
const DATA_UNIT_BYTES: u64 = 512_000;

const NVME_GET_LOG_PAGE: u8 = 0x02;
const NVME_IDENTIFY: u8 = 0x06;
/// Identify Controller (CNS 01h).
const CNS_CONTROLLER: u32 = 1;
/// The SMART / Health Information log.
const SMART_LOG: u32 = 0x02;
const SMART_LOG_LEN: usize = 512;
const IDENTIFY_LEN: usize = 4096;
/// The controller-wide health log rather than one namespace's.
const ALL_NAMESPACES: u32 = 0xFFFF_FFFF;

/// Parses an NVMe SMART / Health Information log page (log identifier 02h, 512 bytes).
pub fn parse_nvme_health_log(log: &[u8]) -> Option<CardHealth> {
    if log.len() < 192 {
        return None;
    }
    let u16_at = |at: usize| u16::from_le_bytes([log[at], log[at + 1]]) as u64;
    let u32_at = |at: usize| {
        log.get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as u64)
    };
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
        controller_busy_minutes: Some(u128_at(96)),
        power_cycles: Some(u128_at(112)),
        power_on_hours: Some(u128_at(128)),
        unsafe_shutdowns: Some(u128_at(144)),
        media_errors: Some(u128_at(160)),
        error_log_entries: Some(u128_at(176)),
        warning_temp_minutes: u32_at(192),
        critical_temp_minutes: u32_at(196),
        ..CardHealth::default()
    })
}

/// The NVMe version in an Identify Controller structure, e.g. "1.4". Controllers older
/// than NVMe 1.2 may leave it empty.
pub fn parse_nvme_version(data: &[u8]) -> Option<String> {
    let version = u32::from_le_bytes(data.get(80..84)?.try_into().ok()?);
    let (major, minor, tertiary) = (version >> 16, (version >> 8) & 0xFF, version & 0xFF);
    match (major, tertiary) {
        (0, _) => None,
        (_, 0) => Some(format!("{major}.{minor}")),
        _ => Some(format!("{major}.{minor}.{tertiary}")),
    }
}

/// The USB-to-NVMe bridge chips the app talks to cards through, by the reader's USB ID.
/// Only chips read with a real card are listed; smartctl is left to try other bridges.
pub fn nvme_bridge(vendor: u16, product: u16) -> Option<&'static str> {
    match (vendor, product) {
        // E.g. the CHIPFANCIER CFCR-II CFexpress Type B reader.
        (0x152D, 0x0583) => Some("JMicron JMS583"),
        _ => None,
    }
}

/// The vendor and product ID in a USB device's instance ID, such as
/// `USB\VID_152D&PID_0583\MSFT30CF2022198849FB`.
pub fn parse_usb_id(instance_id: &str) -> Option<(u16, u16)> {
    let id = instance_id.to_ascii_uppercase();
    let rest = id.strip_prefix("USB\\VID_")?;
    let vendor = u16::from_str_radix(rest.get(..4)?, 16).ok()?;
    let product = rest.get(4..)?.strip_prefix("&PID_")?.get(..4)?;
    Some((vendor, u16::from_str_radix(product, 16).ok()?))
}

/// JMicron's NVMe pass-through, as smartmontools' `sntjmicron` uses it. One NVMe
/// command takes three SCSI commands: the NVMe command goes to the bridge in a 512-byte
/// block, its data comes back, then its completion. All three use opcode A1h, which
/// other devices read as ATA PASS-THROUGH (12), so they only go to a known JMicron bridge.
#[cfg_attr(not(windows), allow(dead_code))]
mod jmicron {
    const ATA_PASS_THROUGH_12: u8 = 0xA1;
    /// Marks an NVMe admin command.
    const ADMIN: u8 = 0x80;
    pub const SEND_COMMAND: u8 = 0x0;
    pub const READ_DATA: u8 = 0x2;
    pub const READ_COMPLETION: u8 = 0xF;
    /// The size of the command and completion blocks.
    pub const BLOCK: usize = 512;
    const SIGNATURE: &[u8; 4] = b"NVME";

    /// The 12-byte SCSI command for one step, moving `len` bytes (big-endian in bytes 3
    /// to 5; byte 2 is reserved).
    pub fn cdb(step: u8, len: usize) -> [u8; 12] {
        let mut cdb = [0u8; 12];
        cdb[0] = ATA_PASS_THROUGH_12;
        cdb[1] = ADMIN | step;
        cdb[3..6].copy_from_slice(&(len as u32).to_be_bytes()[1..]);
        cdb
    }

    /// The block that carries an NVMe admin command to the bridge.
    pub fn command(opcode: u8, nsid: u32, cdw10: u32) -> [u8; BLOCK] {
        let mut block = [0u8; BLOCK];
        block[..4].copy_from_slice(SIGNATURE);
        block[8] = opcode;
        block[12..16].copy_from_slice(&nsid.to_le_bytes());
        block[48..52].copy_from_slice(&cdw10.to_le_bytes());
        block
    }

    /// Checks the completion block: `Ok` when the card carried out the command.
    pub fn check_completion(reply: &[u8]) -> Result<(), String> {
        if reply.len() < 24 || &reply[..4] != SIGNATURE {
            return Err("the bridge's answer isn't in JMicron's format".into());
        }
        // Dword 3 of the NVMe completion: the status is in bits 31:17.
        let status = u32::from_le_bytes(reply[20..24].try_into().unwrap()) >> 17;
        if status == 0 {
            Ok(())
        } else {
            Err(format!("the card answered with NVMe status {status:#x}"))
        }
    }
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

/// How the drive holding the card is connected and, when the app can reach the card
/// itself without asking, its own model and serial number. Never asks for admin rights.
pub fn identify(root: &Path) -> DriveIdentity {
    platform::identify(root)
}

/// Reads the card's NVMe health log, model and serial number without asking for
/// anything: straight from the drive when the reader shows the card as an NVMe drive
/// (PCIe, Thunderbolt or USB4 reader), or through the reader's USB bridge when this
/// process already has admin rights. [`NvmeError::NeedsAdmin`] says that asking for
/// admin rights would work (see [`crate::health::read_health`]).
pub fn read_nvme_health(root: &Path) -> Result<CardHealth, NvmeError> {
    platform::read_nvme_health(root)
}

/// The Windows storage helpers, shared with [`crate::card_details`].
#[cfg(windows)]
pub(crate) mod platform {
    use super::*;
    use std::fs::File;
    use std::io;
    use std::mem::{offset_of, size_of, zeroed};
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use std::ptr;
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_Device_IDW, CM_Get_Parent, CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
        SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
        SetupDiGetDeviceInterfaceDetailW,
    };
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::IscsiDisc::{
        IOCTL_SCSI_PASS_THROUGH, SCSI_IOCTL_DATA_IN, SCSI_IOCTL_DATA_OUT, SCSI_PASS_THROUGH,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        GUID_DEVINTERFACE_DISK, IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_QUERY_PROPERTY,
        NVMeDataTypeIdentify, NVMeDataTypeLogPage, ProtocolTypeNvme,
        StorageAdapterProtocolSpecificProperty, StorageDeviceProperty,
        StorageDeviceProtocolSpecificProperty,
    };

    pub(crate) const BUS_USB: u32 = 7;
    pub(crate) const BUS_SD: u32 = 12;
    pub(crate) const BUS_MMC: u32 = 13;
    pub(crate) const BUS_NVME: u32 = 17;

    pub(crate) fn bus_name(bus: u32) -> &'static str {
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
    pub(crate) fn open(path: &str, access: u32) -> io::Result<File> {
        File::options().access_mode(access).share_mode(3).open(path)
    }

    pub(crate) fn ioctl(
        file: &File,
        code: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> io::Result<usize> {
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

    pub(crate) fn u32_at(buf: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(buf[at..at + 4].try_into().unwrap())
    }

    /// The N of `\\.\PhysicalDriveN`, the drive holding a volume such as `E:\`.
    pub(crate) fn drive_number(root: &Path) -> io::Result<u32> {
        let text = root.to_string_lossy();
        let drive: String = text.chars().take(2).collect();
        if !drive.ends_with(':') {
            return Err(io::Error::other("not a drive letter"));
        }
        device_number(&open(&format!(r"\\.\{drive}"), 0)?)
    }

    fn device_number(device: &File) -> io::Result<u32> {
        let mut number = [0u8; 12];
        ioctl(device, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut number)?;
        Ok(u32_at(&number, 4))
    }

    pub(crate) fn drive_path(number: u32) -> String {
        format!(r"\\.\PhysicalDrive{number}")
    }

    pub(crate) fn wide_text(wide: &[u16]) -> String {
        let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
        String::from_utf16_lossy(&wide[..end])
    }

    /// The USB vendor and product ID of the reader holding disk `number`.
    fn usb_id(number: u32) -> Option<(u16, u16)> {
        usb_reader(number).map(|r| (r.vendor, r.product))
    }

    /// The USB device a card reader is to Windows.
    pub(crate) struct UsbReader {
        /// Its device node, for Configuration Manager calls.
        pub(crate) devinst: u32,
        pub(crate) vendor: u16,
        pub(crate) product: u16,
    }

    /// The USB reader holding disk `number`: the first USB device above the disk in
    /// Windows' device tree.
    pub(crate) fn usb_reader(number: u32) -> Option<UsbReader> {
        // SAFETY: the device information set is freed before returning. Every structure
        // handed to SetupAPI has its size set, and the detail buffer is as large as
        // SetupAPI asked for and aligned for the structure.
        unsafe {
            let set = SetupDiGetClassDevsW(
                &GUID_DEVINTERFACE_DISK,
                ptr::null(),
                ptr::null_mut(),
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            );
            if set == INVALID_HANDLE_VALUE as isize {
                return None;
            }
            let mut found = None;
            for index in 0.. {
                let mut interface: SP_DEVICE_INTERFACE_DATA = zeroed();
                interface.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
                if SetupDiEnumDeviceInterfaces(
                    set,
                    ptr::null(),
                    &GUID_DEVINTERFACE_DISK,
                    index,
                    &mut interface,
                ) == 0
                {
                    break;
                }
                let mut needed = 0u32;
                SetupDiGetDeviceInterfaceDetailW(
                    set,
                    &interface,
                    ptr::null_mut(),
                    0,
                    &mut needed,
                    ptr::null_mut(),
                );
                let needed = (needed as usize).max(size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>());
                let mut buffer = vec![0u64; needed.div_ceil(8)];
                let detail = buffer
                    .as_mut_ptr()
                    .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
                (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
                let mut info: SP_DEVINFO_DATA = zeroed();
                info.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;
                if SetupDiGetDeviceInterfaceDetailW(
                    set,
                    &interface,
                    detail,
                    needed as u32,
                    ptr::null_mut(),
                    &mut info,
                ) == 0
                {
                    continue;
                }
                let offset = offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
                let path = wide_text(std::slice::from_raw_parts(
                    ptr::addr_of!((*detail).DevicePath).cast::<u16>(),
                    (needed - offset) / 2,
                ));
                if open(&path, 0).and_then(|d| device_number(&d)).ok() == Some(number) {
                    found = usb_ancestor(info.DevInst);
                    break;
                }
            }
            SetupDiDestroyDeviceInfoList(set);
            found
        }
    }

    /// The first USB device above `devinst`: the reader itself, whether the disk hangs
    /// off it directly (UAS) or through a USB storage device (BOT).
    fn usb_ancestor(mut devinst: u32) -> Option<UsbReader> {
        for _ in 0..8 {
            let mut parent = 0;
            let mut id = [0u16; 400];
            // SAFETY: plain out-parameters, with the buffer's real length.
            unsafe {
                if CM_Get_Parent(&mut parent, devinst, 0) != CR_SUCCESS
                    || CM_Get_Device_IDW(parent, id.as_mut_ptr(), id.len() as u32, 0) != CR_SUCCESS
                {
                    return None;
                }
            }
            if let Some((vendor, product)) = parse_usb_id(&wide_text(&id)) {
                return Some(UsbReader {
                    devinst: parent,
                    vendor,
                    product,
                });
            }
            devinst = parent;
        }
        None
    }

    /// A SCSI pass-through request with room for its sense data and up to 4 KiB of data.
    #[repr(C)]
    struct PassThrough {
        header: SCSI_PASS_THROUGH,
        sense: [u8; 32],
        data: [u8; IDENTIFY_LEN],
    }

    /// Sends one SCSI command to the drive: `data` goes to the drive, or is filled from
    /// it when `to_host` is set.
    fn scsi(device: &File, cdb: &[u8], to_host: bool, data: &mut [u8]) -> io::Result<()> {
        // SAFETY: the request is plain integers and bytes, so all zeros is a valid value.
        let mut request: Box<PassThrough> = Box::new(unsafe { zeroed() });
        let len = data.len();
        if len > request.data.len() || cdb.len() > request.header.Cdb.len() {
            return Err(io::Error::other("SCSI request too large"));
        }
        let header = &mut request.header;
        header.Length = size_of::<SCSI_PASS_THROUGH>() as u16;
        header.CdbLength = cdb.len() as u8;
        header.SenseInfoLength = 32;
        header.DataIn = if to_host {
            SCSI_IOCTL_DATA_IN
        } else {
            SCSI_IOCTL_DATA_OUT
        } as u8;
        header.DataTransferLength = len as u32;
        header.TimeOutValue = 10;
        header.DataBufferOffset = offset_of!(PassThrough, data);
        header.SenseInfoOffset = offset_of!(PassThrough, sense) as u32;
        header.Cdb[..cdb.len()].copy_from_slice(cdb);
        if !to_host {
            request.data[..len].copy_from_slice(data);
        }
        let size = (offset_of!(PassThrough, data) + len) as u32;
        let buffer: *mut PassThrough = &mut *request;
        let mut returned = 0u32;
        // SAFETY: `buffer` points to a live request of at least `size` bytes, which
        // METHOD_BUFFERED lets serve as both the input and the output buffer.
        let ok = unsafe {
            DeviceIoControl(
                device.as_raw_handle(),
                IOCTL_SCSI_PASS_THROUGH,
                buffer.cast(),
                size,
                buffer.cast(),
                size,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if request.header.ScsiStatus != 0 {
            let sense = &request.sense;
            return Err(io::Error::other(format!(
                "SCSI status {:#04x}, sense {:x}/{:02x}/{:02x}",
                request.header.ScsiStatus,
                sense[2] & 0x0F,
                sense[12],
                sense[13]
            )));
        }
        if to_host {
            data.copy_from_slice(&request.data[..len]);
        }
        Ok(())
    }

    /// Runs one NVMe admin command that reads `data` from the card, through a JMicron
    /// bridge.
    fn jmicron_admin(
        device: &File,
        opcode: u8,
        nsid: u32,
        cdw10: u32,
        data: &mut [u8],
    ) -> io::Result<()> {
        let mut command = jmicron::command(opcode, nsid, cdw10);
        scsi(
            device,
            &jmicron::cdb(jmicron::SEND_COMMAND, command.len()),
            false,
            &mut command,
        )?;
        scsi(
            device,
            &jmicron::cdb(jmicron::READ_DATA, data.len()),
            true,
            data,
        )?;
        let mut reply = [0u8; jmicron::BLOCK];
        scsi(
            device,
            &jmicron::cdb(jmicron::READ_COMPLETION, reply.len()),
            true,
            &mut reply,
        )?;
        jmicron::check_completion(&reply).map_err(io::Error::other)
    }

    /// Opens the drive for raw SCSI commands. Windows passes them only on a handle opened
    /// for reading and writing, which takes admin rights; the commands only read.
    fn open_for_commands(path: &str) -> io::Result<File> {
        File::options()
            .read(true)
            .write(true)
            .share_mode(3)
            .open(path)
    }

    fn identify_through_bridge(device: &File) -> io::Result<Vec<u8>> {
        let mut data = vec![0u8; IDENTIFY_LEN];
        jmicron_admin(device, NVME_IDENTIFY, 0, CNS_CONTROLLER, &mut data)?;
        Ok(data)
    }

    /// Fills in the card's model, serial number, firmware and NVMe version from its
    /// Identify Controller data. False when the data holds no serial number.
    fn apply_identify(health: &mut CardHealth, data: &[u8]) -> bool {
        let Some((serial, model, firmware)) = parse_nvme_identify(data) else {
            return false;
        };
        health.serial = Some(serial);
        health.model = (!model.is_empty()).then_some(model);
        health.firmware = (!firmware.is_empty()).then_some(firmware);
        health.nvme_version = parse_nvme_version(data);
        true
    }

    pub(crate) struct Descriptor {
        pub(crate) bus: u32,
        pub(crate) product: Option<String>,
        pub(crate) serial: Option<String>,
        /// The device says its medium can be removed (SD and most card readers).
        pub(crate) removable: bool,
    }

    pub(crate) fn descriptor(device: &File) -> io::Result<Descriptor> {
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
            removable: out[10] != 0,
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
    pub(crate) fn open_drive(path: &str) -> io::Result<File> {
        open(path, 0)
    }

    pub fn identify(root: &Path) -> DriveIdentity {
        let Ok(number) = drive_number(root) else {
            return DriveIdentity::default();
        };
        let path = drive_path(number);
        let Ok(device) = open_drive(&path) else {
            return DriveIdentity::default();
        };
        let Ok(d) = descriptor(&device) else {
            return DriveIdentity::default();
        };
        let (mut usb, mut bridge) = (None, None);
        // The card's own Identify Controller: its model and serial, whatever the maker
        // and however Windows names the drive.
        let card = match d.bus {
            BUS_NVME => nvme_query(
                &device,
                StorageAdapterProtocolSpecificProperty,
                NVMeDataTypeIdentify,
                CNS_CONTROLLER,
                IDENTIFY_LEN,
            )
            .ok(),
            BUS_USB => {
                usb = usb_id(number);
                bridge = usb.and_then(|(vendor, product)| nvme_bridge(vendor, product));
                // Through the bridge only when this process already has admin rights;
                // otherwise the card waits for "Read card health", which asks.
                bridge
                    .and_then(|_| open_for_commands(&path).ok())
                    .and_then(|commands| identify_through_bridge(&commands).ok())
            }
            _ => None,
        }
        .and_then(|data| parse_nvme_identify(&data));
        let (product, card_serial) = identity(d.bus, d.product, d.serial, card);
        DriveIdentity {
            bus: Some(bus_name(d.bus).to_string()),
            product,
            card_serial,
            usb_id: usb.map(|(vendor, product)| format!("{vendor:04X}:{product:04X}")),
            bridge: bridge.map(str::to_string),
        }
    }

    /// The model and serial to show for the drive holding a card. When the app reaches
    /// the card itself (on the NVMe bus or through the reader's bridge), the card's own
    /// Identify strings win: a third-party CFexpress card then shows its own model (e.g.
    /// a Nikon Z 9's Type B card) rather than the name Windows gives the drive.
    fn identity(
        bus: u32,
        product: Option<String>,
        descriptor_serial: Option<String>,
        card: Option<(String, String, String)>,
    ) -> (Option<String>, Option<String>) {
        let (product, card_serial) = match card {
            Some((serial, model, _)) => {
                let model = (!model.is_empty()).then_some(model);
                (model.or(product), Some(serial))
            }
            None => {
                let serial = match bus {
                    // Windows' own "serial" for NVMe is the namespace EUI-64, which is
                    // also unique but less familiar.
                    BUS_NVME => descriptor_serial,
                    // An SD host controller reports the card's own product serial (from
                    // its CID).
                    BUS_SD | BUS_MMC => descriptor_serial,
                    // Behind USB the serial is the reader's.
                    _ => None,
                };
                (product, serial)
            }
        };
        (
            product,
            card_serial.map(|s| s.trim_end_matches('.').to_string()),
        )
    }

    pub fn read_nvme_health(root: &Path) -> Result<CardHealth, NvmeError> {
        let failed = |what: &str, e: io::Error| NvmeError::Failed(format!("{what} ({e})."));
        let number = drive_number(root).map_err(|e| failed("Couldn't find the card's drive", e))?;
        let path = drive_path(number);
        let device = open_drive(&path).map_err(|e| failed("Couldn't open the card's drive", e))?;
        let d = descriptor(&device).map_err(|e| failed("Couldn't query the card's drive", e))?;
        match d.bus {
            BUS_NVME => read_native(&device, d.product),
            BUS_USB => {
                match usb_id(number).and_then(|(vendor, product)| nvme_bridge(vendor, product)) {
                    Some(bridge) => read_through_bridge(&path, bridge),
                    None => Err(NvmeError::Failed(
                        "The reader's USB bridge isn't one the app can talk to the card through."
                            .into(),
                    )),
                }
            }
            bus => Err(NvmeError::Failed(format!(
                "The card is on the {} bus, not NVMe.",
                bus_name(bus)
            ))),
        }
    }

    fn read_native(device: &File, product: Option<String>) -> Result<CardHealth, NvmeError> {
        let log = nvme_query(
            device,
            StorageDeviceProtocolSpecificProperty,
            NVMeDataTypeLogPage,
            SMART_LOG,
            SMART_LOG_LEN,
        )
        .map_err(|e| NvmeError::Failed(format!("The card didn't return its health log ({e}).")))?;
        let mut health = parse_nvme_health_log(&log)
            .ok_or_else(|| NvmeError::Failed("The card's health log was too short.".into()))?;
        let identified = nvme_query(
            device,
            StorageAdapterProtocolSpecificProperty,
            NVMeDataTypeIdentify,
            CNS_CONTROLLER,
            IDENTIFY_LEN,
        )
        .is_ok_and(|data| apply_identify(&mut health, &data));
        if !identified {
            health.model = product;
        }
        Ok(health)
    }

    fn read_through_bridge(path: &str, bridge: &str) -> Result<CardHealth, NvmeError> {
        let device = open_for_commands(path).map_err(|e| match e.kind() {
            io::ErrorKind::PermissionDenied => NvmeError::NeedsAdmin {
                bridge: bridge.to_string(),
            },
            _ => NvmeError::Failed(format!("Couldn't open the card's drive ({e}).")),
        })?;
        let through = |what: &str, e: io::Error| {
            NvmeError::Failed(format!(
                "The card didn't {what} through the reader's {bridge} bridge ({e})."
            ))
        };
        let identify = identify_through_bridge(&device).map_err(|e| through("answer", e))?;
        let mut log = [0u8; SMART_LOG_LEN];
        let cdw10 = SMART_LOG | (((SMART_LOG_LEN / 4 - 1) as u32) << 16);
        jmicron_admin(&device, NVME_GET_LOG_PAGE, ALL_NAMESPACES, cdw10, &mut log)
            .map_err(|e| through("return its health log", e))?;
        let mut health = parse_nvme_health_log(&log)
            .ok_or_else(|| NvmeError::Failed("The card's health log was too short.".into()))?;
        apply_identify(&mut health, &identify);
        health.bridge = Some(bridge.to_string());
        Ok(health)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::storage::tests::identify_of;

        #[test]
        fn a_third_party_cfexpress_card_shows_its_own_model_and_serial() {
            // A Nikon Z 9's CFexpress Type B card on the NVMe bus: the card's own
            // model and serial, not the generic name Windows gives the drive.
            let (product, serial) = identity(
                BUS_NVME,
                Some("NVMe Disk".into()),
                Some("eui.0025385b61b0a1c4".into()),
                Some(identify_of("602123456789", "SDCFE-256G", "6203")),
            );
            assert_eq!(product.as_deref(), Some("SDCFE-256G"));
            assert_eq!(serial.as_deref(), Some("602123456789"));
        }

        #[test]
        fn a_card_whose_identify_fails_keeps_what_windows_reports() {
            let (product, serial) = identity(
                BUS_NVME,
                Some("CEA-G240T".into()),
                Some("EUI-64".into()),
                None,
            );
            assert_eq!(product.as_deref(), Some("CEA-G240T"));
            assert_eq!(serial.as_deref(), Some("EUI-64"));
        }

        #[test]
        fn behind_usb_and_on_sd_the_card_is_not_mistaken_for_the_bus_device() {
            // A reader's serial says nothing about the card inside it.
            let (product, serial) = identity(
                BUS_USB,
                Some("MRW-G3".into()),
                Some("reader-serial".into()),
                None,
            );
            assert_eq!(product.as_deref(), Some("MRW-G3"));
            assert_eq!(serial, None);

            // An SD card's CID serial is the card's own.
            let (_, serial) = identity(BUS_SD, Some("SD Card".into()), Some("CID-1".into()), None);
            assert_eq!(serial.as_deref(), Some("CID-1"));
        }

        #[test]
        fn a_card_read_through_the_readers_bridge_shows_its_own_model_and_serial() {
            let (product, serial) = identity(
                BUS_USB,
                Some("CHIPFANCIER".into()),
                Some("CF2022198849FB".into()),
                Some(identify_of("602123456789", "SDCFE-256G", "6203")),
            );
            assert_eq!(product.as_deref(), Some("SDCFE-256G"));
            assert_eq!(serial.as_deref(), Some("602123456789"));
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub fn identify(_root: &Path) -> DriveIdentity {
        DriveIdentity::default()
    }

    pub fn read_nvme_health(_root: &Path) -> Result<CardHealth, NvmeError> {
        Err(NvmeError::Failed(
            "Reading the health log directly is built for Windows only.".into(),
        ))
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
        let (serial, model, fw) = identify_of("ABC123456", "CEA-G240T", "X2J0R100");
        assert_eq!(
            (serial.as_str(), model.as_str(), fw.as_str()),
            ("ABC123456", "CEA-G240T", "X2J0R100")
        );
        assert!(parse_nvme_identify(&[0u8; 4096]).is_none());
    }

    #[test]
    fn parses_the_time_and_temperature_counters_and_the_nvme_version() {
        let mut log = [0u8; 512];
        log[96..104].copy_from_slice(&1_234u64.to_le_bytes());
        log[192..196].copy_from_slice(&7u32.to_le_bytes());
        log[196..200].copy_from_slice(&1u32.to_le_bytes());
        let h = parse_nvme_health_log(&log).unwrap();
        assert_eq!(h.controller_busy_minutes, Some(1_234));
        assert_eq!(h.warning_temp_minutes, Some(7));
        assert_eq!(h.critical_temp_minutes, Some(1));
        // A log cut short before the temperature times still reads.
        assert_eq!(
            parse_nvme_health_log(&log[..192])
                .unwrap()
                .warning_temp_minutes,
            None
        );

        let mut identify = identify_bytes("ABC", "CEB-G256T", "1.00");
        assert_eq!(parse_nvme_version(&identify), None);
        identify[80..84].copy_from_slice(&0x0001_0400u32.to_le_bytes());
        assert_eq!(parse_nvme_version(&identify).as_deref(), Some("1.4"));
        identify[80..84].copy_from_slice(&0x0002_0001u32.to_le_bytes());
        assert_eq!(parse_nvme_version(&identify).as_deref(), Some("2.0.1"));
    }

    #[test]
    fn finds_the_readers_usb_id_and_knows_only_tested_bridges() {
        assert_eq!(
            parse_usb_id(r"USB\VID_152D&PID_0583\MSFT30CF2022198849FB"),
            Some((0x152D, 0x0583))
        );
        assert_eq!(
            parse_usb_id(r"usb\vid_054c&pid_0fb9&mi_00\7&1"),
            Some((0x054C, 0x0FB9))
        );
        assert_eq!(parse_usb_id(r"USBSTOR\Disk&Ven_Sony&Prod_MRW-G3\1"), None);
        assert_eq!(
            parse_usb_id(r"SCSI\DISK&VEN_CFCR-II&PROD_CHIPFANCIER\9&3"),
            None
        );
        assert_eq!(parse_usb_id(r"USB\VID_15"), None);

        assert_eq!(nvme_bridge(0x152D, 0x0583), Some("JMicron JMS583"));
        // Sony's MRW-G3 behind its USB bridge: left to smartctl.
        assert_eq!(nvme_bridge(0x054C, 0x0FB9), None);
    }

    #[test]
    fn builds_jmicron_commands_as_smartctl_does() {
        // Step 1 sends a 512-byte block; the length is big-endian in bytes 3 to 5.
        assert_eq!(
            jmicron::cdb(jmicron::SEND_COMMAND, 512),
            [0xA1, 0x80, 0x00, 0x00, 0x02, 0x00, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            jmicron::cdb(jmicron::READ_DATA, 4096)[..6],
            [0xA1, 0x82, 0, 0x00, 0x10, 0x00]
        );
        assert_eq!(jmicron::cdb(jmicron::READ_COMPLETION, 512)[1], 0x8F);

        // Get Log Page for the SMART log, all namespaces, 128 dwords.
        let block = jmicron::command(0x02, 0xFFFF_FFFF, 0x007F_0002);
        assert_eq!(&block[..4], b"NVME");
        assert_eq!(block[8], 0x02);
        assert_eq!(block[12..16], [0xFF; 4]);
        assert_eq!(block[48..52], [0x02, 0x00, 0x7F, 0x00]);
        assert!(block[52..].iter().all(|&b| b == 0));
    }

    #[test]
    fn reads_the_status_of_a_jmicron_completion() {
        let mut reply = [0u8; 512];
        reply[..4].copy_from_slice(b"NVME");
        // The phase bit (16) alone means success.
        reply[20..24].copy_from_slice(&(1u32 << 16).to_le_bytes());
        assert_eq!(jmicron::check_completion(&reply), Ok(()));
        // Invalid Field in Command (status code 02h).
        reply[20..24].copy_from_slice(&(0x02u32 << 17).to_le_bytes());
        assert!(
            jmicron::check_completion(&reply)
                .unwrap_err()
                .contains("0x2")
        );
        // Something that isn't a JMicron bridge's answer.
        assert!(jmicron::check_completion(&[0u8; 512]).is_err());
    }

    /// The fixed-width text fields of an Identify Controller structure, with a trailing
    /// NUL as drives write them.
    fn identify_bytes(serial: &str, model: &str, firmware: &str) -> Vec<u8> {
        fn field(text: &str, width: usize) -> Vec<u8> {
            let mut out = vec![b' '; width];
            let bytes = text.as_bytes();
            let n = bytes.len().min(width);
            out[..n].copy_from_slice(&bytes[..n]);
            if n < width {
                out[n] = 0;
            }
            out
        }
        let mut data = vec![0u8; 4096];
        data[4..24].copy_from_slice(&field(serial, 20));
        data[24..64].copy_from_slice(&field(model, 40));
        data[64..72].copy_from_slice(&field(firmware, 8));
        data
    }

    /// What the app reads out of an NVMe Identify Controller answer.
    pub(crate) fn identify_of(
        serial: &str,
        model: &str,
        firmware: &str,
    ) -> (String, String, String) {
        parse_nvme_identify(&identify_bytes(serial, model, firmware)).unwrap()
    }
}
