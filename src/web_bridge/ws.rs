//! Minimal RFC 6455 WebSocket plumbing for the PTY bridge: the HTTP upgrade
//! handshake's accept-key derivation (SHA-1 + base64, both in-tree — no new
//! crates) and a frame codec. Pure functions over byte slices, so the whole
//! layer is unit-tested without a socket.
//!
//! Deliberately server-side-only and small: client frames must be masked
//! (the RFC requires it and we enforce it), server frames are written
//! unmasked, and extensions/fragmentation are not negotiated — the bridge's
//! peers are browsers speaking vanilla `WebSocket`, which never fragment the
//! small control messages and fragment data frames only above sizes we cap
//! anyway.

use crate::core::base64_encode;

/// The GUID every WebSocket handshake concatenates to the client's key
/// (RFC 6455 §1.3).
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Largest client frame payload the bridge accepts. Terminal input is
/// keystrokes and pastes; a megabyte is generous and bounds memory against
/// a hostile peer.
pub(crate) const MAX_FRAME: usize = 1 << 20;

/// Frame opcodes (RFC 6455 §5.2) — the subset the bridge handles.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Opcode {
    Continuation,
    Text,
    Binary,
    Close,
    Ping,
    Pong,
}

impl Opcode {
    fn from_bits(b: u8) -> Option<Opcode> {
        Some(match b {
            0x0 => Opcode::Continuation,
            0x1 => Opcode::Text,
            0x2 => Opcode::Binary,
            0x8 => Opcode::Close,
            0x9 => Opcode::Ping,
            0xA => Opcode::Pong,
            _ => return None,
        })
    }

    fn bits(self) -> u8 {
        match self {
            Opcode::Continuation => 0x0,
            Opcode::Text => 0x1,
            Opcode::Binary => 0x2,
            Opcode::Close => 0x8,
            Opcode::Ping => 0x9,
            Opcode::Pong => 0xA,
        }
    }
}

/// One decoded (unmasked) frame.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) fin: bool,
    pub(crate) opcode: Opcode,
    pub(crate) payload: Vec<u8>,
}

/// Why [`decode_frame`] refused the buffer (as opposed to needing more bytes).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FrameError {
    /// Reserved bits set or an unknown opcode — a peer speaking extensions we
    /// never negotiated (or garbage). The connection should close.
    Protocol,
    /// A client frame arrived unmasked (the RFC requires closing then).
    Unmasked,
    /// Payload length exceeds [`MAX_FRAME`].
    TooLarge,
}

/// Decode the first frame in `buf`, returning it plus how many bytes it
/// consumed — `Ok(None)` when the buffer doesn't hold a whole frame yet.
/// Client-to-server framing: masked payloads are required and unmasked here.
pub(crate) fn decode_frame(buf: &[u8]) -> Result<Option<(Frame, usize)>, FrameError> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let (b0, b1) = (buf[0], buf[1]);
    if b0 & 0x70 != 0 {
        return Err(FrameError::Protocol); // RSV1-3: no extensions negotiated
    }
    let Some(opcode) = Opcode::from_bits(b0 & 0x0F) else {
        return Err(FrameError::Protocol);
    };
    let fin = b0 & 0x80 != 0;
    let masked = b1 & 0x80 != 0;
    if !masked {
        return Err(FrameError::Unmasked);
    }
    let mut at = 2usize;
    let len = match b1 & 0x7F {
        126 => {
            if buf.len() < at + 2 {
                return Ok(None);
            }
            let l = u16::from_be_bytes([buf[at], buf[at + 1]]) as usize;
            at += 2;
            l
        }
        127 => {
            if buf.len() < at + 8 {
                return Ok(None);
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[at..at + 8]);
            at += 8;
            let l = u64::from_be_bytes(b);
            if l > MAX_FRAME as u64 {
                return Err(FrameError::TooLarge);
            }
            l as usize
        }
        l => l as usize,
    };
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge);
    }
    if buf.len() < at + 4 {
        return Ok(None);
    }
    let mask = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
    at += 4;
    if buf.len() < at + len {
        return Ok(None);
    }
    let payload: Vec<u8> =
        buf[at..at + len].iter().enumerate().map(|(i, b)| b ^ mask[i % 4]).collect();
    Ok(Some((Frame { fin, opcode, payload }, at + len)))
}

