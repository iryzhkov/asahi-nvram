use super::*;

fn field(data: &mut Vec<u8>, tag: u8, value: &[u8]) {
    data.push(tag);
    data.push(value.len() as u8);
    data.extend_from_slice(value);
}

fn record(ediv: &[u8], rand: &[u8]) -> Vec<u8> {
    let mut data = Vec::new();
    field(&mut data, 0xe, &[0, 1, 2, 3, 4, 5, 6]);
    field(&mut data, 0xa, ediv);
    field(&mut data, 0xb, rand);
    data
}

#[test]
fn compact_numeric_fields_are_zero_extended() {
    let devices = parse_ble_data(&record(&[0x34], &[0x78, 0x56])).unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].ediv, 0x34);
    assert_eq!(devices[0].rand, 0x5678);
}

#[test]
fn full_width_numeric_fields_keep_their_values() {
    let devices = parse_ble_data(&record(&[0x34, 0x12], &[1, 2, 3, 4, 5, 6, 7, 8])).unwrap();
    assert_eq!(devices[0].ediv, 0x1234);
    assert_eq!(devices[0].rand, 0x0807060504030201);
}

#[test]
fn empty_or_oversized_integers_are_rejected() {
    for value in [vec![], vec![1, 2, 3]] {
        assert!(parse_ble_data(&record(&value, &[0])).is_err());
    }
    assert!(parse_ble_data(&record(&[0], &[1; 9])).is_err());
}

#[test]
fn truncated_fields_and_wrong_key_lengths_return_errors() {
    for data in [vec![2], vec![2, 4, 1], vec![8, 1, 0], vec![0xe, 0],
                 vec![0x10, 1, 0], vec![0x9, 0], vec![0x6, 0]] {
        assert!(parse_ble_data(&data).is_err());
    }
}

#[test]
fn delimiter_does_not_create_a_zero_address_device() {
    let mut data = record(&[0], &[0]);
    field(&mut data, 0xf, &[]);
    assert_eq!(parse_ble_data(&data).unwrap().len(), 1);
    assert!(parse_ble_data(&[]).unwrap().is_empty());
}

#[test]
fn appearance_and_unknown_optional_tags_do_not_corrupt_keys() {
    let mut data = record(&[0], &[0]);
    field(&mut data, 0x11, &[0xc2, 0x03]);
    field(&mut data, 0x7f, &[1, 2, 3]);
    field(&mut data, 0x8, &[0x12; 16]);
    let devices = parse_ble_data(&data).unwrap();
    assert_eq!(devices[0].irk, [0x12; 16]);
    assert_eq!(devices[0].mac, [1, 2, 3, 4, 5, 6]);
}
