//! Minimal WebP decoder (no crates) for inline images (iTerm2 OSC 1337).
//!
//! Handles the RIFF container and the **VP8L lossless** bitstream: canonical
//! prefix (Huffman) codes with the compact code-length encoding, LZ77
//! backward references (including the 120 two-dimensional "close" distance
//! codes), the color cache, meta-prefix entropy images, and all four
//! transforms (predictor, color, subtract-green, color-indexing with
//! sub-byte pixel packing).
//!
//! **Lossy (VP8) streams return `None`**: VP8 intra decoding is a full
//! boolean-arithmetic DCT codec (a video keyframe decoder) — far more code
//! than every other decoder here combined, for a format PNG/VP8L covers in
//! practice on the inline-image path. Extended (VP8X) containers are walked
//! for their embedded VP8L chunk; animation (ANMF) and separate alpha
//! (ALPH+VP8) also fall out as unsupported.

/// A decoded image as tightly-packed RGBA8, shaped like [`super::png::Image`].
pub(crate) struct Image {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// Pixel-count cap (4M px), matching the PNG/JPEG/GIF decoders.
const MAX_PIXELS: usize = 4 * 1024 * 1024;

/// Decode a WebP file, or `None` if malformed or a lossy/animated variant.
pub(crate) fn decode(data: &[u8]) -> Option<Image> {
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return None;
    }
    // Walk the chunks for VP8L (directly, or inside a VP8X extended file).
    let mut i = 12;
    while i + 8 <= data.len() {
        let tag = &data[i..i + 4];
        let len = u32::from_le_bytes(data[i + 4..i + 8].try_into().ok()?) as usize;
        let body = data.get(i + 8..i + 8 + len)?;
        match tag {
            b"VP8L" => return vp8l(body),
            b"VP8 " | b"ANMF" | b"ALPH" => return None, // lossy / animated
            _ => {}                                     // VP8X header, ICCP, EXIF, XMP — skip
        }
        i += 8 + len + (len & 1); // chunks are 2-byte aligned
    }
    None
}

/// LSB-first bit reader over the VP8L stream.
struct Br<'a> {
    data: &'a [u8],
    pos: usize, // bit position
}

impl Br<'_> {
    fn bits(&mut self, n: usize) -> u32 {
        let mut v = 0u32;
        for k in 0..n {
            let byte = self.pos >> 3;
            let bit = self.pos & 7;
            let b = self.data.get(byte).copied().unwrap_or(0);
            v |= (((b >> bit) & 1) as u32) << k;
            self.pos += 1;
        }
        v
    }
}

/// One canonical prefix-code table, decoded bit by bit via the canonical
/// (code-length-ordered) tree walk.
struct Prefix {
    /// Sorted (length, symbol) pairs realized as first-code/offset tables per
    /// length, like the JPEG decoder's Annex-F walk.
    counts: [u32; 16],
    firsts: [u32; 16],
    offsets: [u32; 16],
    symbols: Vec<u32>,
    /// Fast path: a code with a single symbol emits it with zero bits read.
    single: Option<u32>,
}

impl Prefix {
    fn build(lengths: &[u32]) -> Option<Prefix> {
        let mut counts = [0u32; 16];
        let mut used = 0usize;
        let mut only = 0u32;
        for (sym, &l) in lengths.iter().enumerate() {
            if l > 0 {
                if l > 15 {
                    return None;
                }
                counts[l as usize] += 1;
                used += 1;
                only = sym as u32;
            }
        }
        if used == 0 {
            return None;
        }
        if used == 1 {
            return Some(Prefix {
                counts: [0; 16],
                firsts: [0; 16],
                offsets: [0; 16],
                symbols: vec![only],
                single: Some(only),
            });
        }
        // Kraft check + canonical first-code per length.
        let mut firsts = [0u32; 16];
        let mut offsets = [0u32; 16];
        let (mut code, mut offset) = (0u32, 0u32);
        for l in 1..16usize {
            code = (code + counts[l - 1]) << 1;
            firsts[l] = code;
            offsets[l] = offset;
            offset += counts[l];
        }
        // Symbols in (length, symbol) order.
        let mut symbols = vec![0u32; used];
        let mut next = offsets;
        for (sym, &l) in lengths.iter().enumerate() {
            if l > 0 {
                symbols[next[l as usize] as usize] = sym as u32;
                next[l as usize] += 1;
            }
        }
        Some(Prefix {
            counts,
            firsts,
            offsets,
            symbols,
            single: None,
        })
    }

