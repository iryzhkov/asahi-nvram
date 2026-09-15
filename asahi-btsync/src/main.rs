/* SPDX-License-Identifier: MIT */

use std::{
    env,
    fmt::Debug,
    fs,
    fs::OpenOptions,
    io::{self, stdout, Read, Write},
    path::Path,
    thread,
    time::Duration,
};

use apple_nvram::{nvram_parse, VarType, Variable};

use ini::Ini;

pub mod dbus;

#[cfg(test)]
mod tests;

#[derive(Debug)]
#[allow(dead_code)]
enum Error {
    Parse,
    SectionTooBig,
    ApplyError(std::io::Error),
    VariableNotFound,
    FileIO,
    BluezConfigDirNotFound,
    SliceError,
    DbusSystemd,
    DbusBluez,
}

impl From<apple_nvram::Error> for Error {
    fn from(e: apple_nvram::Error) -> Self {
        match e {
            apple_nvram::Error::ParseError => Error::Parse,
            apple_nvram::Error::SectionTooBig => Error::SectionTooBig,
            apple_nvram::Error::ApplyError(e) => Error::ApplyError(e),
        }
    }
}

impl From<io::Error> for Error {
    fn from(_e: io::Error) -> Self {
        Error::FileIO
    }
}

impl From<std::array::TryFromSliceError> for Error {
    fn from(_e: std::array::TryFromSliceError) -> Self {
        Error::SliceError
    }
}

type Result<T> = std::result::Result<T, Error>;

