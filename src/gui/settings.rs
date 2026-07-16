//! The in-app settings page model (windowed front-end).
//!
//! A small, mostly-pure state machine: the editable settings grouped into
//! categories (a Windows-Terminal-style sidebar), their current values seeded
//! from the live [`Config`](crate::config::Config), how ←/→ change each, how
//! each row renders (label + one-line description + a value widget), and how
//! the set persists to the config file. The window owns *applying* a change
//! to the running terminal (retheme, font rebuild, …) and drawing the page;
//! this module owns the *values* and their transitions, so the cycling/
//! clamping/formatting/persistence logic stays unit-testable without a live
//! window.

use crate::config::{self, ClipboardPolicy, Config, LaunchMode, SettingEdit};
use crate::core::{CursorShape, SCROLLBACK_MAX, Theme};
use crate::shells::DetectedShell;

/// Font-size adjuster bounds. Tighter than the config's 4–512 clamp so the
/// +/- steps stay on a sane on-screen range. Shared with `window`'s Ctrl+=/
/// Ctrl+- runtime zoom, which enforces the same range.
pub(crate) const FONT_MIN: f32 = 6.0;
pub(crate) const FONT_MAX: f32 = 72.0;
/// Scrollback adjuster step and ceiling (the config clamps to 10M; the page
/// steps in readable increments).
const SCROLLBACK_STEP: usize = 1000;
const SCROLLBACK_CEIL: usize = 1_000_000;
/// The built-in font size when the config names none (mirrors `window::FONT_PX`).
pub(crate) const DEFAULT_FONT_PX: f32 = 18.0;
/// Built-in defaults mirrored from `window` (`INIT_COLS`/`INIT_ROWS`/
/// `DEFAULT_PAD`), used both to seed unset values and to gate persistence —
/// a value still at its default never adds a line to the config file.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_PAD: u32 = 8;
/// Padding adjuster bounds/step (the config clamps to 0–64).
const PAD_MAX: u32 = 64;
const PAD_STEP: u32 = 2;
/// Opacity adjuster bounds/step; floored above 0 so a stray ←-mash can't
/// make the window invisible from inside the window.
const OPACITY_MIN: f32 = 0.30;
const OPACITY_STEP: f32 = 0.05;
/// Minimum-contrast choices: off, then the WCAG large-text / normal-text /
/// enhanced thresholds. (The config accepts any 1.0–21.0; a custom value
/// seeds to the nearest step.)
const CONTRAST_STEPS: [f32; 4] = [1.0, 3.0, 4.5, 7.0];

/// Sidebar categories, in display order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Category {
    Appearance,
    Terminal,
    Window,
}

/// `(category, sidebar label)` in display order.
pub(crate) const CATEGORIES: [(Category, &str); 3] = [
    (Category::Appearance, "Appearance"),
    (Category::Terminal, "Terminal"),
    (Category::Window, "Window"),
];

/// Which setting a row edits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Field {
    // Appearance
    Theme,
    FontSize,
    Ligatures,
    Cursor,
    Blink,
    CursorTrail,
    MinContrast,
    // Terminal
    Shell,
    Scrollback,
    CopyHtml,
    Clipboard,
    ClickToMove,
    Bell,
    // Window
    Padding,
    Opacity,
    LaunchMode,
    Cols,
    Rows,
}

impl Field {
    /// The rows of `cat`, in display order.
    pub(crate) fn in_category(cat: Category) -> &'static [Field] {
        match cat {
            Category::Appearance => &[
                Field::Theme,
                Field::FontSize,
                Field::Ligatures,
                Field::Cursor,
                Field::Blink,
                Field::CursorTrail,
                Field::MinContrast,
            ],
            Category::Terminal => &[
                Field::Shell,
                Field::Scrollback,
                Field::CopyHtml,
                Field::Clipboard,
                Field::ClickToMove,
                Field::Bell,
            ],
            Category::Window => &[
                Field::Padding,
                Field::Opacity,
                Field::LaunchMode,
                Field::Cols,
                Field::Rows,
            ],
        }
    }

    /// Row title.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Field::Theme => "Color theme",
            Field::FontSize => "Font size",
            Field::Ligatures => "Ligatures",
            Field::Cursor => "Cursor shape",
            Field::Blink => "Cursor blink",
            Field::CursorTrail => "Cursor trail",
            Field::MinContrast => "Minimum contrast",
            Field::Shell => "Default shell",
            Field::Scrollback => "Scrollback",
            Field::CopyHtml => "Copy as HTML",
            Field::Clipboard => "Clipboard access (OSC 52)",
            Field::ClickToMove => "Click to move cursor",
            Field::Bell => "Bell alert",
            Field::Padding => "Padding",
            Field::Opacity => "Opacity",
            Field::LaunchMode => "Launch mode",
            Field::Cols => "Columns",
            Field::Rows => "Rows",
        }
    }

    /// One-line description shown dimmed under the label.
    pub(crate) fn description(self) -> &'static str {
        match self {
            Field::Theme => "Color preset for text, background, and the ANSI palette",
            Field::FontSize => "Terminal font size in pixels (Ctrl+= / Ctrl+- zoom too)",
            Field::Ligatures => "Programming-font ligatures, when the font has them",
            Field::Cursor => "Default cursor shape; programs may override it",
            Field::Blink => "Blink the cursor",
            Field::CursorTrail => "Brief fading trail when the cursor jumps",
            Field::MinContrast => "Recolor text that falls under this contrast ratio",
            Field::Shell => "Program spawned in new tabs",
            Field::Scrollback => "History lines kept per pane",
            Field::CopyHtml => "Copy also puts styled HTML on the clipboard",
            Field::Clipboard => "Whether programs may write or read the clipboard",
            Field::ClickToMove => "Click at the prompt moves the input cursor",
            Field::Bell => "BEL requests attention and badges the tab",
            Field::Padding => "Inner margin around the terminal, in pixels",
            Field::Opacity => "Window background opacity",
            Field::LaunchMode => "How new windows open",
            Field::Cols => "Initial width in cells",
            Field::Rows => "Initial height in cells",
        }
    }

    /// Whether this setting only takes effect at the next launch (rendered
    /// as a dim "next launch" tag beside the value, instead of burying the
    /// caveat in description prose).
    pub(crate) fn applies_next_launch(self) -> bool {
        matches!(self, Field::LaunchMode | Field::Cols | Field::Rows)
    }
}

/// How a row's value renders: a switch, or a cyclable choice / stepped number
/// (both drawn as `‹ value ›` when selected).
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Widget {
    Toggle(bool),
    Choice(String),
    Number(String),
}

/// One rendered row of the active category (or of the search results).
pub(crate) struct Row {
    /// The field this row edits — lets the renderer add field-specific
    /// garnish (the theme row's palette swatches).
    pub field: Field,
    pub label: &'static str,
    pub description: &'static str,
    pub widget: Widget,
    /// Renders a dim "next launch" tag beside the value.
    pub next_launch: bool,
    /// Rendered dimmed and inert — the running configuration can't honor
    /// this setting (e.g. opacity under the CPU renderer).
    pub disabled: bool,
    /// The owning category's label, shown beside the row while a search
    /// filter is active (results span categories); `None` otherwise.
    pub category: Option<&'static str>,
    /// The value differs from its built-in default — rendered as an accent
    /// dot so customizations are visible at a glance.
    pub modified: bool,
}

