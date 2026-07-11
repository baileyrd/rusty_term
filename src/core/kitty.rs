//! Kitty graphics protocol decoder (library-free).
//!
//! Handles `ESC _ G <control> ; <base64-payload> ESC \` commands. Supported
//! transmission formats (`f`): 24 (raw RGB), 32 (raw RGBA, the default), and 100
//! (PNG, decoded by [`super::png`]). Optional `o=z` zlib compression and chunked
//! transmission (`m=1`) are handled.
//!
//! Actions: query (`a=q`), transmit (`a=t`, into the grid's bounded image
//! store), transmit-and-display (`a=T`), put (`a=p`, placing a stored image —
//! honoring `c`/`r` cell geometry, and `U=1` creating a *virtual* placement
//! for Unicode placeholders, the mechanism that survives tmux), delete
//! (`a=d`, whole-store or by id), animation frames (`a=f`, composited onto
//! the previous frame at `x`/`y` with a `z` ms gap), and animation control
//! (`a=a`, run/stop). Non-virtual placements render as half-block cells via
//! [`Grid::render_image`] (plus the CPU renderer's pixel overlay); virtual
//! placements render wherever `U+10EEEE` placeholder cells appear.

use super::grid::Grid;
use super::{base64, inflate, png};

/// Cap on accumulated base64 payload bytes across chunks.
const MAX_PAYLOAD: usize = 8 * 1024 * 1024;
/// Cap on decompressed/raw pixel bytes.
const MAX_DECODED: usize = 16 * 1024 * 1024;

/// An in-flight (possibly chunked) Kitty transmission. Control keys come from
/// the first chunk; later chunks carry only `m` plus more payload.
#[derive(Default)]
pub(crate) struct Transmission {
    active: bool,
    action: u8,
    format: u32,
    width: usize,
    height: usize,
    compressed: bool,
    id: u32,
    quiet: u32,
    /// `c`/`r`: placement size in cells (0 = derive from the pixel size).
    cols: usize,
    rows: usize,
    /// `U=1`: a virtual placement (rendered via Unicode placeholders).
    virt: bool,
    /// `x`/`y`: an animation frame's offset within the root frame.
    frame_x: usize,
    frame_y: usize,
    /// `z`: an animation frame's display gap in milliseconds.
    gap_ms: u32,
    /// `s` for `a=a`: 1 stops, 2/3 run the animation.
    anim_state: u32,
    /// `d` for `a=d`: the delete scope letter.
    delete: u8,
    payload: Vec<u8>,
}

/// Process one Kitty APC command — `apc` is the bytes between `ESC _` and `ST`,
/// beginning with `G` (the caller guarantees this). Renders on the final chunk
/// of a transmit-and-display, and writes any acknowledgement to `responses`
/// (which the driver sends back to the child).
pub(crate) fn feed(t: &mut Transmission, apc: &[u8], g: &mut Grid, responses: &mut Vec<u8>) {
    let body = &apc[1..]; // drop the leading 'G'
    let (control, payload) = match body.iter().position(|&b| b == b';') {
        Some(p) => (&body[..p], &body[p + 1..]),
        None => (body, &body[0..0]),
    };

    let mut more = false;
    if t.active {
        // Continuation chunk: only `m` is meaningful.
        for (k, v) in kv_pairs(control) {
            if k == b"m" {
                more = parse_u32(v) == 1;
            }
        }
    } else {
        // First (or only) chunk: parse the full control set, with Kitty defaults.
        *t = Transmission {
            active: true,
            action: b't',
            format: 32,
            ..Transmission::default()
        };
        // `s` means width on a transmission but run-state on `a=a`, so the
        // action must be known before the other keys are interpreted.
        for (k, v) in kv_pairs(control) {
            if k == b"a" {
                t.action = v.first().copied().unwrap_or(b't');
            }
        }
        for (k, v) in kv_pairs(control) {
            match k {
                b"a" => {}
                b"s" if t.action == b'a' => t.anim_state = parse_u32(v),
                b"f" => t.format = parse_u32(v),
                b"s" => t.width = parse_u32(v) as usize,
                b"v" => t.height = parse_u32(v) as usize,
                b"o" => t.compressed = v == b"z",
                b"i" => t.id = parse_u32(v),
                b"q" => t.quiet = parse_u32(v),
                b"c" => t.cols = parse_u32(v) as usize,
                b"r" => t.rows = parse_u32(v) as usize,
                b"U" => t.virt = parse_u32(v) == 1,
                b"x" => t.frame_x = parse_u32(v) as usize,
                b"y" => t.frame_y = parse_u32(v) as usize,
                b"z" => t.gap_ms = parse_u32(v),
                b"d" => t.delete = v.first().copied().unwrap_or(b'a'),
                b"m" => more = parse_u32(v) == 1,
                _ => {}
            }
        }
    }

    if t.payload.len().saturating_add(payload.len()) <= MAX_PAYLOAD {
        t.payload.extend_from_slice(payload);
    }
    if more {
        return; // await the final chunk
    }

    // Final chunk: act on the completed command.
    let ok = match t.action {
        b'q' => true, // query: we speak the protocol
        b'T' => {
            // Transmit-and-display: store (so later `a=p`/frames can refer
            // to it) and place at the cursor — or virtually with `U=1`.
            match decode(t) {
                Some((w, h, px)) => {
                    if t.id != 0 {
                        g.kitty_store(t.id, w, h, px.clone());
                    }
                    place(t, g, w, h, &px);
                    true
                }
                None => false,
            }
        }
        b't' => match decode(t) {
            Some((w, h, px)) => {
                g.kitty_store(t.id, w, h, px);
                true
            }
            None => false,
        },
        b'p' => match g.kitty_get(t.id) {
            Some((w, h, px)) => {
                place(t, g, w, h, &px);
                true
            }
            None => false,
        },
        b'f' => match decode(t) {
            Some((w, h, px)) => g.kitty_add_frame(t.id, w, h, px, t.frame_x, t.frame_y, t.gap_ms),
            None => false,
        },
        b'a' => g.kitty_animate(t.id, t.anim_state != 1),
        b'd' => {
            // `i`/`I` delete by id; every other scope clears the store (a
            // superset of the requested visible-placement scopes — honest
            // over-deletion beats silently keeping "deleted" images).
            if matches!(t.delete, b'i' | b'I') {
                g.kitty_delete(Some(t.id));
            } else {
                g.kitty_delete(None);
            }
            true
        }
        _ => false,
    };
    // Acknowledge unless suppressed: q=0 → OK + errors, q=1 → errors only,
    // q=2 → silent. Queries are always answered (that's their purpose).
    if t.id != 0 {
        if ok && t.quiet == 0 {
            respond(responses, t.id, b"OK");
        } else if !ok && t.quiet < 2 {
            respond(responses, t.id, b"EBADF");
        }
    }
    *t = Transmission::default();
}