fn main() {
    if let Err(error) = real_main() {
        eprintln!("asahi-btsync: {error:?}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let matches = clap::command!()
        .arg(clap::arg!(-d --device [DEVICE] "Path to the nvram device."))
        .subcommand(clap::Command::new("list").about("Parse shared Bluetooth keys from nvram"))
        .subcommand(
            clap::Command::new("sync")
                .about("Sync Bluetooth device information from nvram")
                .arg(clap::arg!(-c --config [CONFIG] "Bluez config path.")),
        )
        .subcommand(
            clap::Command::new("dump").about("Dump binary Bluetooth device info from nvram"),
        )
        .get_matches();

    let default_name = "/dev/mtd/by-name/nvram".to_owned();
    let default_config = "/var/lib/bluetooth".to_owned();
    let bt_var = "BluetoothUHEDevices";
    let ble_var = "BluetoothInfo";

    let mut file = OpenOptions::new()
        .read(true)
        .open(matches.get_one::<String>("device").unwrap_or(&default_name))
        .unwrap();
    let mut data = Vec::new();
    file.read_to_end(&mut data).unwrap();
    let mut nv = nvram_parse(&data)?;
    let active = nv.active_part_mut();
    let Some(bt_devs) = active.get_variable(bt_var.as_bytes(), VarType::System) else {
        return Ok(());
    };
    let ble_devs = active.get_variable(ble_var.as_bytes(), VarType::System);

    match matches.subcommand() {
        Some(("list", _args)) => {
            print_btkeys(bt_devs, ble_devs)?;
        }
        Some(("sync", args)) => {
            sync_btkeys(
                bt_devs,
                ble_devs,
                args.get_one::<String>("config").unwrap_or(&default_config),
            )?;
        }
        Some(("dump", _args)) => {
            dump(bt_devs).expect("Failed to dump bt device info");
        }
        _ => {
            print_btkeys(bt_devs, ble_devs)?;
        }
    }
    Ok(())
}

fn dump(var: &dyn Variable) -> Result<()> {
    stdout().write_all(&var.value())?;
    Ok(())
}

trait Device {
    fn mac(&self) -> [u8; 6];
    fn render(&self) -> Ini;
}

pub struct BtDevice {
    mac: [u8; 6],
    class: u16,
    name: String,
    vendor_id: u16,
    product_id: u16,
    pairing_key: [u8; 16],
}

pub struct BtInfo {
    mac: [u8; 6],
    devices: Vec<BtDevice>,
}

#[derive(Default, Debug)]
pub struct BleDevice {
    mac: [u8; 6],
    mac_type: u8,
    name: String,
    vendor_source: u8,
    vendor_id: u16,
    product_id: u16,
    irk: [u8; 16],
    pltk: [u8; 16],
    ltk: [u8; 16],
    ediv: u16,
    rand: u64,
}

fn read_le_u16(input: &mut &[u8]) -> Result<u16> {
    let (int_bytes, rest) = input.split_at(std::mem::size_of::<u16>());
    *input = rest;
    Ok(u16::from_le_bytes(int_bytes.try_into()?))
}

fn parse_bt_device(input: &mut &[u8]) -> Result<BtDevice> {
    // parse MAC
    let (mac_bytes, remain) = input.split_at(6_usize);
    *input = remain;
    let mac: [u8; 6] = mac_bytes.try_into().expect("end of bytes");

    let class = read_le_u16(input)?;

    // skip 2 bytes
    *input = &input[2..];

    // parse device name (u16_le length + \0 terminated utf-8 string)
    let name_len = read_le_u16(input)? as usize;
    let (name_bytes, remain) = input.split_at(name_len);
    *input = remain;
    let name = String::from_utf8_lossy(&name_bytes[..name_len - 1]).to_string();

    // parse pairing key
    let (key_bytes, remain) = input.split_at(16);
    *input = remain;
    let key: [u8; 16] = key_bytes.try_into().expect("end of bytes");

    // parse product / vendor id
    let product_id = read_le_u16(input)?;
    let vendor_id = read_le_u16(input)?;

    // skip 2 unknown trailing bytes
    *input = &input[2..];

    Ok(BtDevice {
        mac,
        class,
        name,
        vendor_id,
        product_id,
        pairing_key: key,
    })
}

fn parse_ble_info(var: &dyn Variable) -> Result<Vec<BleDevice>> {
    parse_ble_data(&var.value())
}

// Apple encodes small integer values compactly, including EDIV and Rand.
fn ble_integer<const N: usize>(value: &[u8]) -> Result<[u8; N]> {
    if value.is_empty() || value.len() > N {
        return Err(Error::Parse);
    }
    let mut bytes = [0; N];
    bytes[..value.len()].copy_from_slice(value);
    Ok(bytes)
}

fn parse_ble_data(mut data: &[u8]) -> Result<Vec<BleDevice>> {
    let mut devices = Vec::new();
    if data.is_empty() {
        return Ok(devices);
    }
    let mut device = BleDevice::default();
    while !data.is_empty() {
        let header = data.get(..2).ok_or(Error::Parse)?;
        let tag = header[0];
        let length = header[1] as usize;
        let value = data.get(2..2 + length).ok_or(Error::Parse)?;
        data = &data[2 + length..];
        match tag {
            0x2 => {
                device.name = String::from_utf8_lossy(value).into_owned();
            }
            0x8 => {
                device.irk = value.try_into()?;
            }
            0xe => {
                device.mac_type = *value.first().ok_or(Error::Parse)?;
                device.mac = value.get(1..).ok_or(Error::Parse)?.try_into()?;
            }
            0x10 => {
                device.pltk = value.get(1..).ok_or(Error::Parse)?.try_into()?;
            }
            0x5 => {
                device.vendor_id = u16::from_le_bytes(ble_integer(value)?);
            }
            0x6 => {
                device.vendor_source = u8::from_le_bytes(ble_integer(value)?);
            }
            0xf => {
                if device.mac != [0; 6] {
                    devices.push(device);
                }
                device = BleDevice::default();
            }
            0x4 => {
                device.product_id = u16::from_le_bytes(ble_integer(value)?);
            }
            0x9 => {
                device.ltk = value.get(1..).ok_or(Error::Parse)?.try_into()?;
            }
            0xa => {
                device.ediv = u16::from_le_bytes(ble_integer(value)?);
            }
            0xb => {
                device.rand = u64::from_le_bytes(ble_integer(value)?);
            }
            0x11 => {} // GAP Appearance; not needed to import pairing keys.
            _ => {
                eprintln!("WARNING: Unknown BLE pairing data field: {tag:x}")
            }
        }
    }
    if device.mac != [0; 6] {
        devices.push(device);
    }

    Ok(devices)
}

fn parse_bt_info(var: &dyn Variable) -> Result<BtInfo> {
    let data = var.value();

    assert!(data.len() >= 8);
    let adapter_mac: [u8; 6] = data[0..6].try_into()?;
    let num_devices = data[6];
    assert!(data[7] == 0x04);

    let mut dev_data = &data[8..];

    let mut devices: Vec<BtDevice> = Vec::new();
    for _n in 0..num_devices {
        devices.push(parse_bt_device(&mut dev_data)?);
    }

    Ok(BtInfo {
        mac: adapter_mac,
        devices,
    })
}

fn format_mac(mac: &[u8; 6]) -> Result<String> {
    Ok(mac
        .iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<String>>()
        .join(":"))
}

fn format_key(key: &[u8; 16]) -> String {
    key.iter().map(|x| format!("{x:02X}")).rev().collect()
}

fn format_key_reverse(key: &[u8; 16]) -> String {
    key.iter().rev().map(|x| format!("{x:02X}")).rev().collect()
}

fn print_btkeys(bt_var: &dyn Variable, ble_var: Option<&dyn Variable>) -> Result<()> {
    let info = parse_bt_info(bt_var)?;

    for dev in info.devices {
        println!(
            "ID {:04x}:{:04x} {} ({}) BR/EDR",
            dev.vendor_id,
            dev.product_id,
            dev.name,
            format_mac(&dev.mac)?
        );
    }

    if let Some(ble_var) = ble_var {
        let devs = parse_ble_info(ble_var)?;

        for dev in devs {
            println!(
                "ID {:04x}:{:04x} {} ({}) LE",
                dev.vendor_id,
                dev.product_id,
                dev.name,
                format_mac(&dev.mac)?
            );
        }
    }

    Ok(())
}

impl Device for BtDevice {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }
    fn render(&self) -> Ini {
        let mut info = Ini::new();

        info.with_section(Some("General"))
            .set("Name", self.name.clone())
            .set("Class", format!("{:#08X}", self.class))
            .set("Trusted", "true")
            .set("Blocked", "false")
            .set("WakeAllowed", "true");
        info.with_section(Some("LinkKey"))
            .set("Key", format_key(&self.pairing_key));
        info.with_section(Some("SelficeID"))
            .set("Vendor", format!("{}", self.vendor_id))
            .set("Product", format!("{}", self.product_id));
        info
    }
}

impl Device for BleDevice {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }
    fn render(&self) -> Ini {
        let mut info = Ini::new();

        info.with_section(Some("General"))
            .set("Name", self.name.clone())
            .set(
                "AddressType",
                if self.mac_type == 0 {
                    "public"
                } else {
                    "static"
                },
            )
            .set("Trusted", "true")
            .set("Blocked", "false")
            .set("SupportedTechnologies", "LE;")
            .set("CablePairing", "false")
            .set("WakeAllowed", "true");
        info.with_section(Some("ConnectionParameters"))
            .set("MinInterval", "6")
            .set("MaxInterval", "6")
            .set("Latency", "0")
            .set("Timeout", "300");
        info.with_section(Some("IdentityResolvingKey"))
            .set("Key", format_key_reverse(&self.irk));
        if self.pltk != [0; _] {
            for alias in ["PeripheralLongTermKey", "SlaveLongTermKey"] {
                info.with_section(Some(alias))
                    .set("Key", format_key_reverse(&self.pltk))
                    .set("Authenticated", "2")
                    .set("EncSize", "16")
                    .set("EDiv", "0")
                    .set("Rand", "0");
            }
        }
        info.with_section(Some("DeviceID"))
            .set("Source", format!("{}", self.vendor_source))
            .set("Vendor", format!("{}", self.vendor_id))
            .set("Product", format!("{}", self.product_id))
            .set("Version", "0");
        info
    }
}