/// The settings page state: current values plus navigation (active category,
/// highlighted row, scroll offset, hovered row).
pub(crate) struct Settings {
    /// Active category, an index into [`CATEGORIES`].
    pub cat: usize,
    /// Highlighted row within the active category.
    pub sel: usize,
    /// First visible row of the active category (list scrolling).
    pub scroll: usize,
    /// Row under the mouse pointer, if any (hover highlight).
    pub hover: Option<usize>,
    /// Active search query. While `Some`, the page shows one flat list of
    /// every setting (across all categories) whose label or description
    /// matches, and the sidebar deactivates.
    pub filter: Option<String>,
    /// In-progress text edit of the selected number row (click-to-type /
    /// Enter-to-edit). Committed by Enter or blur, cancelled by Esc.
    pub editing: Option<String>,
    /// Whether any value changed since the page opened (gates the save and
    /// drives the Saved/Modified header indicator).
    pub dirty: bool,
    /// Whether the running renderer honors opacity (GPU yes, CPU no); when
    /// false the opacity row renders disabled and refuses changes rather
    /// than silently doing nothing.
    opacity_supported: bool,
    // --- values ---
    /// Index into [`config::PRESETS`].
    theme: usize,
    font_size: f32,
    ligatures: bool,
    cursor: CursorShape,
    blink: bool,
    cursor_trail: bool,
    /// Index into [`CONTRAST_STEPS`].
    min_contrast: usize,
    /// Detected shells as `(name, launch-path)`. Index `0` of the row's choice
    /// is "(default)"; `1..` map to `shells[choice - 1]`.
    shells: Vec<(String, String)>,
    shell: usize,
    scrollback: usize,
    copy_html: bool,
    clipboard: ClipboardPolicy,
    click_to_move: bool,
    bell: bool,
    padding: u32,
    opacity: f32,
    /// `None` = normal window.
    launch_mode: Option<LaunchMode>,
    cols: u16,
    rows: u16,
}

