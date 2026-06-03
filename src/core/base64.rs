//! Minimal standard base64 decoder (RFC 4648) — no crates. The Kitty graphics
//! protocol transmits its payloads as base64.

/// Decode standard base64 (alphabet `A–Za–z0–9+/`, optional `=` padding),
/// skipping ASCII whitespace. Returns `None` on an invalid byte.
pub(crate) fn decode(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &b in input {
        let v: u32 = match b {
            b'A'..=b'Z' => (b - b'A') as u32,
            b'a'..=b'z' => (b - b'a' + 26) as u32,
            b'0'..=b'9' => (b - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            b'=' => break, // padding marks the end of data
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}
