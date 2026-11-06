//! Card health from the card's own NVMe health log (CFexpress cards are NVMe drives).
//!
//! - Windows: a card the reader shows as an NVMe drive (PCIe, Thunderbolt or USB4
//!   readers such as the Sony MRW-G3 on a USB4 port) is read directly, without smartctl
//!   or admin rights. Behind a JMicron JMS583 USB bridge (CFexpress Type B readers such
//!   as the CHIPFANCIER CFCR-II) the app passes the NVMe commands through the bridge
//!   itself; that needs admin rights, so Windows asks once per reading. Behind other
//!   USB bridges, smartctl reaches the log through the bridge chip's pass-through
//!   (`-d sntrealtek`, `sntjmicron`, `sntasmedia`), also with admin rights.
//! - macOS: smartctl can only read drives macOS sees as NVMe (e.g. in a Thunderbolt
//!   reader). Through a USB reader macOS offers no pass-through, so it can't be read.
//! - SD cards have no health log at all.

use crate::volume_info::VolumeInfo;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
use std::path::Path;

/// One reading of a card's NVMe health log.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CardHealth {
    pub model: Option<String>,
    /// The card's own serial number. Unlike the volume ID, formatting doesn't change it.
    pub serial: Option<String>,
    pub firmware: Option<String>,
    /// smartctl's overall verdict.
    pub passed: Option<bool>,
    /// Non-zero means the card itself flags a problem.
    pub critical_warning: Option<u64>,
    /// Percent of the card's rated write endurance used (can pass 100).
    pub percentage_used: Option<u64>,
    pub available_spare: Option<u64>,
    pub available_spare_threshold: Option<u64>,
    pub data_read_bytes: Option<u64>,
    pub data_written_bytes: Option<u64>,
    /// Read commands the card has completed over its life.
    pub host_read_commands: Option<u64>,
    /// Write commands the card has completed over its life.
    pub host_write_commands: Option<u64>,
    /// Minutes the card's controller has spent busy with commands.
    pub controller_busy_minutes: Option<u64>,
    pub power_cycles: Option<u64>,
    pub power_on_hours: Option<u64>,
    pub unsafe_shutdowns: Option<u64>,
    pub media_errors: Option<u64>,
    pub error_log_entries: Option<u64>,
    pub temperature_c: Option<u64>,
    /// Minutes spent above the card's warning temperature.
    pub warning_temp_minutes: Option<u64>,
    /// Minutes spent above the card's critical temperature.
    pub critical_temp_minutes: Option<u64>,
    /// The NVMe version the card's controller follows, e.g. "1.4".
    pub nvme_version: Option<String>,
    /// The reader's USB bridge chip the app read the card through, e.g. "JMicron JMS583".
    pub bridge: Option<String>,
    /// The smartctl device type that worked, e.g. "sntrealtek".
    pub device_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HealthOutcome {
    Read { health: Box<CardHealth> },
    Unavailable { reason: String },
}

/// NVMe counts data in units of 1000 × 512 bytes.
const DATA_UNIT_BYTES: u64 = 512_000;

/// Parses `smartctl -j -a` output. Fails when there's no NVMe health log in it,
/// with smartctl's own error message when it gave one.
pub fn parse_smartctl(bytes: &[u8]) -> Result<CardHealth, String> {
    let text = String::from_utf8_lossy(bytes);
    let json: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|_| "smartctl didn't return any data".to_string())?;
    let Some(log) = json.get("nvme_smart_health_information_log") else {
        let message = json["smartctl"]["messages"]
            .as_array()
            .and_then(|m| m.iter().find_map(|m| m["string"].as_str()))
            .unwrap_or("no NVMe health log in smartctl's answer");
        return Err(message.to_string());
    };
    let num = |key: &str| log.get(key).and_then(Value::as_u64);
    let text = |key: &str| {
        json.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Ok(CardHealth {
        model: text("model_name"),
        serial: text("serial_number"),
        firmware: text("firmware_version"),
        passed: json["smart_status"]["passed"].as_bool(),
        critical_warning: num("critical_warning"),
        percentage_used: num("percentage_used"),
        available_spare: num("available_spare"),
        available_spare_threshold: num("available_spare_threshold"),
        data_read_bytes: num("data_units_read").and_then(|u| u.checked_mul(DATA_UNIT_BYTES)),
        data_written_bytes: num("data_units_written").and_then(|u| u.checked_mul(DATA_UNIT_BYTES)),
        host_read_commands: num("host_reads"),
        host_write_commands: num("host_writes"),
        controller_busy_minutes: num("controller_busy_time"),
        power_cycles: num("power_cycles"),
        power_on_hours: num("power_on_hours"),
        unsafe_shutdowns: num("unsafe_shutdowns"),
        media_errors: num("media_errors"),
        error_log_entries: num("num_err_log_entries"),
        temperature_c: num("temperature"),
        warning_temp_minutes: num("warning_temp_time"),
        critical_temp_minutes: num("critical_comp_time"),
        nvme_version: json["nvme_version"]["string"].as_str().map(str::to_string),
        bridge: None,
        device_type: json["device"]["type"].as_str().map(str::to_string),
    })
}

