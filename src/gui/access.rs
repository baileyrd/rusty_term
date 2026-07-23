//! AccessKit integration (C20): exposes the visible screen text and cursor
//! position of the focused pane to assistive technology (screen readers),
//! via the `winit` window's accessibility tree.
//!
//! No terminal in the field researched for `docs/research/gap-analysis-2026-07.md`
//! has meaningful screen-reader support either, so this is differentiation
//! rather than catch-up — a plain-text dump of the visible grid plus a
//! polite live region announcing the cursor's row/column, refreshed only
//! when either actually changes (a blink-only redraw doesn't re-announce).

use accesskit::{Live, Node, NodeId, Role, Tree, TreeId, TreeUpdate};

use crate::core::{Cell, Grid, WIDE_TRAILER};

const ROOT_ID: NodeId = NodeId(0);
const CURSOR_ID: NodeId = NodeId(1);

/// Per-window AccessKit state: the platform adapter plus the last content
/// pushed, so unchanged frames don't rebuild or re-announce identical text.
pub(crate) struct AccessState {
    adapter: accesskit_winit::Adapter,
    last_rows: Vec<String>,
    last_cursor: (usize, usize),
    last_title: String,
    /// `false` until the first [`AccessState::sync`] call, so that call
    /// always pushes even if the (empty) starting state happens to match.
    synced_once: bool,
}

impl AccessState {
    pub(crate) fn new(adapter: accesskit_winit::Adapter) -> Self {
        Self {
            adapter,
            last_rows: Vec::new(),
            last_cursor: (0, 0),
            last_title: String::new(),
            synced_once: false,
        }
    }

    /// Feed every `winit` window event to the adapter — required so it can
    /// track platform-specific accessibility activation/queries.
    pub(crate) fn process_event(
        &mut self,
        window: &winit::window::Window,
        event: &winit::event::WindowEvent,
    ) {
        self.adapter.process_event(window, event);
    }

    /// Push an updated tree if the focused pane's visible text, cursor, or
    /// title changed since the last call. Safe to call on every redraw: the
    /// row/cursor extraction below is cheap (grid-sized, not screen-sized),
    /// and `update_if_active`'s closure only actually runs — and only then
    /// builds the [`TreeUpdate`] — while assistive tech is attached.
    pub(crate) fn sync(&mut self, title: &str, grid: &Grid) {
        let rows = visible_rows(grid);
        let cursor = grid.cursor;
        if self.synced_once
            && unchanged(
                &self.last_rows,
                self.last_cursor,
                &self.last_title,
                &rows,
                cursor,
                title,
            )
        {
            return;
        }
        self.last_rows = rows;
        self.last_cursor = cursor;
        self.last_title = title.to_string();
        self.synced_once = true;
        self.push_current();
    }

    /// Rebuild and push the full tree from the last-synced content,
    /// unconditionally. Used to answer AccessKit's `InitialTreeRequested`
    /// event, which can arrive well after the content it should describe was
    /// last synced (a screen reader attaching mid-session).
    pub(crate) fn push_initial(&mut self) {
        self.push_current();
    }

    fn push_current(&mut self) {
        let rows = &self.last_rows;
        let cursor = self.last_cursor;
        let title = self.last_title.as_str();
        self.adapter
            .update_if_active(|| build_tree(title, rows, cursor));
    }
}

/// One string per visible row, top to bottom, trailing blanks trimmed —
/// the same per-cell-to-text conversion the core parser uses, duplicated
/// here in miniature since `crate::core::grid`'s own `row_text` isn't
/// reachable outside `core` (its module is private; only selected types are
/// re-exported).
fn visible_rows(grid: &Grid) -> Vec<String> {
    (0..grid.rows)
        .map(|y| {
            let start = y * grid.cols;
            row_text(&grid.cells[start..start + grid.cols], &grid.clusters)
        })
        .collect()
}

/// Whether a newly-extracted `(rows, cursor, title)` triple is identical to
/// the last one synced — split out from [`AccessState::sync`] so it's
/// unit-testable without a real `accesskit_winit::Adapter` (which needs a
/// live winit window to construct, so `AccessState` itself can't be built
/// headlessly — same limitation as the rest of this module, see `window.rs`).
fn unchanged(
    prev_rows: &[String],
    prev_cursor: (usize, usize),
    prev_title: &str,
    rows: &[String],
    cursor: (usize, usize),
    title: &str,
) -> bool {
    prev_rows == rows && prev_cursor == cursor && prev_title == title
}