    fn read(&self, br: &mut Br) -> Option<u32> {
        if let Some(s) = self.single {
            return Some(s);
        }
        let mut code = 0u32;
        for l in 1..16usize {
            code = (code << 1) | br.bits(1);
            let count = self.counts[l];
            if count > 0 && code >= self.firsts[l] && code < self.firsts[l] + count {
                return self
                    .symbols
                    .get((self.offsets[l] + code - self.firsts[l]) as usize)
                    .copied();
            }
        }
        None
    }
}

/// The order code-length code lengths are transmitted in.
const CL_ORDER: [usize; 19] = [
    17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
];

/// Read one prefix code (spec 6.2.2): either "simple" (1-2 symbols inline) or
/// the code-length-coded form with 16/17/18 repeat symbols.
fn read_prefix(br: &mut Br, alphabet: usize) -> Option<Prefix> {
    if br.bits(1) == 1 {
        // Simple: 1 or 2 symbols.
        let n = br.bits(1) as usize + 1;
        let first_len = br.bits(1); // 0 -> 1 bit symbol, 1 -> 8 bit
        let s0 = br.bits(if first_len == 1 { 8 } else { 1 });
        let mut lengths = vec![0u32; alphabet];
        *lengths.get_mut(s0 as usize)? = 1;
        if n == 2 {
            let s1 = br.bits(8);
            *lengths.get_mut(s1 as usize)? = 1;
        }
        return Prefix::build(&lengths);
    }
    // Code-length code.
    let num_cl = br.bits(4) as usize + 4;
    let mut cl_lengths = [0u32; 19];
    for &pos in CL_ORDER.iter().take(num_cl) {
        cl_lengths[pos] = br.bits(3);
    }
    let cl = Prefix::build(&cl_lengths)?;
    // Optional cap on how many symbols are coded.
    let max_symbols = if br.bits(1) == 1 {
        let length_nbits = 2 + 2 * br.bits(3) as usize;
        2 + br.bits(length_nbits) as usize
    } else {
        alphabet
    };
    let mut lengths = vec![0u32; alphabet];
    let mut prev = 8u32;
    let mut i = 0usize;
    let mut coded = 0usize;
    while i < alphabet && coded < max_symbols {
        let sym = cl.read(br)?;
        coded += 1;
        match sym {
            0..=15 => {
                lengths[i] = sym;
                i += 1;
                if sym != 0 {
                    prev = sym;
                }
            }
            16 => {
                let rep = 3 + br.bits(2) as usize;
                for _ in 0..rep {
                    if i >= alphabet {
                        return None;
                    }
                    lengths[i] = prev;
                    i += 1;
                }
            }
            17 => i += 3 + br.bits(3) as usize,
            18 => i += 11 + br.bits(7) as usize,
            _ => return None,
        }
        if i > alphabet {
            return None;
        }
    }
    Prefix::build(&lengths)
}

/// One meta-code group: the five prefix codes ARGB decoding uses.
struct Group {
    green: Prefix, // literals green + length codes + color-cache indices
    red: Prefix,
    blue: Prefix,
    alpha: Prefix,
    dist: Prefix,
}

/// Length/distance prefix-code extra bits (spec 6.2.3): symbol -> value.
fn lz77_value(br: &mut Br, sym: u32) -> usize {
    if sym < 4 {
        return sym as usize + 1;
    }
    let extra = (sym - 2) >> 1;
    let offset = (2 + (sym & 1)) << extra;
    (offset + br.bits(extra as usize)) as usize + 1
}