/// Encode a server-to-client frame (FIN set, unmasked — servers must not
/// mask, RFC 6455 §5.1).
pub(crate) fn encode_frame(opcode: Opcode, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    out.push(0x80 | opcode.bits());
    match payload.len() {
        l if l < 126 => out.push(l as u8),
        l if l <= u16::MAX as usize => {
            out.push(126);
            out.extend_from_slice(&(l as u16).to_be_bytes());
        }
        l => {
            out.push(127);
            out.extend_from_slice(&(l as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(payload);
    out
}

/// A Close frame carrying `code` (RFC 6455 §5.5.1) and an empty reason.
pub(crate) fn close_frame(code: u16) -> Vec<u8> {
    encode_frame(Opcode::Close, &code.to_be_bytes())
}

/// The `Sec-WebSocket-Accept` value for a client's `Sec-WebSocket-Key`
/// (RFC 6455 §4.2.2 step 5.4): base64(SHA-1(key + GUID)).
pub(crate) fn accept_key(client_key: &str) -> String {
    let mut input = client_key.trim().as_bytes().to_vec();
    input.extend_from_slice(WS_GUID.as_bytes());
    base64_encode(&sha1(&input))
}

/// SHA-1 (FIPS 180-1). Broken for collision resistance and fine here: the
/// WebSocket handshake uses it as a protocol checksum, not for security —
/// every conforming implementation must compute exactly this.
pub(crate) fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    // Pad: 0x80, zeros, 64-bit big-endian bit length, to a 64-byte multiple.
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    let mut w = [0u32; 80];
    for chunk in msg.chunks_exact(64) {
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Parse the upgrade request's header block (everything through the blank
/// line) and return the client's `Sec-WebSocket-Key` and `Origin` (if any).
/// `None` when the request isn't a well-formed GET + websocket upgrade.
pub(crate) fn parse_upgrade(head: &str) -> Option<(String, Option<String>)> {
    let mut lines = head.split("\r\n");
    let request = lines.next()?;
    if !request.starts_with("GET ") {
        return None;
    }
    let (mut key, mut origin, mut upgrade_ok, mut version_ok) = (None, None, false, false);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "upgrade" => upgrade_ok = value.eq_ignore_ascii_case("websocket"),
            "sec-websocket-key" => key = Some(value.to_string()),
            "sec-websocket-version" => version_ok = value == "13",
            "origin" => origin = Some(value.to_string()),
            _ => {}
        }
    }
    match (upgrade_ok && version_ok, key) {
        (true, Some(k)) => Some((k, origin)),
        _ => None,
    }
}

/// Whether `origin` is one a local development bridge should trust: pages
/// served from localhost (any port, http/https). Browsers always send
/// `Origin` on WebSocket connects, so this shuts out random web pages
/// driving a shell on the developer's machine; non-browser clients that
/// omit the header entirely are allowed (they aren't subject to the
/// cross-origin confused-deputy problem this guards against).
pub(crate) fn origin_allowed(origin: Option<&str>) -> bool {
    let Some(origin) = origin else { return true };
    let rest = origin.strip_prefix("http://").or_else(|| origin.strip_prefix("https://"));
    let Some(rest) = rest else { return false };
    // A bracketed IPv6 literal ("[::1]" or "[::1]:5173") has colons inside
    // the brackets, so a naive split(':') never reaches the host — take
    // everything through the closing bracket instead.
    let host = if rest.starts_with('[') {
        match rest.find(']') {
            Some(end) => &rest[..=end],
            None => rest, // malformed; won't match any allowed host below
        }
    } else {
        rest.split(':').next().unwrap_or("")
    };
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn sha1_matches_fips_vectors() {
        assert_eq!(hex(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(&sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        // Two blocks with length in the second: 64 bytes of 'a'.
        assert_eq!(hex(&sha1(&[b'a'; 64])), "0098ba824b5c16427bd7a1122a5a442a25ec644d");
    }

    #[test]
    fn accept_key_matches_the_rfc_example() {
        // RFC 6455 §1.3's worked example.
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    /// Mask and frame `payload` the way a client would.
    fn client_frame(opcode: Opcode, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
        let mut out = vec![0x80 | opcode.bits()];
        match payload.len() {
            l if l < 126 => out.push(0x80 | l as u8),
            l if l <= u16::MAX as usize => {
                out.push(0x80 | 126);
                out.extend_from_slice(&(l as u16).to_be_bytes());
            }
            l => {
                out.push(0x80 | 127);
                out.extend_from_slice(&(l as u64).to_be_bytes());
            }
        }
        out.extend_from_slice(&mask);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        out
    }

    #[test]
    fn decode_unmasks_and_reports_consumed_length() {
        let wire = client_frame(Opcode::Text, b"resize 100 30", [0x11, 0x22, 0x33, 0x44]);
        let (frame, used) = decode_frame(&wire).unwrap().unwrap();
        assert_eq!(used, wire.len());
        assert!(frame.fin);
        assert_eq!(frame.opcode, Opcode::Text);
        assert_eq!(frame.payload, b"resize 100 30");
        // A truncated buffer asks for more rather than erroring.
        assert_eq!(decode_frame(&wire[..wire.len() - 1]).unwrap(), None);
        assert_eq!(decode_frame(&wire[..1]).unwrap(), None);
    }

    #[test]
    fn decode_handles_extended_lengths() {
        let payload = vec![0xAB; 300]; // needs the 16-bit length form
        let wire = client_frame(Opcode::Binary, &payload, [9, 8, 7, 6]);
        let (frame, used) = decode_frame(&wire).unwrap().unwrap();
        assert_eq!(used, wire.len());
        assert_eq!(frame.payload, payload);
    }

    #[test]
    fn decode_rejects_unmasked_reserved_and_oversized() {
        // Unmasked client frame: header only is enough to refuse.
        assert_eq!(decode_frame(&[0x81, 0x03]), Err(FrameError::Unmasked));
        // RSV bit set.
        assert_eq!(decode_frame(&[0xC1, 0x83]), Err(FrameError::Protocol));
        // Unknown opcode 0x3.
        assert_eq!(decode_frame(&[0x83, 0x83]), Err(FrameError::Protocol));
        // 64-bit length far past MAX_FRAME.
        let mut huge = vec![0x82, 0x80 | 127];
        huge.extend_from_slice(&(u64::MAX).to_be_bytes());
        assert_eq!(decode_frame(&huge), Err(FrameError::TooLarge));
    }

    #[test]
    fn encode_decode_roundtrip_via_a_mask_of_zeros() {
        // A zero mask makes the client and server wire forms differ only in
        // the mask bit, so we can roundtrip encode_frame through the decoder.
        let server = encode_frame(Opcode::Binary, b"hello from the pty");
        let mut as_client = server.clone();
        as_client[1] |= 0x80;
        // Splice a zero mask after the 2-byte header.
        as_client.splice(2..2, [0u8; 4]);
        let (frame, _) = decode_frame(&as_client).unwrap().unwrap();
        assert_eq!(frame.payload, b"hello from the pty");
        assert_eq!(frame.opcode, Opcode::Binary);
    }

    #[test]
    fn parse_upgrade_extracts_key_and_origin() {
        let head = "GET /pty HTTP/1.1\r\nHost: 127.0.0.1:7703\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nOrigin: http://localhost:5173\r\n";
        let (key, origin) = parse_upgrade(head).unwrap();
        assert_eq!(key, "dGhlIHNhbXBsZSBub25jZQ==");
        assert_eq!(origin.as_deref(), Some("http://localhost:5173"));
        // Missing upgrade header / wrong version / POST are all refused.
        assert!(parse_upgrade("GET / HTTP/1.1\r\nSec-WebSocket-Key: x\r\n").is_none());
        assert!(
            parse_upgrade(
                "POST / HTTP/1.1\r\nUpgrade: websocket\r\nSec-WebSocket-Key: x\r\nSec-WebSocket-Version: 13\r\n"
            )
            .is_none()
        );
    }

    #[test]
    fn origin_policy_admits_localhost_only() {
        assert!(origin_allowed(None), "non-browser clients omit Origin");
        assert!(origin_allowed(Some("http://localhost:5173")));
        assert!(origin_allowed(Some("http://127.0.0.1:8080")));
        assert!(origin_allowed(Some("https://localhost")));
        assert!(!origin_allowed(Some("https://evil.example.com")));
        assert!(!origin_allowed(Some("http://localhost.evil.com")));
        assert!(!origin_allowed(Some("file://whatever")));
    }

    #[test]
    fn origin_policy_admits_bracketed_ipv6_loopback() {
        // A naive `rest.split(':').next()` sees the colons inside the
        // brackets and never reaches the host — these must still match.
        assert!(origin_allowed(Some("http://[::1]:5173")));
        assert!(origin_allowed(Some("https://[::1]")));
        assert!(!origin_allowed(Some("http://[::2]:5173")), "not the loopback address");
        // A malformed/unterminated bracket must not panic and must be
        // refused rather than accidentally matching.
        assert!(!origin_allowed(Some("http://[::1")));
    }
}