impl Settings {
    /// Seed the page from the live configuration. `theme` and `font_size`
    /// come in separately from `cfg` because both have live runtime state the
    /// config may lag (an auto-resolved theme, a Ctrl+= zoomed font);
    /// `opacity_supported` reports whether the active renderer honors
    /// opacity (see the field of the same name).
    pub(crate) fn new(
        cfg: &Config,
        theme: &Theme,
        font_size: f32,
        detected: &[DetectedShell],
        opacity_supported: bool,
    ) -> Self {
        let shells: Vec<(String, String)> = detected
            .iter()
            .map(|s| (s.name.to_string(), s.path.to_string_lossy().into_owned()))
            .collect();
        // Match the configured theme to a preset by its colors, else "default".
        let theme = config::PRESETS
            .iter()
            .position(|&n| config::preset(n).as_ref() == Some(theme))
            .unwrap_or(0);
        // Match the configured shell to a detected one (by friendly name or path).
        let shell = cfg
            .shell
            .as_deref()
            .and_then(|s| shells.iter().position(|(n, p)| n == s || p == s).map(|i| i + 1))
            .unwrap_or(0);
        // A custom contrast ratio seeds to the nearest step.
        let ratio = cfg.minimum_contrast.unwrap_or(1.0);
        let min_contrast = CONTRAST_STEPS
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                (*a - ratio).abs().partial_cmp(&(*b - ratio).abs()).unwrap()
            })
            .map(|(i, _)| i)
            .unwrap_or(0);
        Settings {
            cat: 0,
            sel: 0,
            scroll: 0,
            hover: None,
            filter: None,
            editing: None,
            dirty: false,
            opacity_supported,
            theme,
            font_size,
            ligatures: cfg.ligatures.unwrap_or(true),
            cursor: cfg.cursor_style.unwrap_or_default(),
            blink: cfg.cursor_blink.unwrap_or(false),
            cursor_trail: cfg.cursor_trail.unwrap_or(false),
            min_contrast,
            shells,
            shell,
            scrollback: cfg.scrollback.unwrap_or(SCROLLBACK_MAX),
            copy_html: cfg.copy_html.unwrap_or(true),
            clipboard: cfg.clipboard.unwrap_or_default(),
            click_to_move: cfg.click_to_move.unwrap_or(true),
            bell: cfg.bell.unwrap_or(true),
            padding: cfg.padding.unwrap_or(DEFAULT_PAD),
            opacity: cfg.opacity.unwrap_or(1.0),
            launch_mode: cfg.launch_mode,
            cols: cfg.cols.unwrap_or(DEFAULT_COLS),
            rows: cfg.rows.unwrap_or(DEFAULT_ROWS),
        }
    }

    /// The active category.
    pub(crate) fn category(&self) -> Category {
        CATEGORIES[self.cat].0
    }

    /// The fields the page currently shows, each with its category's label:
    /// the active category's rows, or — while a search filter is active —
    /// every setting across all categories whose label or description
    /// matches (case-insensitive). An empty query matches everything.
    fn visible(&self) -> Vec<(Field, &'static str)> {
        match &self.filter {
            None => Field::in_category(self.category())
                .iter()
                .map(|&f| (f, CATEGORIES[self.cat].1))
                .collect(),
            Some(q) => {
                let q = q.to_lowercase();
                CATEGORIES
                    .iter()
                    .flat_map(|&(c, name)| {
                        Field::in_category(c).iter().map(move |&f| (f, name))
                    })
                    .filter(|(f, _)| {
                        q.is_empty()
                            || f.label().to_lowercase().contains(&q)
                            || f.description().to_lowercase().contains(&q)
                    })
                    .collect()
            }
        }
    }

    /// The highlighted field, `None` when a filter matches nothing.
    pub(crate) fn field(&self) -> Option<Field> {
        self.visible().get(self.sel).map(|&(f, _)| f)
    }

    /// Number of visible rows (for the renderer and hit-testing).
    pub(crate) fn len(&self) -> usize {
        self.visible().len()
    }

    // --- search filter ---

    /// Start (or keep) filtering; the query begins empty.
    pub(crate) fn start_filter(&mut self) {
        if self.filter.is_none() {
            self.filter = Some(String::new());
            self.reset_view();
        }
    }

    /// Append `c` to the filter query, starting the filter if needed.
    pub(crate) fn filter_input(&mut self, c: char) {
        self.filter.get_or_insert_with(String::new).push(c);
        self.reset_view();
    }

    /// Delete the last query character; an emptied query keeps filtering
    /// (Esc clears it).
    pub(crate) fn filter_backspace(&mut self) {
        if let Some(q) = &mut self.filter {
            q.pop();
            self.reset_view();
        }
    }

    /// Drop the filter and return to the active category's rows.
    pub(crate) fn clear_filter(&mut self) {
        if self.filter.take().is_some() {
            self.reset_view();
        }
    }

    /// Selection/scroll/hover reset after anything that changes which rows
    /// are visible — a stale index would silently point at a different row.
    fn reset_view(&mut self) {
        self.sel = 0;
        self.scroll = 0;
        self.hover = None;
        self.editing = None;
    }

    // --- number editing (click-to-type) ---

    /// Begin editing the selected row if it's an editable number, seeding
    /// the buffer with `seed` (a typed digit) or, when `None`, the current
    /// value's bare text. Returns whether an edit started.
    pub(crate) fn begin_edit(&mut self, seed: Option<char>) -> bool {
        let Some(field) = self.field() else { return false };
        if self.is_disabled(field) || !matches!(self.widget(field), Widget::Number(_)) {
            return false;
        }
        self.editing = Some(match seed {
            Some(c) => c.to_string(),
            None => self.edit_seed(field),
        });
        true
    }

    /// The bare editable text of `field`'s current value (no unit, no
    /// thousands separators — what a user would retype).
    fn edit_seed(&self, field: Field) -> String {
        match field {
            Field::FontSize => fmt_px(self.font_size),
            Field::Scrollback => self.scrollback.to_string(),
            Field::Padding => self.padding.to_string(),
            Field::Opacity => format!("{}", (self.opacity * 100.0).round()),
            Field::Cols => self.cols.to_string(),
            Field::Rows => self.rows.to_string(),
            _ => String::new(),
        }
    }

    /// Append a character to the edit buffer (digits, plus `.` for the
    /// font size); anything else is ignored.
    pub(crate) fn edit_input(&mut self, c: char) {
        if let Some(buf) = &mut self.editing
            && (c.is_ascii_digit() || c == '.')
        {
            buf.push(c);
        }
    }

    /// Delete the last edit-buffer character.
    pub(crate) fn edit_backspace(&mut self) {
        if let Some(buf) = &mut self.editing {
            buf.pop();
        }
    }

    /// Abandon the edit, keeping the old value.
    pub(crate) fn cancel_edit(&mut self) {
        self.editing = None;
    }

    /// Parse and apply the edit buffer to the selected field, clamped to
    /// the field's own bounds. Returns the field on success so the window
    /// can apply it live; an unparsable or empty buffer cancels instead.
    pub(crate) fn commit_edit(&mut self) -> Option<Field> {
        let buf = self.editing.take()?;
        let field = self.field()?;
        match field {
            Field::FontSize => {
                self.font_size = buf.parse::<f32>().ok()?.clamp(FONT_MIN, FONT_MAX);
            }
            Field::Scrollback => {
                self.scrollback = buf.parse::<usize>().ok()?.min(SCROLLBACK_CEIL);
            }
            Field::Padding => self.padding = buf.parse::<u32>().ok()?.min(PAD_MAX),
            Field::Opacity => {
                let floor = (OPACITY_MIN * 100.0).round() as u32;
                self.opacity = buf.parse::<u32>().ok()?.clamp(floor, 100) as f32 / 100.0;
            }
            Field::Cols => self.cols = buf.parse::<u16>().ok()?.clamp(20, 400),
            Field::Rows => self.rows = buf.parse::<u16>().ok()?.clamp(10, 200),
            _ => return None,
        }
        self.dirty = true;
        Some(field)
    }

    /// Move the highlight by one row within the active category, wrapping.
    pub(crate) fn move_sel(&mut self, forward: bool) {
        self.sel = wrap(self.sel, self.len(), forward);
    }

    /// Highlight row `i` of the active category directly (a mouse click);
    /// ignores an out-of-range row.
    pub(crate) fn select(&mut self, i: usize) {
        if i < self.len() {
            self.sel = i;
        }
    }

    /// Switch to category `i` (sidebar click), resetting the row highlight
    /// and scroll — and dropping any search filter, since the user just
    /// chose a category over the results. Ignores an out-of-range index.
    pub(crate) fn set_category(&mut self, i: usize) {
        if i >= CATEGORIES.len() {
            return;
        }
        let filtering = self.filter.is_some();
        if i != self.cat || filtering {
            self.filter = None;
            self.cat = i;
            self.reset_view();
        }
    }

    /// Step to the next/previous category (Tab / Shift+Tab), wrapping.
    pub(crate) fn cycle_category(&mut self, forward: bool) {
        self.set_category(wrap(self.cat, CATEGORIES.len(), forward));
    }

    /// Scroll so the highlighted row is inside a window of `visible` rows.
    /// Call after any selection move, with the renderer's current capacity.
    pub(crate) fn ensure_visible(&mut self, visible: usize) {
        if visible == 0 {
            return;
        }
        if self.sel < self.scroll {
            self.scroll = self.sel;
        } else if self.sel >= self.scroll + visible {
            self.scroll = self.sel + 1 - visible;
        }
        // Never leave a dangling empty window past the end.
        self.scroll = self.scroll.min(self.len().saturating_sub(1));
    }

    /// Scroll the list by `delta` rows (mouse wheel; positive = down),
    /// clamped to the list.
    pub(crate) fn scroll_by(&mut self, delta: isize, visible: usize) {
        let max = self.len().saturating_sub(visible.max(1));
        self.scroll = self.scroll.saturating_add_signed(delta).min(max);
    }

    /// Whether `f` is inert in the running configuration (rendered disabled;
    /// changes are refused rather than silently doing nothing).
    fn is_disabled(&self, f: Field) -> bool {
        f == Field::Opacity && !self.opacity_supported
    }

    /// Whether `f`'s value differs from its built-in default — the same
    /// defaults `edits()` uses to gate persistence, so the accent dot and
    /// "this line lands in the config file" always agree.
    fn is_modified(&self, f: Field) -> bool {
        match f {
            Field::Theme => self.theme != 0,
            Field::FontSize => self.font_size != DEFAULT_FONT_PX,
            Field::Ligatures => !self.ligatures,
            Field::Cursor => self.cursor != CursorShape::default(),
            Field::Blink => self.blink,
            Field::CursorTrail => self.cursor_trail,
            Field::MinContrast => self.min_contrast().is_some(),
            Field::Shell => self.shell != 0,
            Field::Scrollback => self.scrollback != SCROLLBACK_MAX,
            Field::CopyHtml => !self.copy_html,
            Field::Clipboard => self.clipboard != ClipboardPolicy::default(),
            Field::ClickToMove => !self.click_to_move,
            Field::Bell => !self.bell,
            Field::Padding => self.padding != DEFAULT_PAD,
            Field::Opacity => self.opacity != 1.0,
            Field::LaunchMode => self.launch_mode.is_some(),
            Field::Cols => self.cols != DEFAULT_COLS,
            Field::Rows => self.rows != DEFAULT_ROWS,
        }
    }

    /// Whether any of `cat`'s settings are modified (the sidebar's dot).
    pub(crate) fn category_modified(&self, cat: Category) -> bool {
        Field::in_category(cat).iter().any(|&f| self.is_modified(f))
    }

    /// Change the highlighted row's value by `1` or, with `big` on a stepped
    /// number, `10` small steps (Shift+←/→) — choices and toggles ignore
    /// `big`, since "ten entries onward" isn't a meaningful jump.
    pub(crate) fn change_by(&mut self, forward: bool, big: bool) -> Option<Field> {
        let field = self.field()?;
        let steps = match (big, self.widget(field)) {
            (true, Widget::Number(_)) => 10,
            _ => 1,
        };
        let mut changed = None;
        for _ in 0..steps {
            changed = self.change(forward);
        }
        changed
    }

    /// Jump a stepped number to its bound (Home = minimum, End = maximum) by
    /// stepping until the value stops moving. Choices and toggles don't
    /// jump — they wrap, so they have no bound to land on.
    pub(crate) fn jump(&mut self, to_max: bool) -> Option<Field> {
        let field = self.field()?;
        if !matches!(self.widget(field), Widget::Number(_)) || self.is_disabled(field) {
            return None;
        }
        // Every number field clamps, so this terminates; the cap is a
        // backstop, sized for the widest range (scrollback: 0..=1M by 1000).
        for _ in 0..2000 {
            let before = self.widget(field);
            self.change(to_max);
            if self.widget(field) == before {
                break;
            }
        }
        Some(field)
    }

    /// Change the highlighted row's value (`forward` = ←/→ direction; toggles
    /// ignore it). Returns the field changed so the window can apply it live;
    /// `None` when a filter matches nothing or the row is disabled.
    pub(crate) fn change(&mut self, forward: bool) -> Option<Field> {
        let field = self.field()?;
        if self.is_disabled(field) {
            return None;
        }
        match field {
            Field::Theme => self.theme = wrap(self.theme, config::PRESETS.len(), forward),
            Field::FontSize => {
                // Step to the next whole point. A config-seeded fractional size
                // (e.g. 18.5) would otherwise stay fractional forever; round
                // toward the step direction so it lands on integers.
                let next = if forward {
                    (self.font_size + 1.0).floor()
                } else {
                    (self.font_size - 1.0).ceil()
                };
                self.font_size = next.clamp(FONT_MIN, FONT_MAX);
            }
            Field::Ligatures => self.ligatures = !self.ligatures,
            Field::Cursor => {
                const ORDER: [CursorShape; 3] =
                    [CursorShape::Block, CursorShape::Underline, CursorShape::Bar];
                let i = ORDER.iter().position(|&c| c == self.cursor).unwrap_or(0);
                self.cursor = ORDER[wrap(i, ORDER.len(), forward)];
            }
            Field::Blink => self.blink = !self.blink,
            Field::CursorTrail => self.cursor_trail = !self.cursor_trail,
            Field::MinContrast => {
                self.min_contrast = wrap(self.min_contrast, CONTRAST_STEPS.len(), forward)
            }
            Field::Shell => self.shell = wrap(self.shell, self.shells.len() + 1, forward),
            Field::Scrollback => {
                self.scrollback = if forward {
                    (self.scrollback + SCROLLBACK_STEP).min(SCROLLBACK_CEIL)
                } else {
                    self.scrollback.saturating_sub(SCROLLBACK_STEP)
                };
            }
            Field::CopyHtml => self.copy_html = !self.copy_html,
            Field::Clipboard => {
                const ORDER: [ClipboardPolicy; 3] =
                    [ClipboardPolicy::Off, ClipboardPolicy::WriteOnly, ClipboardPolicy::ReadWrite];
                let i = ORDER.iter().position(|&c| c == self.clipboard).unwrap_or(1);
                self.clipboard = ORDER[wrap(i, ORDER.len(), forward)];
            }
            Field::ClickToMove => self.click_to_move = !self.click_to_move,
            Field::Bell => self.bell = !self.bell,
            Field::Padding => {
                self.padding = if forward {
                    (self.padding + PAD_STEP).min(PAD_MAX)
                } else {
                    self.padding.saturating_sub(PAD_STEP)
                };
            }
            Field::Opacity => {
                // Step in integer percent so a config-seeded 0.87 lands on
                // the 85/90 grid (float step-and-round accumulates noise like
                // 0.90000004, which would then persist to the config file).
                let step = (OPACITY_STEP * 100.0).round() as u32;
                let floor = (OPACITY_MIN * 100.0).round() as u32;
                let pct = (self.opacity * 100.0).round() as u32;
                let pct = if forward {
                    (pct / step + 1) * step
                } else {
                    (pct.div_ceil(step)).saturating_sub(1) * step
                };
                self.opacity = pct.clamp(floor, 100) as f32 / 100.0;
            }
            Field::LaunchMode => {
                const ORDER: [Option<LaunchMode>; 3] =
                    [None, Some(LaunchMode::Maximized), Some(LaunchMode::Fullscreen)];
                let i = ORDER.iter().position(|&m| m == self.launch_mode).unwrap_or(0);
                self.launch_mode = ORDER[wrap(i, ORDER.len(), forward)];
            }
            Field::Cols => {
                self.cols = if forward {
                    (self.cols + 5).min(400)
                } else {
                    self.cols.saturating_sub(5).max(20)
                };
            }
            Field::Rows => {
                self.rows = if forward {
                    (self.rows + 1).min(200)
                } else {
                    self.rows.saturating_sub(1).max(10)
                };
            }
        }
        self.dirty = true;
        Some(field)
    }

    // --- live-value getters the window applies on a change ---

    pub(crate) fn theme_name(&self) -> &'static str {
        config::PRESETS[self.theme]
    }
    pub(crate) fn font_size(&self) -> f32 {
        self.font_size
    }
    pub(crate) fn ligatures(&self) -> bool {
        self.ligatures
    }
    pub(crate) fn cursor(&self) -> CursorShape {
        self.cursor
    }
    pub(crate) fn blink(&self) -> bool {
        self.blink
    }
    pub(crate) fn cursor_trail(&self) -> bool {
        self.cursor_trail
    }
    /// The chosen contrast ratio, `None` when off.
    pub(crate) fn min_contrast(&self) -> Option<f32> {
        let r = CONTRAST_STEPS[self.min_contrast];
        (r > 1.0).then_some(r)
    }
    /// The chosen shell's launch path, or `None` for the platform default.
    pub(crate) fn shell_path(&self) -> Option<String> {
        self.shell.checked_sub(1).map(|i| self.shells[i].1.clone())
    }
    pub(crate) fn scrollback(&self) -> usize {
        self.scrollback
    }
    pub(crate) fn copy_html(&self) -> bool {
        self.copy_html
    }
    pub(crate) fn clipboard(&self) -> ClipboardPolicy {
        self.clipboard
    }
    pub(crate) fn click_to_move(&self) -> bool {
        self.click_to_move
    }
    pub(crate) fn bell(&self) -> bool {
        self.bell
    }
    pub(crate) fn padding(&self) -> u32 {
        self.padding
    }
    pub(crate) fn opacity(&self) -> f32 {
        self.opacity
    }
    /// The chosen launch mode, `None` for a normal window.
    pub(crate) fn launch_mode_value(&self) -> Option<LaunchMode> {
        self.launch_mode
    }
    pub(crate) fn cols_value(&self) -> u16 {
        self.cols
    }
    pub(crate) fn rows_value(&self) -> u16 {
        self.rows
    }

    /// The visible rows in display order, for the renderer. A disabled row
    /// swaps its description for the reason it's disabled; while filtering,
    /// each row carries its category's label.
    pub(crate) fn rows(&self) -> Vec<Row> {
        let filtering = self.filter.is_some();
        self.visible()
            .into_iter()
            .map(|(f, cat)| {
                let disabled = self.is_disabled(f);
                Row {
                    field: f,
                    label: f.label(),
                    description: if disabled {
                        "Requires the GPU renderer (launch with --gpu)"
                    } else {
                        f.description()
                    },
                    widget: self.widget(f),
                    next_launch: f.applies_next_launch(),
                    disabled,
                    category: filtering.then_some(cat),
                    modified: self.is_modified(f),
                }
            })
            .collect()
    }

    /// The value widget for `f`.
    fn widget(&self, f: Field) -> Widget {
        match f {
            Field::Theme => Widget::Choice(self.theme_name().to_string()),
            Field::FontSize => Widget::Number(format!("{} px", fmt_px(self.font_size))),
            Field::Ligatures => Widget::Toggle(self.ligatures),
            // The shape previews itself: a full/bottom/left block beside the
            // name (boxdraw synthesizes all three, so no font dependency).
            Field::Cursor => Widget::Choice(format!(
                "{} {}",
                match self.cursor {
                    CursorShape::Block => '\u{2588}',
                    CursorShape::Underline => '\u{2581}',
                    CursorShape::Bar => '\u{258F}',
                },
                cursor_name(self.cursor),
            )),
            Field::Blink => Widget::Toggle(self.blink),
            Field::CursorTrail => Widget::Toggle(self.cursor_trail),
            Field::MinContrast => Widget::Choice(match self.min_contrast() {
                None => "off".to_string(),
                Some(r) => format!("{r}:1"),
            }),
            Field::Shell => Widget::Choice(match self.shell {
                0 => "(default)".to_string(),
                i => self.shells[i - 1].0.clone(),
            }),
            Field::Scrollback => Widget::Number(fmt_thousands(self.scrollback)),
            Field::CopyHtml => Widget::Toggle(self.copy_html),
            Field::Clipboard => Widget::Choice(clipboard_name(self.clipboard).to_string()),
            Field::ClickToMove => Widget::Toggle(self.click_to_move),
            Field::Bell => Widget::Toggle(self.bell),
            Field::Padding => Widget::Number(format!("{} px", self.padding)),
            Field::Opacity => Widget::Number(format!("{}%", (self.opacity * 100.0).round())),
            Field::LaunchMode => Widget::Choice(launch_mode_name(self.launch_mode).to_string()),
            Field::Cols => Widget::Number(self.cols.to_string()),
            Field::Rows => Widget::Number(self.rows.to_string()),
        }
    }

    /// Persistence edits for the whole managed set. A value equal to its
    /// built-in default is written only if the file already names it, so the
    /// page never clutters a minimal config with defaults; choices returning
    /// to "default" (`shell`, `launch_mode`, contrast off) remove the key.
    pub(crate) fn edits(&self) -> Vec<SettingEdit> {
        let mut edits = vec![
            SettingEdit {
                section: "",
                key: "theme",
                value: Some(config::toml_string(self.theme_name())),
                insert: self.theme != 0,
            },
            SettingEdit {
                section: "window",
                key: "font_size",
                value: Some(fmt_px(self.font_size)),
                insert: self.font_size != DEFAULT_FONT_PX,
            },
            SettingEdit {
                section: "window",
                key: "ligatures",
                value: Some(self.ligatures.to_string()),
                insert: !self.ligatures, // default is on
            },
            SettingEdit {
                section: "",
                key: "cursor_style",
                value: Some(config::toml_string(cursor_name(self.cursor))),
                insert: self.cursor != CursorShape::default(),
            },
            SettingEdit {
                section: "",
                key: "cursor_blink",
                value: Some(self.blink.to_string()),
                insert: self.blink,
            },
            SettingEdit {
                section: "",
                key: "cursor_trail",
                value: Some(self.cursor_trail.to_string()),
                insert: self.cursor_trail, // default is off
            },
            match self.min_contrast() {
                Some(r) => SettingEdit {
                    section: "",
                    key: "minimum_contrast",
                    value: Some(r.to_string()),
                    insert: true,
                },
                None => SettingEdit {
                    section: "",
                    key: "minimum_contrast",
                    value: None,
                    insert: false,
                },
            },
            SettingEdit {
                section: "",
                key: "scrollback",
                value: Some(self.scrollback.to_string()),
                insert: self.scrollback != SCROLLBACK_MAX,
            },
            SettingEdit {
                section: "",
                key: "copy_html",
                value: Some(self.copy_html.to_string()),
                insert: !self.copy_html, // default is on
            },
            SettingEdit {
                section: "",
                key: "clipboard",
                value: Some(config::toml_string(clipboard_name(self.clipboard))),
                insert: self.clipboard != ClipboardPolicy::default(),
            },
            SettingEdit {
                section: "",
                key: "click_to_move",
                value: Some(self.click_to_move.to_string()),
                insert: !self.click_to_move, // default is on
            },
            SettingEdit {
                section: "",
                key: "bell",
                value: Some(self.bell.to_string()),
                insert: !self.bell, // default is on
            },
            SettingEdit {
                section: "window",
                key: "padding",
                value: Some(self.padding.to_string()),
                insert: self.padding != DEFAULT_PAD,
            },
            SettingEdit {
                section: "window",
                key: "opacity",
                value: Some(format!("{:.2}", self.opacity)),
                insert: self.opacity != 1.0,
            },
            match self.launch_mode {
                Some(m) => SettingEdit {
                    section: "window",
                    key: "launch_mode",
                    value: Some(config::toml_string(launch_mode_name(Some(m)))),
                    insert: true,
                },
                None => SettingEdit {
                    section: "window",
                    key: "launch_mode",
                    value: None,
                    insert: false,
                },
            },
            SettingEdit {
                section: "window",
                key: "cols",
                value: Some(self.cols.to_string()),
                insert: self.cols != DEFAULT_COLS,
            },
            SettingEdit {
                section: "window",
                key: "rows",
                value: Some(self.rows.to_string()),
                insert: self.rows != DEFAULT_ROWS,
            },
        ];
        edits.push(match self.shell_path() {
            Some(path) => SettingEdit {
                section: "",
                key: "shell",
                value: Some(config::toml_string(&path)),
                insert: true,
            },
            None => SettingEdit { section: "", key: "shell", value: None, insert: false },
        });
        edits
    }
}