/// Device types to try, in order: smartctl's own guess first, then the common
/// USB-to-NVMe bridge chips.
#[cfg_attr(not(windows), allow(dead_code))]
const DEVICE_TYPES: &[Option<&str>] = &[
    None,
    Some("sntrealtek"),
    Some("sntjmicron"),
    Some("sntasmedia"),
];

const SD_NOTE: &str = "SD cards keep no health log; only CFexpress cards do.";

/// Reads the health log of the card mounted at `root`. On Windows this may ask for
/// admin rights (a card behind a USB bridge).
pub fn read_health(root: &Path, volume: &VolumeInfo, smartctl: Option<&Path>) -> HealthOutcome {
    platform::read_health(root, volume, smartctl)
}

/// The argument Card Intake's own program answers when the app starts it with admin
/// rights to read a card through the reader's USB bridge.
const ADMIN_FLAG: &str = "--read-card-health-as-admin";

/// Call first thing in `main`. When this process is the copy the app started with admin
/// rights to read a card (`<program> --read-card-health-as-admin E: <answer.json>`), it
/// reads the card, writes the answer for the process that asked, and returns the exit
/// code. Otherwise returns `None`.
pub fn serve_admin_request(args: &[String]) -> Option<i32> {
    if args.get(1).map(String::as_str) != Some(ADMIN_FLAG) {
        return None;
    }
    let (Some(root), Some(answer_path)) = (args.get(2), args.get(3)) else {
        return Some(2);
    };
    let answer = match crate::storage::read_nvme_health(Path::new(root)) {
        Ok(health) => serde_json::json!({ "health": health }),
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    };
    // Only ever a new file: this copy runs with admin rights.
    let written = std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(answer_path)
        .and_then(|mut file| file.write_all(answer.to_string().as_bytes()));
    Some(if written.is_ok() { 0 } else { 1 })
}

/// The card health in the answer [`serve_admin_request`] wrote, or the reason there's none.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_admin_answer(text: &str) -> Result<CardHealth, String> {
    let unreadable = || "The admin helper's answer couldn't be read.".to_string();
    let json: Value = serde_json::from_str(text).map_err(|_| unreadable())?;
    if let Some(error) = json["error"].as_str() {
        return Err(error.to_string());
    }
    serde_json::from_value(json["health"].clone()).map_err(|_| unreadable())
}

