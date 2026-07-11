//! iTerm2 inline-image protocol (`OSC 1337 ; File=<args> : <base64>`).
//!
//! Only the inline-image subcommand is handled here; other `1337;` subcommands
//! (SetUserVar, CurrentDir, shell integration, …) are ignored by the OSC
//! dispatcher. The payload is a complete image file — PNG (via [`super::png`])
//! or baseline JPEG (via [`super::jpeg`]) — decoded to pixels and handed to
//! [`Grid::render_image_sized`] (half-block cells, plus the full-res overlay
//! under the `gui` renderer), honoring the `width`/`height`/
//! `preserveAspectRatio` geometry hints when present; display auto-fits to
//! the available columns otherwise, like Sixel/Kitty. GIF decodes via
//! [`super::gif`] — a multi-frame GIF plays in the windowed overlay through
//! the Kitty animation timer (TUI passthrough shows the first frame) — and
//! lossless WebP via [`super::webp`]; lossy (VP8) WebP would need a full
//! DCT video-intra decoder and stays out of scope.

use super::grid::Grid;
use super::{base64, gif, jpeg, png, webp};

/// Cap on the decoded file size (8 MiB) so a huge payload can't exhaust memory.
const MAX_FILE: usize = 8 * 1024 * 1024;

/// Handle an `OSC 1337` argument string (everything after `1337;`). Renders the
/// image when it is a well-formed inline PNG/JPEG transfer; otherwise no-ops.
pub(crate) fn feed(text: &str, g: &mut Grid) {
    let Some(rest) = text.strip_prefix("File=") else {
        return; // not the inline-image subcommand
    };
    let Some((args, b64)) = rest.split_once(':') else {
        return; // no payload separator
    };
    // iTerm2 displays inline only when `inline=1`; absent/0 means a file
    // download, which a terminal emulator has no surface for.
    let mut inline = false;
    let mut target_cols = None;
    let mut target_rows = None;
    let mut preserve_aspect = true;
    for kv in args.split(';') {
        let Some((k, v)) = kv.split_once('=') else { continue };
        match k {
            "inline" => inline = v == "1",
            "width" => target_cols = resolve_dimension(v, g.cols, g.cell_px.map(|(w, _)| w)),
            "height" => target_rows = resolve_dimension(v, g.rows, g.cell_px.map(|(_, h)| h)),
            "preserveAspectRatio" => preserve_aspect = v != "0",
            _ => {}
        }
    }
    if !inline {
        return;
    }
    // Bound the work before allocating: base64 expands ~4:3.
    if b64.len() / 4 * 3 > MAX_FILE {
        return;
    }
    let Some(file) = base64::decode(b64.as_bytes()) else {
        return;
    };
    // GIF first: it may be animated, which takes its own multi-frame path.
    if file.starts_with(b"GIF8") {
        if let Some(g_img) = gif::decode(&file) {
            let frames: Vec<(Vec<Option<u32>>, u32)> =
                g_img.frames.into_iter().map(|f| (f.pixels, f.delay_ms)).collect();
            #[cfg(any(test, feature = "gui"))]
            g.render_animated_image(
                g_img.width,
                g_img.height,
                frames,
                target_cols,
                target_rows,
                preserve_aspect,
            );
            // Plain TTY build: no overlay/animation timer exists — draw the
            // first frame's half-block cells, exactly like a static image.
            #[cfg(not(any(test, feature = "gui")))]
            if let Some((px, _)) = frames.into_iter().next() {
                g.render_image_sized(
                    g_img.width,
                    g_img.height,
                    &px,
                    target_cols,
                    target_rows,
                    preserve_aspect,
                );
            }
        }
        return;
    }
    let decoded = if file.starts_with(&[0x89, b'P', b'N', b'G']) {
        png::decode(&file).map(|im| (im.width, im.height, im.rgba))
    } else if file.starts_with(&[0xFF, 0xD8, 0xFF]) {
        jpeg::decode(&file).map(|im| (im.width, im.height, im.rgba))
    } else if file.starts_with(b"RIFF") {
        webp::decode(&file).map(|im| (im.width, im.height, im.rgba))
    } else {
        None // other formats not supported
    };
    let Some((w, h, rgba)) = decoded else {
        return;
    };
    // Pack RGBA8 into the grid's pixel format (alpha 0 -> transparent).
    let pixels: Vec<Option<u32>> = rgba
        .chunks_exact(4)
        .map(|p| {
            if p[3] == 0 {
                None
            } else {
                Some(((p[0] as u32) << 16) | ((p[1] as u32) << 8) | p[2] as u32)
            }
        })
        .collect();
    g.render_image_sized(w, h, &pixels, target_cols, target_rows, preserve_aspect);
}

/// Resolve an iTerm2 `width=`/`height=` value to a cell count along one axis:
/// a bare integer is cells, `N%` a percentage of `axis_cells` (the terminal's
/// current column or row count), `Npx` a pixel count converted via
/// `axis_cell_px` (that axis's real cell size in pixels — `None` in TUI mode,
/// where no real pixel size is ours to report, so a pixel hint can't be
/// resolved and falls back to "unset"/auto). `auto` is `None` too — iTerm2's
/// own spelling for "use the image's natural size here".
pub(crate) fn resolve_dimension(spec: &str, axis_cells: usize, axis_cell_px: Option<u16>) -> Option<usize> {
    if let Some(pct) = spec.strip_suffix('%') {
        let p: f64 = pct.parse().ok()?;
        return Some(((axis_cells as f64 * p / 100.0).round().max(1.0)) as usize);
    }
    if let Some(px) = spec.strip_suffix("px") {
        let n: f64 = px.parse().ok()?;
        let cell_px = axis_cell_px? as f64;
        if cell_px <= 0.0 {
            return None;
        }
        return Some(((n / cell_px).round().max(1.0)) as usize);
    }
    if spec.eq_ignore_ascii_case("auto") {
        return None;
    }
    spec.parse::<usize>().ok()
}
