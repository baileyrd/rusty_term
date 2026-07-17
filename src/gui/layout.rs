//! Split-pane layout for a tab: a binary tree of panes tiled over the tab's
//! cell area.
//!
//! Each leaf is a pane id; each split divides its area in two (with a one-cell
//! divider between) either side-by-side ([`Dir::Vertical`]) or stacked
//! ([`Dir::Horizontal`]). The tree is pure geometry over ids, so the window's
//! pane lifecycle (one shell per id) stays separate and this is unit-testable
//! without a GUI.

/// Which way a split divides its area.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    /// A horizontal divider: the two children stack (top / bottom).
    Horizontal,
    /// A vertical divider: the two children sit side by side (left / right).
    Vertical,
}

/// A rectangle in terminal cells: top-left `(col, row)` and size `cols × rows`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub col: usize,
    pub row: usize,
    pub cols: usize,
    pub rows: usize,
}

impl Rect {
    pub fn new(col: usize, row: usize, cols: usize, rows: usize) -> Self {
        Self {
            col,
            row,
            cols,
            rows,
        }
    }

    pub(crate) fn contains(&self, col: usize, row: usize) -> bool {
        col >= self.col
            && col < self.col + self.cols
            && row >= self.row
            && row < self.row + self.rows
    }
}

enum Node {
    Leaf(u64),
    Split {
        dir: Dir,
        ratio: f32,
        a: Box<Node>,
        b: Box<Node>,
    },
}

impl Node {
    /// The first (top-left-most) leaf id in this subtree — a sensible focus
    /// target after a sibling closes.
    fn first(&self) -> u64 {
        match self {
            Node::Leaf(id) => *id,
            Node::Split { a, .. } => a.first(),
        }
    }

    fn collect(&self, out: &mut Vec<u64>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { a, b, .. } => {
                a.collect(out);
                b.collect(out);
            }
        }
    }

    /// Replace the leaf for `target` with a split of `[target, new]`.
    fn split(&mut self, target: u64, new: u64, dir: Dir) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                *self = Node::Split {
                    dir,
                    ratio: 0.5,
                    a: Box::new(Node::Leaf(target)),
                    b: Box::new(Node::Leaf(new)),
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => a.split(target, new, dir) || b.split(target, new, dir),
        }
    }

    /// If either child is `Leaf(id)`, collapse this split into the *other*
    /// child (the sibling absorbs the closed pane's area). Returns whether a
    /// collapse happened here.
    fn close_child(&mut self, id: u64) -> bool {
        let Node::Split { a, b, .. } = self else {
            return false;
        };
        if matches!(**a, Node::Leaf(x) if x == id) {
            *self = *std::mem::replace(b, Box::new(Node::Leaf(0)));
            return true;
        }
        if matches!(**b, Node::Leaf(x) if x == id) {
            *self = *std::mem::replace(a, Box::new(Node::Leaf(0)));
            return true;
        }
        // Recurse; either side may contain the target deeper down.
        let (a, b) = match self {
            Node::Split { a, b, .. } => (a, b),
            _ => unreachable!(),
        };
        a.close_child(id) || b.close_child(id)
    }

    /// Whether pane `id` is anywhere in this subtree.
    fn contains(&self, id: u64) -> bool {
        match self {
            Node::Leaf(x) => *x == id,
            Node::Split { a, b, .. } => a.contains(id) || b.contains(id),
        }
    }

    /// Grow (`delta > 0`) or shrink pane `id` along `dir` by adjusting the
    /// ratio of the nearest enclosing split of that axis — deepest first, so
    /// the boundary closest to the pane moves. Returns whether any split
    /// changed. The pane's side decides the sign: growing a first (`a`)
    /// child raises the ratio, growing a second (`b`) child lowers it.
    fn resize(&mut self, id: u64, dir: Dir, delta: f32) -> bool {
        let Node::Split {
            dir: d,
            ratio,
            a,
            b,
        } = self
        else {
            return false;
        };
        let (child, sign): (&mut Node, f32) = if a.contains(id) {
            (a, 1.0)
        } else if b.contains(id) {
            (b, -1.0)
        } else {
            return false;
        };
        if child.resize(id, dir, delta) {
            return true;
        }
        if *d == dir {
            *ratio = (*ratio + sign * delta).clamp(0.1, 0.9);
            return true;
        }
        false
    }

    fn rects(&self, area: Rect, out: &mut Vec<(u64, Rect)>) {
        match self {
            Node::Leaf(id) => out.push((*id, area)),
            Node::Split { dir, ratio, a, b } => {
                let (ra, rb) = split_area(area, *dir, *ratio);
                a.rects(ra, out);
                b.rects(rb, out);
            }
        }
    }
}