fn row_text(cells: &[Cell], clusters: &[String]) -> String {
    let mut s = String::new();
    for cell in cells {
        if cell.flags & WIDE_TRAILER != 0 {
            continue;
        }
        s.push(cell.ch);
        if cell.cluster != 0
            && let Some(suffix) = clusters.get((cell.cluster - 1) as usize)
        {
            s.push_str(suffix);
        }
    }
    s.trim_end().to_string()
}

/// Build the accessibility tree: a `Terminal`-role root node carrying the
/// full visible screen as its text value, and a `Status` child announcing
/// the cursor's 1-based row/column as a polite live region.
fn build_tree(title: &str, rows: &[String], cursor: (usize, usize)) -> TreeUpdate {
    let mut root = Node::new(Role::Terminal);
    root.set_label(if title.is_empty() {
        "rusty_term"
    } else {
        title
    });
    root.set_value(rows.join("\n"));
    root.set_children(vec![CURSOR_ID]);

    let (col, row) = cursor;
    let mut cursor_node = Node::new(Role::Status);
    cursor_node.set_value(format!("Row {}, column {}", row + 1, col + 1));
    cursor_node.set_live(Live::Polite);

    TreeUpdate {
        nodes: vec![(ROOT_ID, root), (CURSOR_ID, cursor_node)],
        tree: Some(Tree::new(ROOT_ID)),
        tree_id: TreeId::ROOT,
        focus: ROOT_ID,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid_with(rows: &[&str], cursor: (usize, usize)) -> Grid {
        let cols = rows.iter().map(|r| r.chars().count()).max().unwrap_or(1);
        let mut g = Grid::new(cols.max(1), rows.len().max(1));
        for (y, line) in rows.iter().enumerate() {
            for (x, ch) in line.chars().enumerate() {
                g.cells[y * g.cols + x].ch = ch;
            }
        }
        g.cursor = cursor;
        g
    }

    #[test]
    fn visible_rows_trims_trailing_blanks_per_row() {
        let g = grid_with(&["hello", "hi"], (0, 0));
        assert_eq!(
            visible_rows(&g),
            vec!["hello".to_string(), "hi".to_string()]
        );
    }

    #[test]
    fn unchanged_true_for_identical_content() {
        let rows = vec!["hello".to_string()];
        assert!(unchanged(&rows, (2, 0), "t", &rows, (2, 0), "t"));
    }

    #[test]
    fn unchanged_false_when_cursor_moves() {
        let rows = vec!["hello".to_string()];
        assert!(!unchanged(&rows, (2, 0), "t", &rows, (3, 0), "t"));
    }

    #[test]
    fn unchanged_false_when_text_or_title_differs() {
        let rows = vec!["hello".to_string()];
        let rows2 = vec!["world".to_string()];
        assert!(!unchanged(&rows, (0, 0), "t", &rows2, (0, 0), "t"));
        assert!(!unchanged(&rows, (0, 0), "t", &rows, (0, 0), "other"));
    }

    #[test]
    fn build_tree_reports_one_based_cursor_position() {
        let update = build_tree("my title", &["abc".to_string()], (2, 0));
        let (_, cursor_node) = update
            .nodes
            .iter()
            .find(|(id, _)| *id == CURSOR_ID)
            .expect("cursor node present");
        assert_eq!(cursor_node.value(), Some("Row 1, column 3"));
        let (_, root) = update
            .nodes
            .iter()
            .find(|(id, _)| *id == ROOT_ID)
            .expect("root node present");
        assert_eq!(root.value(), Some("abc"));
        assert_eq!(root.label(), Some("my title"));
    }

    #[test]
    fn build_tree_falls_back_to_default_label_for_empty_title() {
        let update = build_tree("", &[], (0, 0));
        let (_, root) = update
            .nodes
            .iter()
            .find(|(id, _)| *id == ROOT_ID)
            .expect("root node present");
        assert_eq!(root.label(), Some("rusty_term"));
    }
}