/// Step `i` by one within `0..len`, wrapping. `len == 0` stays at `0`.
fn wrap(i: usize, len: usize, forward: bool) -> usize {
    if len == 0 {
        0
    } else if forward {
        (i + 1) % len
    } else {
        (i + len - 1) % len
    }
}

fn cursor_name(c: CursorShape) -> &'static str {
    match c {
        CursorShape::Block => "block",
        CursorShape::Underline => "underline",
        CursorShape::Bar => "bar",
    }
}

/// The config-file spelling of a clipboard policy (also the display text).
fn clipboard_name(p: ClipboardPolicy) -> &'static str {
    match p {
        ClipboardPolicy::Off => "off",
        ClipboardPolicy::WriteOnly => "write-only",
        ClipboardPolicy::ReadWrite => "read-write",
    }
}

/// The config-file spelling of a launch mode (also the display text; `None`
/// displays as "normal" but persists by *removing* the key, since the parser
/// has no "normal" spelling — absence is normal).
fn launch_mode_name(m: Option<LaunchMode>) -> &'static str {
    match m {
        None => "normal",
        Some(LaunchMode::Maximized) => "maximized",
        Some(LaunchMode::Fullscreen) => "fullscreen",
    }
}

/// Format a font size: an integral value as a bare integer, else one decimal.
fn fmt_px(px: f32) -> String {
    if px.fract() == 0.0 { format!("{}", px as i32) } else { format!("{px:.1}") }
}

