//! HomeKit TLV8 encoding used by pair-setup and pair-verify.

use anyhow::{bail, Result};

pub const TLV_METHOD: u8 = 0x00;
pub const TLV_IDENTIFIER: u8 = 0x01;
pub const TLV_SALT: u8 = 0x02;
pub const TLV_PUBLIC_KEY: u8 = 0x03;
pub const TLV_PROOF: u8 = 0x04;
pub const TLV_ENCRYPTED_DATA: u8 = 0x05;
pub const TLV_STATE: u8 = 0x06;
pub const TLV_ERROR: u8 = 0x07;
pub const TLV_RETRY_DELAY: u8 = 0x08;
pub const TLV_SIGNATURE: u8 = 0x0A;
pub const TLV_FLAGS: u8 = 0x13;

#[derive(Debug, Clone)]
pub struct TlvItem {
    pub typ: u8,
    pub value: Vec<u8>,
}

impl TlvItem {
    pub fn new(typ: u8, value: Vec<u8>) -> Self {
        Self { typ, value }
    }
}

/// Encode TLV items. Values longer than 255 bytes are split into consecutive
/// fragments of the same type, each prior fragment exactly 255 bytes long.
pub fn encode(items: &[TlvItem]) -> Vec<u8> {
    let mut out = Vec::new();
    for item in items {
        let mut val = item.value.as_slice();
        while val.len() > 255 {
            out.push(item.typ);
            out.push(255);
            out.extend_from_slice(&val[..255]);
            val = &val[255..];
        }
        if !val.is_empty() {
            out.push(item.typ);
            out.push(val.len() as u8);
            out.extend_from_slice(val);
        }
    }
    out
}

/// Decode a TLV byte stream, reassembling fragmented values.
pub fn decode(data: &[u8]) -> Result<Vec<TlvItem>> {
    let mut items: Vec<TlvItem> = Vec::new();
    let mut prev_full = false;
    let mut i = 0usize;
    while i < data.len() {
        if i + 2 > data.len() {
            bail!("truncated TLV");
        }
        let typ = data[i];
        let len = data[i + 1] as usize;
        i += 2;
        if i + len > data.len() {
            bail!("truncated TLV value");
        }
        if prev_full && items.last().map(|it| it.typ) == Some(typ) {
            items.last_mut().unwrap().value.extend_from_slice(&data[i..i + len]);
        } else {
            items.push(TlvItem::new(typ, data[i..i + len].to_vec()));
        }
        prev_full = len == 255;
        i += len;
    }
    Ok(items)
}

/// Look up the first value of a given type.
pub fn get(items: &[TlvItem], typ: u8) -> Option<&[u8]> {
    items.iter().find(|it| it.typ == typ).map(|it| it.value.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_merges_fragments() {
        let val: Vec<u8> = (0..384).map(|i| i as u8).collect();
        let enc = encode(&[
            TlvItem::new(TLV_PUBLIC_KEY, val.clone()),
            TlvItem::new(TLV_PROOF, vec![0xAA]),
        ]);
        let items = decode(&enc).unwrap();
        assert_eq!(get(&items, TLV_PUBLIC_KEY), Some(val.as_slice()));
        assert_eq!(get(&items, TLV_PROOF), Some([0xAA].as_slice()));
    }
}
