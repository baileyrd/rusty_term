//! Minimal GIF decoder (no crates) for inline images (iTerm2 OSC 1337).
//!
//! Handles GIF87a/GIF89a: global and local color tables, LZW-compressed
//! image data with variable code widths, interlacing, per-frame transparency
//! (GIF89a graphic-control extension), and multi-frame animations with the
//! full disposal-method compositing model (none / keep / restore-background /
//! restore-previous). Every frame is composited to a full logical-screen
//! canvas so callers get ready-to-blit frames plus their display times.
//!
//! Malformed streams decode to `None` (or stop at the last good frame)
//! rather than guessing, mirroring [`super::png`] / [`super::jpeg`].

/// One composited frame: full-canvas pixels in the grid's format
/// (`0xRRGGBB`, `None` = transparent) plus its display time.
pub(crate) struct Frame {
    pub pixels: Vec<Option<u32>>,
    pub delay_ms: u32,
}

/// A decoded GIF: logical-screen size and at least one composited frame.
pub(crate) struct Gif {
    pub width: usize,
    pub height: usize,
    pub frames: Vec<Frame>,
}

/// Pixel-count cap per canvas (4M px), matching the PNG/JPEG decoders.
const MAX_PIXELS: usize = 4 * 1024 * 1024;
/// Frame cap, matching the Kitty animation store's budget.
const MAX_FRAMES: usize = 64;

/// Little-endian u16 at `data[i]`.
fn u16le(data: &[u8], i: usize) -> Option<usize> {
    Some(((*data.get(i + 1)? as usize) << 8) | *data.get(i)? as usize)
}

/// Decode a GIF into composited full-canvas frames, or `None` if malformed
/// (or its first frame is). Later-frame corruption truncates the animation
/// at the last frame that decoded.
pub(crate) fn decode(data: &[u8]) -> Option<Gif> {
    if !(data.starts_with(b"GIF89a") || data.starts_with(b"GIF87a")) {
        return None;
    }
    let width = u16le(data, 6)?;
    let height = u16le(data, 8)?;
    if width == 0 || height == 0 || width.checked_mul(height)? > MAX_PIXELS {
        return None;
    }
    let packed = *data.get(10)?;
    let mut i = 13;
    let mut global_pal: Vec<u32> = Vec::new();
    if packed & 0x80 != 0 {
        let n = 2usize << (packed & 0x07);
        global_pal = read_palette(data, i, n)?;
        i += n * 3;
    }

    let mut frames: Vec<Frame> = Vec::new();
    // The compositing canvas frames are painted onto, and the disposal
    // snapshot the *next* frame starts from.
    let mut canvas: Vec<Option<u32>> = vec![None; width * height];
    // Pending graphic-control state for the next image descriptor.
    let (mut delay_ms, mut transparent, mut disposal) = (0u32, None::<u8>, 0u8);

    loop {
        match data.get(i) {
            None => break, // truncated after a good frame: keep what we have
            Some(0x3B) => break, // trailer
            Some(0x21) => {
                // Extension: label + sub-blocks. Only the graphic-control
                // extension (0xF9) carries state we use.
                let label = *data.get(i + 1)?;
                i += 2;
                if label == 0xF9 {
                    let len = *data.get(i)? as usize;
                    if len >= 4 {
                        let p = *data.get(i + 1)?;
                        disposal = (p >> 2) & 0x07;
                        delay_ms = (u16le(data, i + 2)? as u32) * 10;
                        transparent = (p & 1 != 0).then(|| data[i + 4]);
                    }
                }
                i = skip_subblocks(data, i)?;
            }
            Some(0x2C) => {
                let left = u16le(data, i + 1)?;
                let top = u16le(data, i + 3)?;
                let fw = u16le(data, i + 5)?;
                let fh = u16le(data, i + 7)?;
                let fp = *data.get(i + 9)?;
                i += 10;
                let mut pal = &global_pal;
                let local_pal;
                if fp & 0x80 != 0 {
                    let n = 2usize << (fp & 0x07);
                    local_pal = read_palette(data, i, n)?;
                    i += n * 3;
                    pal = &local_pal;
                }
                let interlaced = fp & 0x40 != 0;
                if fw == 0 || fh == 0 || pal.is_empty() {
                    return frames_or_none(width, height, frames);
                }
                // Decompress this frame's index stream.
                let min_code = *data.get(i)? as usize;
                i += 1;
                let (lzw_data, next) = collect_subblocks(data, i)?;
                i = next;
                let Some(indices) = lzw_decode(&lzw_data, min_code, fw * fh) else {
                    return frames_or_none(width, height, frames);
                };

                // What the next frame restores to, per this frame's disposal.
                let restore = match disposal {
                    3 => Some(canvas.clone()), // restore-previous
                    _ => None,
                };
                // Paint the frame rect onto the canvas (row order honors
                // interlacing; index -> palette color, transparent skipped).
                for (n, &idx) in indices.iter().enumerate() {
                    let fx = n % fw;
                    let fy = deinterlace(n / fw, fh, interlaced);
                    let (x, y) = (left + fx, top + fy);
                    if x >= width || y >= height {
                        continue;
                    }
                    if Some(idx) == transparent {
                        continue;
                    }
                    if let Some(&c) = pal.get(idx as usize) {
                        canvas[y * width + x] = Some(c);
                    }
                }
                frames.push(Frame { pixels: canvas.clone(), delay_ms });
                if frames.len() >= MAX_FRAMES {
                    break;
                }
                // Dispose for the next frame.
                match disposal {
                    2 => {
                        // Restore background: the background color is almost
                        // universally rendered as transparency.
                        for y in top..(top + fh).min(height) {
                            for x in left..(left + fw).min(width) {
                                canvas[y * width + x] = None;
                            }
                        }
                    }
                    3 => canvas = restore.unwrap_or(canvas),
                    _ => {} // 0/1: leave as painted
                }
                (delay_ms, transparent, disposal) = (0, None, 0);
            }
            Some(_) => break, // unknown block: keep decoded frames
        }
    }
    frames_or_none(width, height, frames)
}