/// Group digits with commas ("10,000") — display only; the config file
/// still gets the bare integer.
fn fmt_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn shells() -> Vec<DetectedShell> {
        vec![
            DetectedShell { name: "pwsh", path: PathBuf::from("/x/pwsh") },
            DetectedShell { name: "bash", path: PathBuf::from("/bin/bash") },
        ]
    }

    fn seeded() -> Settings {
        Settings::new(&Config::default(), &Theme::default(), 18.0, &shells(), true)
    }

    /// Position the highlight on `f`, switching category as needed.
    fn select_field(s: &mut Settings, f: Field) {
        for (ci, (cat, _)) in CATEGORIES.iter().enumerate() {
            if let Some(ri) = Field::in_category(*cat).iter().position(|&x| x == f) {
                s.set_category(ci);
                s.sel = ri;
                return;
            }
        }
        panic!("field {f:?} not in any category");
    }

    #[test]
    fn every_field_appears_in_exactly_one_category() {
        let all: Vec<Field> = CATEGORIES
            .iter()
            .flat_map(|(c, _)| Field::in_category(*c).iter().copied())
            .collect();
        for f in &all {
            assert_eq!(
                all.iter().filter(|&&x| x == *f).count(),
                1,
                "{f:?} appears exactly once",
            );
        }
        assert_eq!(all.len(), 18, "all fields are reachable from the sidebar");
    }

    #[test]
    fn new_matches_configured_theme_and_shell() {
        let dracula = config::preset("dracula").unwrap();
        let cfg = Config { shell: Some("bash".into()), ..Config::default() };
        let s = Settings::new(&cfg, &dracula, 18.0, &shells(), true);
        assert_eq!(s.theme_name(), "dracula", "theme matched by color");
        assert_eq!(s.shell_path().as_deref(), Some("/bin/bash"), "shell matched by name");
    }

    #[test]
    fn unknown_theme_and_shell_fall_back_to_defaults() {
        // A custom (non-preset) theme shows as "default"; an unrecognized shell
        // resolves to "(default)".
        let custom = Theme { fg: 0x123456, ..Theme::default() };
        let cfg = Config { shell: Some("zsh".into()), ..Config::default() };
        let s = Settings::new(&cfg, &custom, 18.0, &shells(), true);
        assert_eq!(s.theme_name(), "default");
        assert_eq!(s.shell_path(), None);
    }

    #[test]
    fn theme_change_wraps_both_directions() {
        let mut s = seeded(); // sel defaults to Theme (Appearance row 0)
        assert_eq!(s.theme_name(), "default");
        assert_eq!(s.change(true), Some(Field::Theme));
        assert_eq!(s.theme_name(), config::PRESETS[1]);
        // Wrap backward off the start to the last preset.
        let mut s = seeded();
        s.change(false);
        assert_eq!(s.theme_name(), *config::PRESETS.last().unwrap());
        assert!(s.dirty);
    }

    #[test]
    fn cursor_cycles_block_underline_bar() {
        let mut s = seeded();
        select_field(&mut s, Field::Cursor);
        assert_eq!(s.cursor(), CursorShape::Block);
        s.change(true);
        assert_eq!(s.cursor(), CursorShape::Underline);
        s.change(true);
        assert_eq!(s.cursor(), CursorShape::Bar);
        s.change(true);
        assert_eq!(s.cursor(), CursorShape::Block, "wraps");
    }

    #[test]
    fn toggles_ignore_direction() {
        let mut s = seeded();
        select_field(&mut s, Field::Blink);
        assert!(!s.blink());
        s.change(false); // a toggle flips regardless of direction
        assert!(s.blink());
        select_field(&mut s, Field::Ligatures);
        assert!(s.ligatures());
        s.change(true);
        assert!(!s.ligatures());
    }

    #[test]
    fn font_step_rounds_a_fractional_seed_to_integers() {
        let mut s = Settings::new(&Config::default(), &Theme::default(), 18.5, &shells(), true);
        select_field(&mut s, Field::FontSize);
        s.change(true);
        assert_eq!(s.font_size(), 19.0, "stepping up lands on a whole point");
        let mut s = Settings::new(&Config::default(), &Theme::default(), 18.5, &shells(), true);
        select_field(&mut s, Field::FontSize);
        s.change(false);
        assert_eq!(s.font_size(), 18.0, "stepping down lands on a whole point");
    }

    #[test]
    fn font_and_scrollback_clamp_at_bounds() {
        let mut s = seeded();
        select_field(&mut s, Field::FontSize);
        for _ in 0..100 {
            s.change(false);
        }
        assert_eq!(s.font_size(), FONT_MIN, "font clamps to the floor");
        select_field(&mut s, Field::Scrollback);
        let start = s.scrollback();
        s.change(false);
        assert_eq!(s.scrollback(), start - SCROLLBACK_STEP);
        for _ in 0..10_000 {
            s.change(false);
        }
        assert_eq!(s.scrollback(), 0, "scrollback floors at 0 without underflow");
    }

    #[test]
    fn shell_choice_wraps_through_default() {
        let mut s = seeded();
        select_field(&mut s, Field::Shell);
        assert_eq!(s.shell_path(), None, "starts at (default)");
        s.change(true);
        assert_eq!(s.shell_path().as_deref(), Some("/x/pwsh"));
        s.change(true);
        assert_eq!(s.shell_path().as_deref(), Some("/bin/bash"));
        s.change(true);
        assert_eq!(s.shell_path(), None, "wraps back to (default)");
    }

    #[test]
    fn clipboard_cycles_and_persists_the_config_spelling() {
        let mut s = seeded();
        select_field(&mut s, Field::Clipboard);
        assert_eq!(s.clipboard(), ClipboardPolicy::WriteOnly, "seeds to the default");
        s.change(true);
        assert_eq!(s.clipboard(), ClipboardPolicy::ReadWrite);
        let edit = s.edits().into_iter().find(|e| e.key == "clipboard").unwrap();
        assert!(edit.insert);
        assert_eq!(edit.value.as_deref(), Some("\"read-write\""));
    }

    #[test]
    fn launch_mode_normal_removes_the_key() {
        let mut s = seeded();
        select_field(&mut s, Field::LaunchMode);
        s.change(true); // normal -> maximized
        let edit = s.edits().into_iter().find(|e| e.key == "launch_mode").unwrap();
        assert!(edit.insert && edit.value.as_deref() == Some("\"maximized\""));
        s.change(true); // -> fullscreen
        s.change(true); // wraps -> normal
        let edit = s.edits().into_iter().find(|e| e.key == "launch_mode").unwrap();
        assert!(edit.value.is_none() && !edit.insert, "normal removes the key");
    }

    #[test]
    fn opacity_snaps_to_the_step_grid_and_floors_above_zero() {
        let cfg = Config { opacity: Some(0.87), ..Config::default() };
        let mut s = Settings::new(&cfg, &Theme::default(), 18.0, &shells(), true);
        select_field(&mut s, Field::Opacity);
        s.change(true);
        assert_eq!(s.opacity(), 0.90, "0.87 snaps up to the 0.05 grid");
        for _ in 0..100 {
            s.change(false);
        }
        assert_eq!(s.opacity(), OPACITY_MIN, "floors above zero so the window stays visible");
    }

    #[test]
    fn min_contrast_off_removes_the_key() {
        let cfg = Config { minimum_contrast: Some(4.5), ..Config::default() };
        let mut s = Settings::new(&cfg, &Theme::default(), 18.0, &shells(), true);
        assert_eq!(s.min_contrast(), Some(4.5), "seeds to the configured step");
        select_field(&mut s, Field::MinContrast);
        s.change(true); // 4.5 -> 7.0
        assert_eq!(s.min_contrast(), Some(7.0));
        s.change(true); // wraps -> off
        assert_eq!(s.min_contrast(), None);
        let edit = s.edits().into_iter().find(|e| e.key == "minimum_contrast").unwrap();
        assert!(edit.value.is_none() && !edit.insert, "off removes the key");
    }

    #[test]
    fn cols_and_rows_step_within_bounds() {
        let mut s = seeded();
        select_field(&mut s, Field::Cols);
        s.change(true);
        let edit = s.edits().into_iter().find(|e| e.key == "cols").unwrap();
        assert!(edit.insert && edit.value.as_deref() == Some("85"));
        select_field(&mut s, Field::Rows);
        for _ in 0..1000 {
            s.change(false);
        }
        let s2 = s.rows()[Field::in_category(Category::Window)
            .iter()
            .position(|&f| f == Field::Rows)
            .unwrap()]
        .widget
        .clone();
        assert_eq!(s2, Widget::Number("10".into()), "rows floor at 10");
    }

    #[test]
    fn category_navigation_resets_selection_and_scroll() {
        let mut s = seeded();
        s.sel = 3;
        s.scroll = 2;
        s.cycle_category(true);
        assert_eq!(s.category(), Category::Terminal);
        assert_eq!((s.sel, s.scroll), (0, 0));
        s.cycle_category(false);
        assert_eq!(s.category(), Category::Appearance);
        // Wrap backward from the first category to the last.
        s.cycle_category(false);
        assert_eq!(s.category(), Category::Window);
    }

    #[test]
    fn ensure_visible_scrolls_the_selection_into_the_window() {
        let mut s = seeded(); // Appearance: 7 rows
        s.sel = 6;
        s.ensure_visible(3);
        assert_eq!(s.scroll, 4, "selection at the bottom of a 3-row window");
        s.sel = 0;
        s.ensure_visible(3);
        assert_eq!(s.scroll, 0, "scrolls back up");
    }

    #[test]
    fn scroll_by_clamps_to_the_list() {
        let mut s = seeded(); // 7 rows
        s.scroll_by(100, 3);
        assert_eq!(s.scroll, 4, "at most len - visible");
        s.scroll_by(-100, 3);
        assert_eq!(s.scroll, 0);
    }

    #[test]
    fn rows_match_field_count_and_widgets() {
        let s = seeded();
        assert_eq!(s.rows().len(), s.len());
        let rows = s.rows();
        assert_eq!(rows[0].label, "Color theme");
        assert_eq!(rows[0].widget, Widget::Choice("default".into()));
        assert_eq!(rows[1].widget, Widget::Number("18 px".into()));
        assert_eq!(rows[2].widget, Widget::Toggle(true), "ligatures default on");
    }

    #[test]
    fn edits_omit_defaults_but_persist_overrides() {
        // All defaults: every managed edit is a non-inserting (or removing) one.
        let s = seeded();
        let edits = s.edits();
        assert!(
            edits.iter().all(|e| !e.insert),
            "nothing inserted when everything is at its default",
        );
        // Change theme + font: those become inserting edits with the new value.
        let mut s = seeded();
        select_field(&mut s, Field::Theme);
        s.change(true); // theme -> PRESETS[1]
        select_field(&mut s, Field::FontSize);
        s.change(true); // font 18 -> 19
        let edits = s.edits();
        let theme = edits.iter().find(|e| e.key == "theme").unwrap();
        assert!(theme.insert && theme.value == Some(config::toml_string(config::PRESETS[1])));
        let font = edits.iter().find(|e| e.key == "font_size").unwrap();
        assert!(font.insert && font.value.as_deref() == Some("19"));
    }

    #[test]
    fn disabled_opacity_refuses_changes_and_says_why() {
        // CPU renderer: opacity is inert, so the row is disabled.
        let mut s =
            Settings::new(&Config::default(), &Theme::default(), 18.0, &shells(), false);
        select_field(&mut s, Field::Opacity);
        let row = &s.rows()[s.sel];
        assert!(row.disabled);
        assert!(row.description.contains("GPU"), "description says why: {}", row.description);
        s.change(true);
        assert_eq!(s.opacity(), 1.0, "the value refuses to move");
        assert!(!s.dirty, "a refused change is not a modification");
        // GPU renderer: same field is live.
        let mut s =
            Settings::new(&Config::default(), &Theme::default(), 18.0, &shells(), true);
        select_field(&mut s, Field::Opacity);
        assert!(!s.rows()[s.sel].disabled);
        s.change(false);
        assert_eq!(s.opacity(), 0.95);
    }

    #[test]
    fn shift_big_step_multiplies_numbers_but_not_choices() {
        let mut s = seeded();
        select_field(&mut s, Field::Scrollback);
        let start = s.scrollback();
        s.change_by(false, true);
        assert_eq!(s.scrollback(), start - 10_000, "10 small steps at once");
        // A choice ignores `big` — ten entries onward is not a jump anyone means.
        select_field(&mut s, Field::Theme);
        s.change_by(true, true);
        assert_eq!(s.theme_name(), config::PRESETS[1], "one entry, not ten");
    }

    #[test]
    fn home_end_jump_numbers_to_their_bounds() {
        let mut s = seeded();
        select_field(&mut s, Field::FontSize);
        s.jump(true);
        assert_eq!(s.font_size(), FONT_MAX);
        s.jump(false);
        assert_eq!(s.font_size(), FONT_MIN);
        // Scrollback: the widest range still terminates at both bounds.
        select_field(&mut s, Field::Scrollback);
        s.jump(false);
        assert_eq!(s.scrollback(), 0);
        s.jump(true);
        assert_eq!(s.scrollback(), SCROLLBACK_CEIL);
        // Choices don't jump — they wrap, so there's no bound to land on.
        select_field(&mut s, Field::Theme);
        let before = s.theme_name();
        s.jump(true);
        assert_eq!(s.theme_name(), before);
    }

    #[test]
    fn scrollback_displays_with_thousands_separators() {
        let mut s = seeded();
        select_field(&mut s, Field::Scrollback);
        assert_eq!(s.rows()[s.sel].widget, Widget::Number(fmt_thousands(SCROLLBACK_MAX)));
        assert_eq!(fmt_thousands(0), "0");
        assert_eq!(fmt_thousands(999), "999");
        assert_eq!(fmt_thousands(10_000), "10,000");
        assert_eq!(fmt_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn launch_time_fields_carry_the_next_launch_tag() {
        let mut s = seeded();
        s.set_category(2); // Window
        let rows = s.rows();
        let tagged: Vec<&str> =
            rows.iter().filter(|r| r.next_launch).map(|r| r.label).collect();
        assert_eq!(tagged, vec!["Launch mode", "Columns", "Rows"]);
        // And their descriptions no longer duplicate the caveat.
        for r in rows.iter().filter(|r| r.next_launch) {
            assert!(!r.description.contains("launch"), "{}", r.description);
        }
    }

    #[test]
    fn filter_matches_labels_and_descriptions_across_categories() {
        let mut s = seeded();
        for c in "cursor".chars() {
            s.filter_input(c);
        }
        // Matches span Appearance (shape/blink/trail) and Terminal
        // (click-to-move mentions "cursor" in label).
        let rows = s.rows();
        assert!(rows.len() >= 4, "shape, blink, trail, click-to-move: {}", rows.len());
        assert!(rows.iter().all(|r| r.category.is_some()), "results carry category tags");
        assert!(
            rows.iter().any(|r| r.category == Some("Terminal")),
            "results cross category boundaries",
        );
        // The query narrows as it grows...
        for c in " tra".chars() {
            s.filter_input(c);
        }
        assert_eq!(s.rows().len(), 1, "\"cursor tra\" matches only the trail");
        // ...and a nonsense query matches nothing without panicking anywhere.
        for c in "zzz".chars() {
            s.filter_input(c);
        }
        assert_eq!(s.len(), 0);
        assert_eq!(s.field(), None);
        assert_eq!(s.change(true), None, "no row, no change");
        s.move_sel(true); // doesn't wrap into thin air
        // Clearing restores the category view.
        s.clear_filter();
        assert_eq!(s.len(), Field::in_category(Category::Appearance).len());
        assert!(s.rows()[0].category.is_none());
    }

    #[test]
    fn changing_a_value_from_search_results_touches_the_right_field() {
        let mut s = seeded();
        for c in "bell".chars() {
            s.filter_input(c);
        }
        assert_eq!(s.field(), Some(Field::Bell));
        assert!(s.bell());
        assert_eq!(s.change(true), Some(Field::Bell));
        assert!(!s.bell(), "the toggle flipped even though Bell lives in another category");
    }

    #[test]
    fn sidebar_choice_drops_the_filter() {
        let mut s = seeded();
        s.filter_input('x');
        s.set_category(0); // same index as current — still exits the search
        assert!(s.filter.is_none());
        assert_eq!(s.len(), Field::in_category(Category::Appearance).len());
    }

    #[test]
    fn edit_commits_a_typed_number_with_clamping() {
        let mut s = seeded();
        select_field(&mut s, Field::Scrollback);
        assert!(s.begin_edit(Some('2')));
        for c in "5000".chars() {
            s.edit_input(c);
        }
        assert_eq!(s.editing.as_deref(), Some("25000"));
        assert_eq!(s.commit_edit(), Some(Field::Scrollback));
        assert_eq!(s.scrollback(), 25_000);
        assert!(s.dirty);
        // Out-of-range input clamps instead of erroring.
        assert!(s.begin_edit(None));
        s.editing = Some("99999999".into());
        s.commit_edit();
        assert_eq!(s.scrollback(), 1_000_000);
        // Cols clamp to their floor too.
        select_field(&mut s, Field::Cols);
        s.begin_edit(Some('3'));
        assert_eq!(s.commit_edit(), Some(Field::Cols));
        assert_eq!(s.cols_value(), 20, "a bare '3' clamps up to the 20-column floor");
    }

    #[test]
    fn edit_rejects_garbage_and_cancels_cleanly() {
        let mut s = seeded();
        select_field(&mut s, Field::FontSize);
        assert!(s.begin_edit(None));
        assert_eq!(s.editing.as_deref(), Some("18"), "Enter seeds the current value");
        s.edit_input('x'); // non-digit: ignored
        assert_eq!(s.editing.as_deref(), Some("18"));
        s.edit_backspace();
        s.edit_backspace();
        assert_eq!(s.editing.as_deref(), Some(""));
        assert_eq!(s.commit_edit(), None, "an empty buffer cancels");
        assert_eq!(s.font_size(), 18.0);
        assert!(!s.dirty, "a cancelled edit is not a modification");
        // Cancel puts the buffer down without touching the value.
        s.begin_edit(Some('9'));
        s.cancel_edit();
        assert_eq!(s.editing, None);
        assert_eq!(s.font_size(), 18.0);
    }

    #[test]
    fn edit_only_starts_on_enabled_number_rows() {
        let mut s = seeded();
        select_field(&mut s, Field::Theme);
        assert!(!s.begin_edit(None), "a choice is not typable");
        // Disabled opacity refuses an edit too.
        let mut s =
            Settings::new(&Config::default(), &Theme::default(), 18.0, &shells(), false);
        select_field(&mut s, Field::Opacity);
        assert!(!s.begin_edit(Some('5')));
    }

    #[test]
    fn opacity_edit_is_typed_in_percent() {
        let mut s = seeded();
        select_field(&mut s, Field::Opacity);
        assert!(s.begin_edit(None));
        assert_eq!(s.editing.as_deref(), Some("100"), "seeded as percent");
        s.editing = Some("85".into());
        assert_eq!(s.commit_edit(), Some(Field::Opacity));
        assert_eq!(s.opacity(), 0.85);
    }

    #[test]
    fn modified_markers_track_the_persistence_gate() {
        let mut s = seeded();
        // Everything at defaults: no dots anywhere.
        assert!(s.rows().iter().all(|r| !r.modified));
        for (cat, _) in CATEGORIES {
            assert!(!s.category_modified(cat));
        }
        // Flip one Appearance toggle: its row and its category light up,
        // the other categories stay clean.
        select_field(&mut s, Field::Blink);
        s.change(true);
        assert!(s.rows()[s.sel].modified);
        assert!(s.category_modified(Category::Appearance));
        assert!(!s.category_modified(Category::Terminal));
        assert!(!s.category_modified(Category::Window));
        // Flipping it back clears the dot — the marker tracks "differs from
        // default", not "was touched".
        s.change(true);
        assert!(!s.rows()[s.sel].modified);
        assert!(!s.category_modified(Category::Appearance));
    }

    #[test]
    fn cursor_choice_previews_its_shape() {
        let mut s = seeded();
        select_field(&mut s, Field::Cursor);
        assert_eq!(s.rows()[s.sel].widget, Widget::Choice("\u{2588} block".into()));
        s.change(true);
        assert_eq!(s.rows()[s.sel].widget, Widget::Choice("\u{2581} underline".into()));
        s.change(true);
        assert_eq!(s.rows()[s.sel].widget, Widget::Choice("\u{258F} bar".into()));
    }

    #[test]
    fn default_shell_edit_removes_the_key() {
        let s = seeded(); // shell at "(default)"
        let shell = s.edits().into_iter().find(|e| e.key == "shell").unwrap();
        assert!(shell.value.is_none() && !shell.insert, "default shell removes any existing line");
    }
}