/// Map a distance symbol's value through the 120 two-dimensional neighbor
/// codes; values past 120 are linear distances. The table is libwebp's
/// `kCodeToPlane`, each byte `0xYX` meaning `(yoffset = Y, xoffset = 8 - X)`,
/// `dist = yoffset * xsize + xoffset` (floored to 1).
fn plane_distance(value: usize, xsize: usize) -> usize {
    if value > 120 {
        return value - 120;
    }
    const CODE_TO_PLANE: [u8; 120] = [
        0x18, 0x07, 0x17, 0x19, 0x28, 0x06, 0x27, 0x29, 0x16, 0x1a, 0x26, 0x2a, 0x38, 0x05, 0x37,
        0x39, 0x15, 0x1b, 0x36, 0x3a, 0x25, 0x2b, 0x48, 0x04, 0x47, 0x49, 0x14, 0x1c, 0x35, 0x3b,
        0x46, 0x4a, 0x24, 0x2c, 0x58, 0x45, 0x4b, 0x34, 0x3c, 0x03, 0x57, 0x59, 0x13, 0x1d, 0x56,
        0x5a, 0x23, 0x2d, 0x44, 0x4c, 0x55, 0x5b, 0x33, 0x3d, 0x68, 0x02, 0x67, 0x69, 0x12, 0x1e,
        0x66, 0x6a, 0x22, 0x2e, 0x54, 0x5c, 0x43, 0x4d, 0x65, 0x6b, 0x32, 0x3e, 0x78, 0x01, 0x77,
        0x79, 0x53, 0x5d, 0x11, 0x1f, 0x64, 0x6c, 0x42, 0x4e, 0x76, 0x7a, 0x21, 0x2f, 0x75, 0x7b,
        0x31, 0x3f, 0x63, 0x6d, 0x52, 0x5e, 0x00, 0x74, 0x7c, 0x41, 0x4f, 0x10, 0x20, 0x62, 0x6e,
        0x30, 0x73, 0x7d, 0x51, 0x5f, 0x40, 0x72, 0x7e, 0x61, 0x6f, 0x50, 0x71, 0x7f, 0x60, 0x70,
    ];
    let code = CODE_TO_PLANE[value - 1];
    let yoff = (code >> 4) as isize;
    let xoff = 8 - (code & 0x0F) as isize;
    let dist = yoff * xsize as isize + xoff;
    if dist < 1 { 1 } else { dist as usize }
}

/// The four VP8L transforms, in reverse-application order data.
enum Transform {
    Predictor { bits: u32, data: Vec<u32> },
    Color { bits: u32, data: Vec<u32> },
    SubtractGreen,
    ColorIndex { palette: Vec<u32>, packed_bits: u32 },
}