/// Split `area` into two sub-areas for a [`Dir`] at `ratio`, reserving one cell
/// between them for the divider. Degenerates gracefully when the area is too
/// small to divide (the first child takes it all).
fn split_area(area: Rect, dir: Dir, ratio: f32) -> (Rect, Rect) {
    match dir {
        Dir::Vertical => {
            if area.cols < 2 {
                return (area, Rect::new(area.col, area.row, 0, area.rows));
            }
            let usable = area.cols - 1; // one column for the divider
            let a_cols = ((usable as f32 * ratio).round() as usize).clamp(1, usable - 1);
            let b_cols = usable - a_cols;
            (
                Rect::new(area.col, area.row, a_cols, area.rows),
                Rect::new(area.col + a_cols + 1, area.row, b_cols, area.rows),
            )
        }
        Dir::Horizontal => {
            if area.rows < 2 {
                return (area, Rect::new(area.col, area.row, area.cols, 0));
            }
            let usable = area.rows - 1; // one row for the divider
            let a_rows = ((usable as f32 * ratio).round() as usize).clamp(1, usable - 1);
            let b_rows = usable - a_rows;
            (
                Rect::new(area.col, area.row, area.cols, a_rows),
                Rect::new(area.col, area.row + a_rows + 1, area.cols, b_rows),
            )
        }
    }
}

/// A tab's pane layout.
pub struct Layout {
    root: Node,
}

impl Layout {
    /// A layout with a single full-area pane.
    pub fn single(id: u64) -> Self {
        Self {
            root: Node::Leaf(id),
        }
    }

    /// Split the `target` pane, giving its new sibling `new`. No-op (returns
    /// `false`) if `target` isn't present.
    pub fn split(&mut self, target: u64, new: u64, dir: Dir) -> bool {
        self.root.split(target, new, dir)
    }

    /// Remove pane `id`, collapsing its split into the sibling. Returns the pane
    /// to focus next (a neighbor), or `None` if `id` was the only pane.
    pub fn close(&mut self, id: u64) -> Option<u64> {
        if matches!(self.root, Node::Leaf(x) if x == id) {
            return None;
        }
        self.root.close_child(id);
        Some(self.root.first())
    }

    /// Every pane and its rectangle within `area`, in tree (left-to-right,
    /// top-to-bottom) order.
    pub fn rects(&self, area: Rect) -> Vec<(u64, Rect)> {
        let mut out = Vec::new();
        self.root.rects(area, &mut out);
        out
    }

    /// All pane ids in tree order.
    pub fn ids(&self) -> Vec<u64> {
        let mut out = Vec::new();
        self.root.collect(&mut out);
        out
    }

    /// Grow (`delta > 0`) or shrink pane `id` along `dir` (see
    /// [`Node::resize`]). Returns whether a split boundary moved.
    pub fn resize(&mut self, id: u64, dir: Dir, delta: f32) -> bool {
        self.root.resize(id, dir, delta)
    }