/// Decode the accumulated payload per its format into `(w, h, pixels)`.
fn decode(t: &Transmission) -> Option<(usize, usize, Vec<Option<u32>>)> {
    let raw = base64::decode(&t.payload)?;
    let raw = if t.compressed { inflate::zlib_decompress(&raw, MAX_DECODED)? } else { raw };
    match t.format {
        24 => raw_pixels(&raw, t.width, t.height, 3),
        32 => raw_pixels(&raw, t.width, t.height, 4),
        100 => {
            let img = png::decode(&raw)?;
            let px = rgba_pixels(&img.rgba);
            Some((img.width, img.height, px))
        }
        _ => None,
    }
}

/// Apply a placement: virtual (`U=1`) records the placeholder geometry;
/// otherwise the image renders at the cursor, honoring `c`/`r`.
fn place(t: &Transmission, g: &mut Grid, w: usize, h: usize, px: &[Option<u32>]) {
    if t.virt {
        g.kitty_virtual_place(t.id, t.cols, t.rows);
    } else if t.cols != 0 || t.rows != 0 {
        let c = (t.cols != 0).then_some(t.cols);
        let r = (t.rows != 0).then_some(t.rows);
        g.render_image_sized(w, h, px, c, r, true);
    } else {
        g.render_image(w, h, px);
    }
}

/// Pack raw interleaved RGB (`ch == 3`) or RGBA (`ch == 4`) bytes into pixels,
/// mapping alpha 0 to transparent. `None` if the buffer is too small.
fn raw_pixels(
    raw: &[u8],
    w: usize,
    h: usize,
    ch: usize,
) -> Option<(usize, usize, Vec<Option<u32>>)> {
    if w == 0 || h == 0 {
        return None;
    }
    let count = w.checked_mul(h)?;
    if raw.len() < count.checked_mul(ch)? {
        return None;
    }
    let mut px = Vec::with_capacity(count);
    for i in 0..count {
        let o = i * ch;
        let a = if ch == 4 { raw[o + 3] } else { 255 };
        px.push(rgb_or_transparent(raw[o], raw[o + 1], raw[o + 2], a));
    }
    Some((w, h, px))
}

/// Pack RGBA8 bytes (from the PNG decoder) into pixels.
fn rgba_pixels(rgba: &[u8]) -> Vec<Option<u32>> {
    rgba.chunks_exact(4)
        .map(|p| rgb_or_transparent(p[0], p[1], p[2], p[3]))
        .collect()
}

fn rgb_or_transparent(r: u8, g: u8, b: u8, a: u8) -> Option<u32> {
    if a == 0 {
        None
    } else {
        Some(((r as u32) << 16) | ((g as u32) << 8) | b as u32)
    }
}

/// Emit a Kitty response `ESC _ G i=<id>;<message> ESC \` to the child.
fn respond(responses: &mut Vec<u8>, id: u32, message: &[u8]) {
    responses.extend_from_slice(b"\x1b_Gi=");
    responses.extend_from_slice(id.to_string().as_bytes());
    responses.push(b';');
    responses.extend_from_slice(message);
    responses.extend_from_slice(b"\x1b\\");
}

/// Iterate `key=value` pairs from a comma-separated control string.
fn kv_pairs(control: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    control.split(|&b| b == b',').filter_map(|field| {
        if field.is_empty() {
            return None;
        }
        Some(match field.iter().position(|&b| b == b'=') {
            Some(p) => (&field[..p], &field[p + 1..]),
            None => (field, &field[0..0]),
        })
    })
}

/// Parse a leading run of ASCII digits as `u32` (saturating).
fn parse_u32(v: &[u8]) -> u32 {
    let mut n = 0u32;
    for &b in v {
        if !b.is_ascii_digit() {
            break;
        }
        n = n.saturating_mul(10).saturating_add((b - b'0') as u32);
    }
    n
}