/// Decode the VP8L chunk body.
fn vp8l(body: &[u8]) -> Option<Image> {
    if body.first() != Some(&0x2F) {
        return None; // signature
    }
    let mut br = Br {
        data: &body[1..],
        pos: 0,
    };
    let width = br.bits(14) as usize + 1;
    let height = br.bits(14) as usize + 1;
    let _alpha_hint = br.bits(1);
    if br.bits(3) != 0 {
        return None; // version must be 0
    }
    if width.checked_mul(height)? > MAX_PIXELS {
        return None;
    }

    // Transforms (each may appear once), then the ARGB image data.
    let mut transforms = Vec::new();
    let mut xsize = width; // color-indexing narrows the coded width
    while br.bits(1) == 1 {
        match br.bits(2) {
            0 => {
                // Predictor: a subresolution image of predictor modes.
                let bits = br.bits(3) + 2;
                let bw = sub_size(xsize, bits);
                let bh = sub_size(height, bits);
                let data = decode_argb_image(&mut br, bw, bh, false)?;
                transforms.push(Transform::Predictor { bits, data });
            }
            1 => {
                let bits = br.bits(3) + 2;
                let bw = sub_size(xsize, bits);
                let bh = sub_size(height, bits);
                let data = decode_argb_image(&mut br, bw, bh, false)?;
                transforms.push(Transform::Color { bits, data });
            }
            2 => transforms.push(Transform::SubtractGreen),
            3 => {
                let n = br.bits(8) as usize + 1;
                let raw = decode_argb_image(&mut br, n, 1, false)?;
                // Palette entries are delta-coded component-wise.
                let mut palette = raw;
                for i in 1..palette.len() {
                    palette[i] = add_pixels(palette[i], palette[i - 1]);
                }
                let packed_bits: u32 = match n {
                    1..=2 => 3, // 8 pixels per byte-lane
                    3..=4 => 2,
                    5..=16 => 1,
                    _ => 0,
                };
                transforms.push(Transform::ColorIndex {
                    palette,
                    packed_bits,
                });
                xsize = sub_size(xsize, packed_bits);
            }
            _ => return None,
        }
        if transforms.len() > 4 {
            return None;
        }
    }

    let mut argb = decode_argb_image(&mut br, xsize, height, true)?;
    let mut cur_w = xsize;

    // Apply transforms in reverse order.
    for t in transforms.iter().rev() {
        match t {
            Transform::ColorIndex {
                palette,
                packed_bits,
            } => {
                argb = unpack_palette(&argb, cur_w, height, width, palette, *packed_bits)?;
                cur_w = width;
            }
            Transform::SubtractGreen => {
                for p in argb.iter_mut() {
                    let g = (*p >> 8) & 0xFF;
                    let r = ((*p >> 16) & 0xFF).wrapping_add(g) & 0xFF;
                    let b = (*p & 0xFF).wrapping_add(g) & 0xFF;
                    *p = (*p & 0xFF00FF00) | (r << 16) | b;
                }
            }
            Transform::Color { bits, data } => {
                apply_color_transform(&mut argb, cur_w, height, *bits, data);
            }
            Transform::Predictor { bits, data } => {
                apply_predictor(&mut argb, cur_w, height, *bits, data);
            }
        }
    }
    if cur_w != width || argb.len() < width * height {
        return None;
    }

    let mut rgba = Vec::with_capacity(width * height * 4);
    for &p in &argb[..width * height] {
        rgba.push(((p >> 16) & 0xFF) as u8);
        rgba.push(((p >> 8) & 0xFF) as u8);
        rgba.push((p & 0xFF) as u8);
        rgba.push((p >> 24) as u8);
    }
    Some(Image {
        width,
        height,
        rgba,
    })
}

fn sub_size(size: usize, bits: u32) -> usize {
    size.div_ceil(1usize << bits)
}

/// Component-wise modulo-256 add (palette delta coding).
fn add_pixels(a: u32, b: u32) -> u32 {
    let mut out = 0u32;
    for shift in [0u32, 8, 16, 24] {
        let s = (((a >> shift) & 0xFF) + ((b >> shift) & 0xFF)) & 0xFF;
        out |= s << shift;
    }
    out
}