    /// The pane after (`forward`) or before `current` in tree order, wrapping.
    /// Falls back to the first pane if `current` isn't found.
    pub fn cycle(&self, current: u64, forward: bool) -> u64 {
        let ids = self.ids();
        let n = ids.len();
        match ids.iter().position(|&id| id == current) {
            Some(i) if forward => ids[(i + 1) % n],
            Some(i) => ids[(i + n - 1) % n],
            None => ids[0],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_pane_fills_the_area() {
        let l = Layout::single(1);
        let area = Rect::new(0, 1, 80, 24);
        assert_eq!(l.rects(area), vec![(1, area)]);
        assert_eq!(l.ids(), vec![1]);
    }

    #[test]
    fn vertical_split_tiles_side_by_side_with_a_divider() {
        let mut l = Layout::single(1);
        assert!(l.split(1, 2, Dir::Vertical));
        let rects = l.rects(Rect::new(0, 0, 81, 24));
        // 81 cols: 1 divider, 80 usable -> 40 / 40, divider at col 40.
        assert_eq!(
            rects,
            vec![(1, Rect::new(0, 0, 40, 24)), (2, Rect::new(41, 0, 40, 24))]
        );
    }

    #[test]
    fn horizontal_split_stacks_with_a_divider() {
        let mut l = Layout::single(1);
        assert!(l.split(1, 2, Dir::Horizontal));
        let rects = l.rects(Rect::new(0, 0, 80, 25));
        // 25 rows: 1 divider, 24 usable -> 12 / 12, divider at row 12.
        assert_eq!(
            rects,
            vec![(1, Rect::new(0, 0, 80, 12)), (2, Rect::new(0, 13, 80, 12))]
        );
    }

    #[test]
    fn nested_splits_subdivide_the_right_pane() {
        let mut l = Layout::single(1);
        l.split(1, 2, Dir::Vertical); // [1 | 2]
        l.split(2, 3, Dir::Horizontal); // 2 becomes [2 / 3]
        assert_eq!(l.ids(), vec![1, 2, 3]);
        let rects = l.rects(Rect::new(0, 0, 81, 25));
        // Left half is pane 1 full height; right half splits 2 over 3.
        assert_eq!(rects[0], (1, Rect::new(0, 0, 40, 25)));
        assert_eq!(rects[1].0, 2);
        assert_eq!(rects[2].0, 3);
        assert_eq!(rects[1].1.col, 41);
        assert_eq!(rects[2].1.col, 41);
        assert!(rects[2].1.row > rects[1].1.row); // 3 is below 2
    }

    #[test]
    fn closing_collapses_into_the_sibling() {
        let mut l = Layout::single(1);
        l.split(1, 2, Dir::Vertical);
        l.split(2, 3, Dir::Horizontal);
        // Close 3: its split collapses, 2 reclaims the right half.
        assert_eq!(l.close(3), Some(1));
        assert_eq!(l.ids(), vec![1, 2]);
        let rects = l.rects(Rect::new(0, 0, 81, 24));
        assert_eq!(
            rects,
            vec![(1, Rect::new(0, 0, 40, 24)), (2, Rect::new(41, 0, 40, 24))]
        );
        // Close 1: pane 2 takes the whole area.
        assert_eq!(l.close(1), Some(2));
        assert_eq!(
            l.rects(Rect::new(0, 0, 80, 24)),
            vec![(2, Rect::new(0, 0, 80, 24))]
        );
        // Close the last pane: nothing left.
        assert_eq!(l.close(2), None);
    }

    #[test]
    fn pane_at_and_cycle() {
        let mut l = Layout::single(1);
        l.split(1, 2, Dir::Vertical);
        let area = Rect::new(0, 0, 81, 24);
        // Hit-testing goes through rects + Rect::contains (the window's
        // zoom-aware Tab::rects wraps the same primitive).
        let at = |l: &Layout, col: usize, row: usize| {
            l.rects(area)
                .into_iter()
                .find(|(_, r)| r.contains(col, row))
                .map(|(id, _)| id)
        };
        assert_eq!(at(&l, 0, 0), Some(1));
        assert_eq!(at(&l, 41, 0), Some(2));
        assert_eq!(at(&l, 40, 0), None); // the divider column
        assert_eq!(l.cycle(1, true), 2);
        assert_eq!(l.cycle(2, true), 1); // wraps
        assert_eq!(l.cycle(1, false), 2); // wraps backward
    }

    #[test]
    fn resize_adjusts_nearest_matching_split() {
        // [1 | 2] with 2 split into 2/3 vertically again: resizing 1 moves
        // the outer boundary; resizing 3 moves the inner one.
        let mut l = Layout::single(1);
        l.split(1, 2, Dir::Vertical);
        l.split(2, 3, Dir::Vertical);
        let area = Rect::new(0, 0, 41, 20);
        let before = l.rects(area);
        assert!(l.resize(1, Dir::Vertical, 0.2)); // grow pane 1 rightward
        let after = l.rects(area);
        let w = |rs: &Vec<(u64, Rect)>, id: u64| rs.iter().find(|(i, _)| *i == id).unwrap().1.cols;
        assert!(w(&after, 1) > w(&before, 1));
        // Growing pane 3 (a `b` child) lowers its split's ratio -> wider 3.
        let before = after;
        assert!(l.resize(3, Dir::Vertical, 0.2));
        let after = l.rects(area);
        assert!(w(&after, 3) > w(&before, 3));
        // No split of the other axis exists: resize reports false.
        assert!(!l.resize(1, Dir::Horizontal, 0.1));
    }

    #[test]
    fn resize_clamps_ratio() {
        let mut l = Layout::single(1);
        l.split(1, 2, Dir::Horizontal);
        for _ in 0..20 {
            l.resize(1, Dir::Horizontal, 0.2);
        }
        let area = Rect::new(0, 0, 20, 21);
        let rs = l.rects(area);
        // Both panes keep at least one row even after saturating growth.
        assert!(rs.iter().all(|(_, r)| r.rows >= 1));
    }
}
