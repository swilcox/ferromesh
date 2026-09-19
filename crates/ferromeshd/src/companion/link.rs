//! Finding and opening the radio's USB serial port.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serialport::{SerialPort, SerialPortType};

const BAUD: u32 = 115_200;
/// Reads return after this long without data, so the session can check the
/// time and whether to stop.
const READ_TIMEOUT: Duration = Duration::from_millis(200);
/// Espressif's USB vendor id: the built-in USB port of ESP32-S3 boards such as
/// the Heltec V3 and V4.
const ESPRESSIF_VID: u16 = 0x303A;
const BY_ID: &str = "/dev/serial/by-id";

/// Opens `device`, or the one Espressif USB serial port when it's `auto`.
/// Returns the path opened.
pub fn open(device: &str) -> Result<(String, Box<dyn SerialPort>)> {
    let path = if device == "auto" { find()? } else { device.to_owned() };
    let port = serialport::new(&path, BAUD)
        .timeout(READ_TIMEOUT)
        .open()
        .with_context(|| format!("opening {path}"))?;
    Ok((path, port))
}

fn find() -> Result<String> {
    let mut found: Vec<String> = serialport::available_ports()
        .context("listing serial ports")?
        .into_iter()
        .filter(|port| matches!(&port.port_type, SerialPortType::UsbPort(usb) if usb.vid == ESPRESSIF_VID))
        .map(|port| port.port_name)
        // macOS lists each port twice; the tty. one waits for a carrier signal.
        .filter(|name| !name.starts_with("/dev/tty."))
        .collect();
    if found.is_empty() {
        // Without udev, Linux ports carry no USB details, but their
        // /dev/serial/by-id names include the vendor.
        found = by_id();
    }
    found.sort();
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => bail!("no Espressif USB serial port found; set companion.device to the radio's port"),
        many => bail!(
            "found several Espressif USB serial ports ({}); set companion.device to the radio's",
            many.join(", ")
        ),
    }
}

fn by_id() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(BY_ID) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("Espressif"))
        .map(|name| Path::new(BY_ID).join(name).to_string_lossy().into_owned())
        .collect()
}
