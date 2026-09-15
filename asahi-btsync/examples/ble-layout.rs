// Print field types and lengths only; never expose pairing keys or identities.
use apple_nvram::{nvram_parse, VarType};
fn main() {
    let bytes = std::fs::read("/dev/mtd/by-name/nvram").expect("read nvram");
    let mut nv = nvram_parse(&bytes).expect("parse nvram");
    let var = nv.active_part_mut().get_variable(b"BluetoothInfo", VarType::System).expect("BluetoothInfo");
    let bytes = var.value();
    let mut data = bytes.as_slice();
    while !data.is_empty() {
        assert!(data.len() >= 2, "truncated header");
        let tag = data[0];
        let len = data[1] as usize;
        assert!(data.len() >= 2 + len, "truncated value");
        println!("tag=0x{tag:02x} length={len}");
        data = &data[2 + len..];
    }
}
