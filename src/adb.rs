//! Phones on a USB cable, through adb (USB debugging on the phone).
//!
//! Galdr forwards a local TCP port to the phone's control port and asks for the audio on that
//! same connection (`HELLO` with media port 0, see docs/PROTOCOL.md): adb carries TCP only.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};

/// A phone adb sees on USB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDevice {
    pub serial: String,
    /// From `adb devices -l`, e.g. "Pixel 7".
    pub model: String,
    /// False until the phone's "Allow USB debugging?" prompt is accepted.
    pub authorized: bool,
}

/// adb next to Galdr, on PATH, or in the Android SDK.
fn adb() -> Option<&'static PathBuf> {
    static ADB: OnceLock<Option<PathBuf>> = OnceLock::new();
    ADB.get_or_init(|| {
        let exe = if cfg!(windows) { "adb.exe" } else { "adb" };
        let beside = std::env::current_exe()
            .ok()
            .and_then(|p| Some(p.parent()?.join(exe)));
        let sdk = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
            .iter()
            .filter_map(std::env::var_os)
            .map(|d| PathBuf::from(d).join("platform-tools").join(exe));
        let path = std::env::var_os("PATH")
            .map(|p| {
                std::env::split_paths(&p)
                    .map(|d| d.join(exe))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        beside
            .into_iter()
            .chain(sdk)
            .chain(path)
            .find(|p| p.is_file())
    })
    .as_ref()
}

/// True when adb was found, i.e. USB phones can show up at all.
pub fn available() -> bool {
    adb().is_some()
}

fn run(args: &[&str]) -> Result<String> {
    let adb = adb().context("adb not found (needed for USB, see the README)")?;
    let mut cmd = Command::new(adb);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW); // no console flashing up from the GUI
    }
    let out = cmd.output().context("cannot run adb")?;
    if !out.status.success() {
        bail!(
            "adb {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Phones connected by cable. Wireless debugging is left out: that is Wi-Fi again.
pub fn devices() -> Vec<UsbDevice> {
    run(&["devices", "-l"])
        .map(|out| parse_devices(&out))
        .unwrap_or_default()
}

fn parse_devices(out: &str) -> Vec<UsbDevice> {
    out.lines()
        .skip_while(|l| !l.starts_with("List of devices"))
        .skip(1)
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let serial = words.next()?;
            let state = words.next()?;
            if serial.contains(':') || serial.contains("._adb-tls-") {
                return None;
            }
            let model = words
                .find_map(|w| w.strip_prefix("model:"))
                .map(|m| m.replace('_', " "))
                .unwrap_or_else(|| serial.to_string());
            match state {
                "device" | "unauthorized" => Some(UsbDevice {
                    serial: serial.into(),
                    model,
                    authorized: state == "device",
                }),
                _ => None, // offline, recovery, ...
            }
        })
        .collect()
}

/// The name the phone gives itself (as Galdr on it shows), and its Android version.
pub fn describe(serial: &str) -> (Option<String>, Option<String>) {
    let shell = |cmd: &[&str]| {
        let mut args = vec!["-s", serial, "shell"];
        args.extend_from_slice(cmd);
        run(&args)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s != "null")
    };
    (
        shell(&["settings", "get", "global", "device_name"]),
        shell(&["getprop", "ro.build.version.release"]),
    )
}

/// A local TCP port that leads to `remote_port` on the phone; removed when dropped.
pub struct Forward {
    serial: String,
    pub port: u16,
}

pub fn forward(serial: &str, remote_port: u16) -> Result<Forward> {
    let remote = format!("tcp:{remote_port}");
    // tcp:0 lets adb pick a free port and print it.
    let out = run(&["-s", serial, "forward", "tcp:0", &remote])?;
    let port = out.trim().parse().context("adb forward gave no port")?;
    Ok(Forward {
        serial: serial.into(),
        port,
    })
}

impl Drop for Forward {
    fn drop(&mut self) {
        let local = format!("tcp:{}", self.port);
        let _ = run(&["-s", &self.serial, "forward", "--remove", &local]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cable_devices_only() {
        let out = "* daemon started successfully\n\
            List of devices attached\n\
            2A111FDH2004XQ         device usb:1-4 product:panther model:Pixel_7 device:panther transport_id:3\n\
            R58N30ABCDE            unauthorized usb:1-2 transport_id:4\n\
            192.168.1.20:5555      device product:x model:Other transport_id:5\n\
            adb-2A11-xyz._adb-tls-connect._tcp device model:Pixel_7 transport_id:6\n\
            emulator-5554          offline transport_id:7\n";
        assert_eq!(
            parse_devices(out),
            vec![
                UsbDevice {
                    serial: "2A111FDH2004XQ".into(),
                    model: "Pixel 7".into(),
                    authorized: true,
                },
                UsbDevice {
                    serial: "R58N30ABCDE".into(),
                    model: "R58N30ABCDE".into(),
                    authorized: false,
                },
            ]
        );
    }
}