fn frames_or_none(width: usize, height: usize, frames: Vec<Frame>) -> Option<Gif> {
    if frames.is_empty() { None } else { Some(Gif { width, height, frames }) }
}

/// Read `n` RGB triples at `data[at..]` into `0xRRGGBB` colors.
fn read_palette(data: &[u8], at: usize, n: usize) -> Option<Vec<u32>> {
    let raw = data.get(at..at + n * 3)?;
    Some(
        raw.chunks_exact(3)
            .map(|c| ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | c[2] as u32)
            .collect(),
    )
}

/// Skip a chain of data sub-blocks (len byte + payload, 0 terminates),
/// returning the index just past the terminator.
fn skip_subblocks(data: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let len = *data.get(i)? as usize;
        i += 1 + len;
        if len == 0 {
            return Some(i);
        }
    }
}

/// Concatenate a chain of data sub-blocks, returning the payload and the
/// index just past the terminator.
fn collect_subblocks(data: &[u8], mut i: usize) -> Option<(Vec<u8>, usize)> {
    let mut out = Vec::new();
    loop {
        let len = *data.get(i)? as usize;
        i += 1;
        if len == 0 {
            return Some((out, i));
        }
        out.extend_from_slice(data.get(i..i + len)?);
        i += len;
    }
}

/// The source row that pass-ordered row `n` lands on when `interlaced`
/// (GIF's four-pass 8/8, 8/8+4, 4/2, 2/1 scheme); identity otherwise.
fn deinterlace(n: usize, fh: usize, interlaced: bool) -> usize {
    if !interlaced {
        return n;
    }
    // Rows per pass: ceil((fh - start) / step).
    let pass = |start: usize, step: usize| fh.saturating_sub(start).div_ceil(step);
    let (p1, p2, p3) = (pass(0, 8), pass(4, 8), pass(2, 4));
    if n < p1 {
        n * 8
    } else if n < p1 + p2 {
        (n - p1) * 8 + 4
    } else if n < p1 + p2 + p3 {
        (n - p1 - p2) * 4 + 2
    } else {
        (n - p1 - p2 - p3) * 2 + 1
    }
}

/// GIF-flavored LZW: variable-width codes read LSB-first, clear/end codes,
/// dictionary capped at 4096 entries. Decodes exactly `expect` indices
/// (extra data ignored, shortfall padded — encoders in the wild do both).
fn lzw_decode(data: &[u8], min_code: usize, expect: usize) -> Option<Vec<u8>> {
    if !(1..=11).contains(&min_code) || expect == 0 {
        return None;
    }
    let clear = 1usize << min_code;
    let end = clear + 1;
    // Dictionary as (prefix, suffix) links; roots have no prefix.
    let mut prefix: Vec<u16> = Vec::with_capacity(4096);
    let mut suffix: Vec<u8> = Vec::with_capacity(4096);
    let reset = |prefix: &mut Vec<u16>, suffix: &mut Vec<u8>| {
        prefix.clear();
        suffix.clear();
        for c in 0..clear + 2 {
            prefix.push(u16::MAX);
            suffix.push((c & 0xFF) as u8);
        }
    };
    reset(&mut prefix, &mut suffix);

    let mut out = Vec::with_capacity(expect);
    let mut width = min_code + 1;
    let (mut acc, mut bits) = (0u32, 0usize);
    let mut pos = 0usize;
    let mut prev: Option<usize> = None;
    let mut expand = Vec::with_capacity(64);

    while out.len() < expect {
        // Refill and read one `width`-bit code, LSB-first.
        while bits < width {
            let Some(&b) = data.get(pos) else {
                // Truncated stream: pad the remainder as index 0.
                out.resize(expect, 0);
                return Some(out);
            };
            acc |= (b as u32) << bits;
            bits += 8;
            pos += 1;
        }
        let code = (acc & ((1 << width) - 1)) as usize;
        acc >>= width;
        bits -= width;

        if code == clear {
            reset(&mut prefix, &mut suffix);
            width = min_code + 1;
            prev = None;
            continue;
        }
        if code == end {
            out.resize(expect, 0);
            return Some(out);
        }
        // A code past the next free dictionary slot is corrupt; the code AT
        // the next slot is the KwKwK special case (expands to prev+first(prev)).
        if code > prefix.len() || (code == prefix.len() && prev.is_none()) {
            return None;
        }
        let cur = code;
        expand.clear();
        let kwk = cur == prefix.len();
        let mut c = if kwk { prev.unwrap() } else { cur };
        loop {
            expand.push(suffix[c]);
            let p = prefix[c];
            if p == u16::MAX {
                break;
            }
            c = p as usize;
        }
        expand.reverse();
        if kwk {
            let first = expand[0];
            expand.push(first);
        }
        out.extend_from_slice(&expand);

        // Grow the dictionary: prev + first(cur).
        if let Some(p) = prev
            && prefix.len() < 4096
        {
            prefix.push(p as u16);
            suffix.push(expand[0]);
            if prefix.len() == (1 << width) && width < 12 {
                width += 1;
            }
        }
        prev = Some(if kwk { prefix.len() - 1 } else { cur });
    }
    Some(out)
}
