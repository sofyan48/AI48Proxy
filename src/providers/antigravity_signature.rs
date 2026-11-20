//! Non-Vertex Gemini function signature envelope subset from Go's gemini_validation.go.
//! This checks transport shape, not cryptographic provenance. Tool-invocation nested
//! envelopes and Claude reasoning replay require the full replay machinery.
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};

pub(super) fn function_signature(raw: &str) -> bool {
    let raw = raw.strip_prefix("google#").unwrap_or(raw);
    if raw.len() > 32 * 1024 * 1024 {
        return false;
    }
    let Ok(bytes) = STANDARD
        .decode(raw)
        .or_else(|_| STANDARD_NO_PAD.decode(raw))
    else {
        return false;
    };
    let Some(container) = field(&bytes, 0x12) else {
        return false;
    };
    let Some(value) = field(container, 0x0a) else {
        return false;
    };
    // The inner Tink key rotates; Go intentionally checks only its format byte.
    value.first() == Some(&1)
        || (value.len() == 36
            && value.iter().enumerate().all(|(i, b)| {
                if [8, 13, 18, 23].contains(&i) {
                    *b == b'-'
                } else {
                    b.is_ascii_hexdigit()
                }
            }))
}
fn field(bytes: &[u8], tag: u8) -> Option<&[u8]> {
    if bytes.first() != Some(&tag) {
        return None;
    }
    let mut length = 0usize;
    for (index, byte) in bytes.iter().copied().skip(1).take(10).enumerate() {
        let shift = index * 7;
        let low = usize::from(byte & 0x7f);
        if shift >= usize::BITS as usize || low > (usize::MAX >> shift) {
            return None;
        }
        let bits = low << shift;
        length = length.checked_add(bits)?;
        if byte & 0x80 == 0 {
            let payload = bytes.get(index + 2..)?;
            return (payload.len() == length).then_some(payload);
        }
    }
    None
}