/// Decode one ARGB image (spec 6.2): the entropy-coded pixel stream with
/// optional color cache and (for the top-level image only) meta prefix codes.
fn decode_argb_image(br: &mut Br, xsize: usize, ysize: usize, top_level: bool) -> Option<Vec<u32>> {
    if xsize == 0 || ysize == 0 || xsize.checked_mul(ysize)? > MAX_PIXELS {
        return None;
    }
    // Meta prefix image (top-level only).
    let (meta, meta_bits): (Option<Vec<u32>>, u32) = if top_level && br.bits(1) == 1 {
        let bits = br.bits(3) + 2;
        let mw = sub_size(xsize, bits);
        let mh = sub_size(ysize, bits);
        (Some(decode_argb_image(br, mw, mh, false)?), bits)
    } else {
        (None, 0)
    };
    // Color cache.
    let cache_bits = if br.bits(1) == 1 {
        let b = br.bits(4);
        if !(1..=11).contains(&b) {
            return None;
        }
        b as usize
    } else {
        0
    };
    let mut cache = vec![0u32; if cache_bits > 0 { 1 << cache_bits } else { 0 }];
    let cache_size = cache.len() as u32;

    // Prefix-code groups.
    let num_groups = meta
        .as_ref()
        .map(|m| {
            m.iter()
                .map(|&p| ((p >> 8) & 0xFFFF) + 1)
                .max()
                .unwrap_or(1) as usize
        })
        .unwrap_or(1);
    if num_groups > 256 {
        return None;
    }
    let green_alphabet = 256 + 24 + cache_size as usize;
    let mut groups = Vec::with_capacity(num_groups);
    for _ in 0..num_groups {
        groups.push(Group {
            green: read_prefix(br, green_alphabet)?,
            red: read_prefix(br, 256)?,
            blue: read_prefix(br, 256)?,
            alpha: read_prefix(br, 256)?,
            dist: read_prefix(br, 40)?,
        });
    }

    let mut out = vec![0u32; xsize * ysize];
    let mut pos = 0usize;
    while pos < out.len() {
        let (x, y) = (pos % xsize, pos / xsize);
        let group = match (&meta, meta_bits) {
            (Some(m), bits) if bits > 0 => {
                let mw = sub_size(xsize, bits);
                let entry = m[(y >> bits) * mw + (x >> bits)];
                let idx = ((entry >> 8) & 0xFFFF) as usize;
                groups.get(idx)?
            }
            _ => &groups[0],
        };
        let sym = group.green.read(br)?;
        if sym < 256 {
            // Literal: green, then red, blue, alpha from their codes.
            let g = sym;
            let r = group.red.read(br)?;
            let b = group.blue.read(br)?;
            let a = group.alpha.read(br)?;
            let px = (a << 24) | (r << 16) | (g << 8) | b;
            out[pos] = px;
            if cache_bits > 0 {
                cache[(px.wrapping_mul(0x1E35A7BD) >> (32 - cache_bits)) as usize] = px;
            }
            pos += 1;
        } else if sym < 256 + 24 {
            // LZ77 backward reference.
            let len = lz77_value(br, sym - 256);
            let dist_sym = group.dist.read(br)?;
            let dist_val = lz77_value(br, dist_sym);
            let dist = plane_distance(dist_val, xsize);
            if dist > pos || len > out.len() - pos {
                return None;
            }
            for _ in 0..len {
                out[pos] = out[pos - dist];
                if cache_bits > 0 {
                    let px = out[pos];
                    cache[(px.wrapping_mul(0x1E35A7BD) >> (32 - cache_bits)) as usize] = px;
                }
                pos += 1;
            }
        } else {
            // Color-cache hit.
            let idx = sym - 256 - 24;
            if idx >= cache_size {
                return None;
            }
            out[pos] = cache[idx as usize];
            pos += 1;
        }
    }
    Some(out)
}

/// Expand a color-indexed (palettized, possibly sub-byte packed) image back
/// to full width.
fn unpack_palette(
    argb: &[u32],
    packed_w: usize,
    height: usize,
    width: usize,
    palette: &[u32],
    packed_bits: u32,
) -> Option<Vec<u32>> {
    let per = 1usize << packed_bits; // pixels per packed unit
    let mask = (1u32 << (8 >> packed_bits)) - 1;
    let bits_per = 8 >> packed_bits;
    let mut out = vec![0u32; width.checked_mul(height)?];
    for y in 0..height {
        for px in 0..packed_w {
            let packed = (argb.get(y * packed_w + px)? >> 8) & 0xFF; // green channel
            for k in 0..per {
                let x = px * per + k;
                if x >= width {
                    break;
                }
                let idx = ((packed >> (k as u32 * bits_per)) & mask) as usize;
                out[y * width + x] = palette.get(idx).copied().unwrap_or(0);
            }
        }
    }
    Some(out)
}

/// Inverse color transform (spec 4.3): per-block signed cross-channel deltas.
fn apply_color_transform(argb: &mut [u32], width: usize, height: usize, bits: u32, data: &[u32]) {
    let bw = sub_size(width, bits);
    let delta = |t: u8, c: u8| -> i32 { ((t as i8 as i32) * (c as i8 as i32)) >> 5 };
    for y in 0..height {
        for x in 0..width {
            let Some(&cte) = data.get((y >> bits) * bw + (x >> bits)) else {
                continue;
            };
            let g2r = (cte & 0xFF) as u8;
            let g2b = ((cte >> 8) & 0xFF) as u8;
            let r2b = ((cte >> 16) & 0xFF) as u8;
            let p = argb[y * width + x];
            let g = ((p >> 8) & 0xFF) as u8;
            let mut r = ((p >> 16) & 0xFF) as i32;
            let mut b = (p & 0xFF) as i32;
            r += delta(g2r, g);
            b += delta(g2b, g);
            b += delta(r2b, (r & 0xFF) as u8);
            argb[y * width + x] =
                (p & 0xFF00FF00) | (((r & 0xFF) as u32) << 16) | ((b & 0xFF) as u32);
        }
    }
}