fn create_config_entry(adapter_path: &Path, dev: &impl Device, ty: &str) -> Result<usize> {
    let dev_path = adapter_path.join(format_mac(&dev.mac())?);

    if !dev_path.is_dir() {
        fs::create_dir(dev_path.clone())?;
    }

    let info_file = dev_path.as_path().join("info");
    if info_file.exists() {
        return Ok(0);
    }
    dev.render().write_to_file(info_file)?;

    println!("{} {}", format_mac(&dev.mac())?, ty);
    Ok(1)
}

fn sync_btkeys(
    bt_var: &dyn Variable,
    ble_var: Option<&dyn Variable>,
    config: &String,
) -> Result<()> {
    let config_path = Path::new(config);

    if !config_path.is_dir() {
        return Err(Error::BluezConfigDirNotFound);
    }

    let bt_info = parse_bt_info(bt_var)?;

    let adapter_path = config_path.join(format_mac(&bt_info.mac)?);

    if !adapter_path.is_dir() {
        fs::create_dir(adapter_path.clone())?;
    }
    let mut added_devs = 0;

    for dev in &bt_info.devices {
        added_devs += create_config_entry(&adapter_path, dev, "BR/EDR")?;
    }
    let ble_info = ble_var.map(parse_ble_info).transpose()?.unwrap_or_default();
    for dev in &ble_info {
        added_devs += create_config_entry(&adapter_path, dev, "LE")?;
    }
    if added_devs > 0 {
        if let Err(e) = dbus::systemd_reload_bt_config() {
            println!("Failed to reload bluetoothd config {}", e);
            return Err(Error::DbusSystemd);
        }
        // sleep 500 ms to let bluetoothd reload its config
        thread::sleep(Duration::from_millis(500));
        if let Err(e) = dbus::bluez_connect(&bt_info, &ble_info) {
            println!("Failed to connect bluetooth devices {}", e);
            return Err(Error::DbusBluez);
        }
    }
    Ok(())
}