/// Quotes one argument of a Windows command line so the program receives it unchanged:
/// backslashes count only before a quote, so those are doubled.
#[cfg_attr(not(windows), allow(dead_code))]
fn quote_arg(arg: &str) -> String {
    let mut quoted = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        let escaped = if c == '"' {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        quoted.extend(std::iter::repeat_n('\\', escaped));
        quoted.push(c);
        backslashes = 0;
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

fn run_smartctl(smartctl: &Path, device_type: Option<&str>, device: &str) -> Vec<u8> {
    let mut cmd = crate::process::command(smartctl);
    cmd.args(["-j", "-a"]);
    if let Some(t) = device_type {
        cmd.args(["-d", t]);
    }
    cmd.arg(device);
    cmd.output().map(|o| o.stdout).unwrap_or_default()
}

fn unavailable(reason: impl Into<String>) -> HealthOutcome {
    HealthOutcome::Unavailable {
        reason: reason.into(),
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    pub fn read_health(
        _root: &Path,
        volume: &VolumeInfo,
        smartctl: Option<&Path>,
    ) -> HealthOutcome {
        let reader = volume.reader.as_deref().unwrap_or("this reader");
        if volume.bus.as_deref() == Some("USB") {
            return unavailable(format!(
                "macOS can't read a card's health log through a USB reader such as the {reader}: \
                 it offers no way to pass the commands through. Read it on Windows, \
                 or in a Thunderbolt reader that shows the card as an NVMe drive."
            ));
        }
        let Some(device) = volume.device.as_deref() else {
            return unavailable("Couldn't tell which disk holds this card.");
        };
        let Some(smartctl) = smartctl else {
            return unavailable(
                "Needs smartctl. Install smartmontools (smartmontools.org), then try again.",
            );
        };
        match parse_smartctl(&run_smartctl(smartctl, None, &format!("/dev/{device}"))) {
            Ok(health) => HealthOutcome::Read {
                health: Box::new(health),
            },
            Err(e) => unavailable(format!("No health log came back ({e}). {SD_NOTE}")),
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use crate::storage::NvmeError;
    use std::fs;
    use std::mem::{size_of, zeroed};
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };

    pub fn read_health(root: &Path, volume: &VolumeInfo, smartctl: Option<&Path>) -> HealthOutcome {
        let read = |health| HealthOutcome::Read {
            health: Box::new(health),
        };
        // A card the reader shows as an NVMe drive answers directly: no smartctl, no
        // admin rights. So does one behind a JMicron bridge when the app runs as admin.
        match crate::storage::read_nvme_health(root) {
            Ok(health) => return read(health),
            // Behind a JMicron bridge: Windows asks for admin rights once.
            Err(NvmeError::NeedsAdmin { .. }) => {
                return match read_as_admin(root) {
                    Ok(health) => read(health),
                    Err(e) => unavailable(e),
                };
            }
            Err(NvmeError::Failed(_)) => {}
        }
        // An SD card has no log to find, so smartctl and admin rights would be wasted.
        if let Some(class) = crate::card_details::card_details(root).sd_class {
            return unavailable(format!(
                "This is an {class} card. {SD_NOTE} Its size class, lock switch and file \
                 system state are under Card details."
            ));
        }
        let Some(device) = volume.device.as_deref() else {
            return unavailable("Couldn't tell which drive letter holds this card.");
        };
        let Some(smartctl) = smartctl else {
            return unavailable(
                "This reader doesn't show the card as an NVMe drive, so reading its health log \
                 needs smartctl. Install smartmontools for Windows (smartmontools.org), then try again.",
            );
        };

        // Without admin rights first: this works when the app already runs as admin.
        let mut first_error = None;
        for t in DEVICE_TYPES {
            match parse_smartctl(&run_smartctl(smartctl, *t, device)) {
                Ok(mut health) => {
                    health.device_type = health.device_type.or(t.map(str::to_string));
                    return HealthOutcome::Read {
                        health: Box::new(health),
                    };
                }
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }

        // Then once with admin rights, trying every device type in one go (one UAC prompt).
        match run_elevated(smartctl, device) {
            Ok(outputs) => {
                for (t, out) in DEVICE_TYPES.iter().zip(outputs) {
                    if let Ok(mut health) = parse_smartctl(&out) {
                        health.device_type = health.device_type.or(t.map(str::to_string));
                        return HealthOutcome::Read {
                            health: Box::new(health),
                        };
                    }
                }
                unavailable(format!(
                    "No health log came back ({}). {SD_NOTE} Some readers don't pass it through.",
                    first_error.unwrap_or_default()
                ))
            }
            Err(e) => unavailable(e),
        }
    }

    /// Reads the card in a copy of this program that Windows starts with admin rights,
    /// after asking. The copy gets only the drive letter and a new file for its answer.
    fn read_as_admin(root: &Path) -> Result<CardHealth, String> {
        let program = std::env::current_exe()
            .map_err(|e| format!("Couldn't find Card Intake's own program file ({e})."))?;
        let drive: String = root.to_string_lossy().chars().take(2).collect();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let answer = std::env::temp_dir().join(format!(
            "card-intake-health-{}-{stamp}.json",
            std::process::id()
        ));
        let answer_text = answer.to_string_lossy();
        let params = [ADMIN_FLAG, drive.as_str(), answer_text.as_ref()]
            .map(quote_arg)
            .join(" ");
        let exit_code = run_as_admin(&program, &params);
        let text = fs::read_to_string(&answer);
        let _ = fs::remove_file(&answer);
        let exit_code = exit_code?;
        let text = text.map_err(|_| {
            format!("The admin helper stopped without an answer (exit code {exit_code}).")
        })?;
        parse_admin_answer(&text)
    }

    /// Starts `program` with admin rights (Windows asks first), hidden, and waits for it.
    /// Returns its exit code.
    fn run_as_admin(program: &Path, params: &str) -> Result<u32, String> {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        let wide = |s: &OsStr| s.encode_wide().chain(Some(0)).collect::<Vec<u16>>();
        let verb = wide(OsStr::new("runas"));
        let file = wide(program.as_os_str());
        let params = wide(OsStr::new(params));
        // SAFETY: all zeros is a valid SHELLEXECUTEINFOW (null pointers and handles).
        let mut info: SHELLEXECUTEINFOW = unsafe { zeroed() };
        info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = 0; // SW_HIDE
        // SAFETY: `info` is set up above, and the strings it points to outlive the call.
        if unsafe { ShellExecuteExW(&mut info) } == 0 {
            let e = std::io::Error::last_os_error();
            return Err(if e.raw_os_error() == Some(ERROR_CANCELLED as i32) {
                "Windows didn't give admin rights, so the card wasn't read. \
                 Try again and choose Yes."
                    .to_string()
            } else {
                format!("Couldn't start the admin helper ({e}).")
            });
        }
        if info.hProcess.is_null() {
            return Err("Couldn't start the admin helper.".into());
        }
        // SAFETY: `hProcess` is a process handle this function owns and closes once.
        let (waited, exit_code) = unsafe {
            let waited = WaitForSingleObject(info.hProcess, 60_000);
            let mut exit_code = 1u32;
            GetExitCodeProcess(info.hProcess, &mut exit_code);
            CloseHandle(info.hProcess);
            (waited, exit_code)
        };
        if waited != WAIT_OBJECT_0 {
            return Err("The admin helper didn't finish within a minute.".into());
        }
        Ok(exit_code)
    }

    /// Runs smartctl for each device type in one elevated batch file, so Windows asks
    /// for admin rights only once. Returns each attempt's JSON.
    fn run_elevated(smartctl: &Path, device: &str) -> Result<Vec<Vec<u8>>, String> {
        let dir = std::env::temp_dir().join(format!("card-intake-health-{}", std::process::id()));
        fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create a temp folder: {e}"))?;
        let outputs: Vec<_> = (0..DEVICE_TYPES.len())
            .map(|i| dir.join(format!("attempt{i}.json")))
            .collect();

        // chcp 65001 lets the batch file handle non-ASCII paths, e.g. a Chinese user name.
        let mut script = String::from("@echo off\r\nchcp 65001 >nul\r\n");
        for (t, out) in DEVICE_TYPES.iter().zip(&outputs) {
            let device_type = t.map(|t| format!(" -d {t}")).unwrap_or_default();
            script.push_str(&format!(
                "\"{}\" -j -a{device_type} {device} > \"{}\" 2>&1\r\n",
                smartctl.display(),
                out.display()
            ));
        }
        let batch = dir.join("read-health.cmd");
        fs::write(&batch, script).map_err(|e| format!("Couldn't write the helper script: {e}"))?;

        let batch_arg = batch.display().to_string().replace('\'', "''");
        let start = format!(
            "Start-Process -FilePath $env:ComSpec -ArgumentList '/c', '\"{batch_arg}\"' -Verb RunAs -Wait -WindowStyle Hidden"
        );
        let status = crate::process::command(Path::new("powershell"))
            .args(["-NoProfile", "-NonInteractive", "-Command", &start])
            .status();
        let result = match status {
            Ok(s) if s.success() => Ok(outputs
                .iter()
                .map(|p| fs::read(p).unwrap_or_default())
                .collect()),
            Ok(_) => Err(
                "Windows didn't give admin rights, so the health log wasn't read. \
                           Click Read card health again and choose Yes."
                    .to_string(),
            ),
            Err(e) => Err(format!("Couldn't start PowerShell: {e}")),
        };
        let _ = fs::remove_dir_all(&dir);
        result
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::*;

    pub fn read_health(
        _root: &Path,
        _volume: &VolumeInfo,
        _smartctl: Option<&Path>,
    ) -> HealthOutcome {
        unavailable("Reading card health isn't built for this system yet.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NVME: &str = r#"{
      "json_format_version": [1, 0],
      "smartctl": {"exit_status": 0},
      "device": {"name": "E:", "type": "sntrealtek", "protocol": "NVMe"},
      "model_name": "CEA-G240T",
      "serial_number": "ABC123456",
      "firmware_version": "1.00",
      "smart_status": {"passed": true, "nvme": {"value": 0}},
      "nvme_smart_health_information_log": {
        "critical_warning": 0,
        "temperature": 41,
        "available_spare": 100,
        "available_spare_threshold": 10,
        "percentage_used": 2,
        "data_units_read": 18164062,
        "data_units_written": 8984375,
        "host_reads": 61203411,
        "host_writes": 30411205,
        "power_cycles": 386,
        "power_on_hours": 97,
        "unsafe_shutdowns": 12,
        "media_errors": 0,
        "num_err_log_entries": 0
      }
    }"#;

    #[test]
    fn parses_an_nvme_health_log() {
        let h = parse_smartctl(NVME.as_bytes()).unwrap();
        assert_eq!(h.model.as_deref(), Some("CEA-G240T"));
        assert_eq!(h.serial.as_deref(), Some("ABC123456"));
        assert_eq!(h.passed, Some(true));
        assert_eq!(h.percentage_used, Some(2));
        assert_eq!(h.data_written_bytes, Some(8_984_375 * 512_000));
        assert_eq!(h.host_write_commands, Some(30_411_205));
        assert_eq!(h.power_cycles, Some(386));
        assert_eq!(h.temperature_c, Some(41));
        assert_eq!(h.device_type.as_deref(), Some("sntrealtek"));
    }

    #[test]
    fn reports_smartctls_error_when_there_is_no_log() {
        let json = br#"{"smartctl": {"exit_status": 2, "messages": [
            {"string": "E:: Unknown USB bridge [0x054c:0x0fb9 (0x2101)]", "severity": "error"}]}}"#;
        let err = parse_smartctl(json).unwrap_err();
        assert!(err.contains("Unknown USB bridge"), "{err}");
    }

    #[test]
    fn parses_smartctls_busy_time_temperature_times_and_nvme_version() {
        let json =
            br#"{"model_name": "CEB-G256T", "nvme_version": {"string": "1.4", "value": 66560},
            "nvme_smart_health_information_log": {"controller_busy_time": 412,
            "warning_temp_time": 3, "critical_comp_time": 0}}"#;
        let h = parse_smartctl(json).unwrap();
        assert_eq!(h.controller_busy_minutes, Some(412));
        assert_eq!(h.warning_temp_minutes, Some(3));
        assert_eq!(h.critical_temp_minutes, Some(0));
        assert_eq!(h.nvme_version.as_deref(), Some("1.4"));
        assert_eq!(h.bridge, None);
    }

    #[test]
    fn quotes_arguments_for_the_admin_helper() {
        // A trailing backslash would otherwise escape the closing quote.
        assert_eq!(quote_arg(r"E:\"), r#""E:\\""#);
        assert_eq!(quote_arg("E:"), r#""E:""#);
        assert_eq!(
            quote_arg(r"C:\Users\A B\AppData\Local\Temp\answer.json"),
            r#""C:\Users\A B\AppData\Local\Temp\answer.json""#
        );
        assert_eq!(quote_arg(r#"say \"hi""#), r#""say \\\"hi\"""#);
    }

    #[test]
    fn reads_the_admin_helpers_answer() {
        let health = CardHealth {
            serial: Some("ABC".into()),
            bridge: Some("JMicron JMS583".into()),
            ..CardHealth::default()
        };
        let answer = serde_json::json!({ "health": health }).to_string();
        assert_eq!(parse_admin_answer(&answer), Ok(health));
        let refused = r#"{"error": "The card didn't answer."}"#;
        assert_eq!(
            parse_admin_answer(refused).unwrap_err(),
            "The card didn't answer."
        );
        assert!(parse_admin_answer("").is_err());
    }

    #[test]
    fn only_the_admin_flag_starts_the_admin_helper() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(serve_admin_request(&args(&["app"])), None);
        assert_eq!(serve_admin_request(&args(&["app", "--health", "E:"])), None);
        assert_eq!(
            serve_admin_request(&args(&["app", ADMIN_FLAG, "E:"])),
            Some(2)
        );

        // It never overwrites a file, even one it could write.
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("answer.json");
        std::fs::write(&existing, "keep").unwrap();
        let path = existing.to_string_lossy().to_string();
        assert_eq!(
            serve_admin_request(&args(&["app", ADMIN_FLAG, "Z:", &path])),
            Some(1)
        );
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "keep");
    }

    #[test]
    fn rejects_non_json_and_tolerates_a_byte_order_mark() {
        assert!(parse_smartctl(b"Access is denied.").is_err());
        let with_bom = format!("\u{feff}{NVME}");
        assert!(parse_smartctl(with_bom.as_bytes()).is_ok());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn usb_readers_on_macos_are_explained() {
        let volume = VolumeInfo {
            bus: Some("USB".into()),
            reader: Some("MRW-G3".into()),
            device: Some("disk5".into()),
            ..VolumeInfo::default()
        };
        match read_health(Path::new("/Volumes/Untitled"), &volume, None) {
            HealthOutcome::Unavailable { reason } => {
                assert!(reason.contains("MRW-G3"), "{reason}");
                assert!(reason.contains("Windows"), "{reason}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }
}