/// Inverse predictor transform (spec 4.2): add the per-block predictor's
/// prediction to each pixel (component-wise mod 256).
fn apply_predictor(argb: &mut [u32], width: usize, height: usize, bits: u32, data: &[u32]) {
    let bw = sub_size(width, bits);
    let avg2 = |a: u32, b: u32| -> u32 {
        let mut out = 0u32;
        for shift in [0u32, 8, 16, 24] {
            let s = (((a >> shift) & 0xFF) + ((b >> shift) & 0xFF)) / 2;
            out |= s << shift;
        }
        out
    };
    let clamp_add_sub_full = |a: u32, b: u32, c: u32| -> u32 {
        let mut out = 0u32;
        for shift in [0u32, 8, 16, 24] {
            let v = ((a >> shift) & 0xFF) as i32 + ((b >> shift) & 0xFF) as i32
                - ((c >> shift) & 0xFF) as i32;
            out |= (v.clamp(0, 255) as u32) << shift;
        }
        out
    };
    let clamp_add_sub_half = |a: u32, b: u32| -> u32 {
        // Select(?) no — this is ClampAddSubtractHalf(Average2(L,T), TL).
        let mut out = 0u32;
        for shift in [0u32, 8, 16, 24] {
            let avg = ((a >> shift) & 0xFF) as i32;
            let c = ((b >> shift) & 0xFF) as i32;
            let v = avg + (avg - c) / 2;
            out |= (v.clamp(0, 255) as u32) << shift;
        }
        out
    };
    let select = |l: u32, t: u32, tl: u32| -> u32 {
        // Predict from the direction with the smaller gradient.
        let mut pl = 0i32;
        let mut pt = 0i32;
        for shift in [0u32, 8, 16, 24] {
            let lc = ((l >> shift) & 0xFF) as i32;
            let tc = ((t >> shift) & 0xFF) as i32;
            let tlc = ((tl >> shift) & 0xFF) as i32;
            let p = lc + tc - tlc;
            pl += (p - lc).abs();
            pt += (p - tc).abs();
        }
        if pl < pt { l } else { t }
    };

    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            // Border rules: (0,0) predicts opaque black; row 0 predicts L;
            // column 0 predicts T.
            let mode = if x == 0 && y == 0 {
                u32::MAX // sentinel: fixed 0xFF000000
            } else if y == 0 {
                1
            } else if x == 0 {
                2
            } else {
                (data[(y >> bits) * bw + (x >> bits)] >> 8) & 0xFF
            };
            let l = if x > 0 { argb[i - 1] } else { 0 };
            let t = if y > 0 { argb[i - width] } else { 0 };
            let tl = if x > 0 && y > 0 {
                argb[i - width - 1]
            } else {
                0
            };
            // At the right edge `i+1-width` wraps to the current row's first
            // (already reconstructed) pixel -- the reference decoder's exact
            // memory-layout behavior, kept bit-for-bit.
            let tr = if y > 0 { argb[i + 1 - width] } else { 0 };
            let pred = match mode {
                u32::MAX => 0xFF000000,
                0 => 0xFF000000,
                1 => l,
                2 => t,
                3 => tr,
                4 => tl,
                5 => avg2(avg2(l, tr), t),
                6 => avg2(l, tl),
                7 => avg2(l, t),
                8 => avg2(tl, t),
                9 => avg2(t, tr),
                10 => avg2(avg2(l, tl), avg2(t, tr)),
                11 => select(l, t, tl),
                12 => clamp_add_sub_full(l, t, tl),
                13 => clamp_add_sub_half(avg2(l, t), tl),
                _ => 0xFF000000,
            };
            argb[i] = add_pixels(argb[i], pred);
        }
    }
}
