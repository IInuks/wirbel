//! Binary dwindle tree.
//!
//! Leaves hold windows, inner nodes split their area in two. The split axis is not stored: it
//! follows from the shape of the area (wide splits side by side, tall splits stacked), so the
//! layout stays sensible when the output changes size.

/// Rectangle in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn center(self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }

    pub fn contains(self, x: f64, y: f64) -> bool {
        x >= f64::from(self.x)
            && y >= f64::from(self.y)
            && x < f64::from(self.x + self.w)
            && y < f64::from(self.y + self.h)
    }

    pub fn expand(self, by: i32) -> Self {
        Self::new(self.x - by, self.y - by, self.w + 2 * by, self.h + 2 * by)
    }

    /// Moves the rectangle into `area`. If it does not fit, its top left corner stays visible.
    pub fn clamped_into(self, area: Self) -> Self {
        let x = self.x.min(area.x + area.w - self.w).max(area.x);
        let y = self.y.min(area.y + area.h - self.h).max(area.y);
        Self::new(x, y, self.w, self.h)
    }

    /// Shrinks the rectangle on all sides. Width and height never drop below 1, because a
    /// configure with size 0 would tell the client to pick its own size.
    pub fn shrink(self, by: i32) -> Self {
        let w = (self.w - 2 * by).max(1);
        let h = (self.h - 2 * by).max(1);
        Self::new(self.x + (self.w - w) / 2, self.y + (self.h - h) / 2, w, h)
    }

    /// Splits along the longer side, leaving `gap` free between the two halves.
    fn split(self, ratio: f32, gap: i32) -> (Self, Self) {
        let side_by_side = self.w >= self.h;
        let length = if side_by_side { self.w } else { self.h };
        let gap = gap.min(length - 2).max(0);
        let usable = length - gap;
        if usable < 2 {
            // Nothing left to divide. Overlapping is the only way to keep both sizes valid.
            return (self, self);
        }
        let first = ((usable as f32 * ratio).round() as i32).clamp(1, usable - 1);
        let second = usable - first;
        if side_by_side {
            (
                Self::new(self.x, self.y, first, self.h),
                Self::new(self.x + first + gap, self.y, second, self.h),
            )
        } else {
            (
                Self::new(self.x, self.y, self.w, first),
                Self::new(self.x, self.y + first + gap, self.w, second),
            )
        }
    }
}

/// Limits of a split ratio, so that no side can vanish.
const MIN_RATIO: f32 = 0.1;
const MAX_RATIO: f32 = 0.9;

/// Free space around the tiled area (`outer`) and between windows (`inner`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gaps {
    pub outer: i32,
    pub inner: i32,
}

#[derive(Debug)]
enum Node<T> {
    Leaf(T),
    Split {
        ratio: f32,
        first: Box<Node<T>>,
        second: Box<Node<T>>,
    },
}

impl<T> Node<T> {
    fn first_leaf(&self) -> &T {
        match self {
            Node::Leaf(item) => item,
            Node::Split { first, .. } => first.first_leaf(),
        }
    }

    fn last_leaf(&self) -> &T {
        match self {
            Node::Leaf(item) => item,
            Node::Split { second, .. } => second.last_leaf(),
        }
    }

    fn push_leaves<'a>(&'a self, out: &mut Vec<&'a T>) {
        match self {
            Node::Leaf(item) => out.push(item),
            Node::Split { first, second, .. } => {
                first.push_leaves(out);
                second.push_leaves(out);
            }
        }
    }

    fn push_leaves_mut<'a>(&'a mut self, out: &mut Vec<&'a mut T>) {
        match self {
            Node::Leaf(item) => out.push(item),
            Node::Split { first, second, .. } => {
                first.push_leaves_mut(out);
                second.push_leaves_mut(out);
            }
        }
    }

    fn arrange<'a>(&'a self, area: Rect, gap: i32, out: &mut Vec<(&'a T, Rect)>) {
        match self {
            Node::Leaf(item) => out.push((item, area)),
            Node::Split {
                ratio,
                first,
                second,
            } => {
                let (a, b) = area.split(*ratio, gap);
                first.arrange(a, gap, out);
                second.arrange(b, gap, out);
            }
        }
    }
}

impl<T> Node<T> {
    /// Replaces the leaf at position `index` with a split of that leaf and `item`. `index`
    /// counts down to 0 over the leaves, and `item` stays in place until it reaches the target.
    fn insert_at(self, index: &mut usize, item: &mut Option<T>) -> Self {
        match self {
            Node::Leaf(old) => {
                let hit = *index == 0;
                *index = index.wrapping_sub(1);
                match item.take_if(|_| hit) {
                    Some(item) => Node::Split {
                        ratio: 0.5,
                        first: Box::new(Node::Leaf(old)),
                        second: Box::new(Node::Leaf(item)),
                    },
                    None => Node::Leaf(old),
                }
            }
            Node::Split {
                ratio,
                first,
                second,
            } => {
                let first = Box::new(first.insert_at(index, item));
                let second = Box::new(second.insert_at(index, item));
                Node::Split {
                    ratio,
                    first,
                    second,
                }
            }
        }
    }
}

impl<T: PartialEq> Node<T> {
    /// `Ok` holds what remains of this subtree after removing `item`, `Err` the untouched
    /// subtree if `item` is not in it.
    fn remove(self, item: &T) -> Result<Option<Self>, Self> {
        match self {
            Node::Leaf(old) if old == *item => Ok(None),
            leaf @ Node::Leaf(_) => Err(leaf),
            Node::Split {
                ratio,
                first,
                second,
            } => match first.remove(item) {
                Ok(None) => Ok(Some(*second)),
                Ok(Some(first)) => Ok(Some(Node::Split {
                    ratio,
                    first: Box::new(first),
                    second,
                })),
                Err(first) => match second.remove(item) {
                    Ok(None) => Ok(Some(first)),
                    Ok(Some(second)) => Ok(Some(Node::Split {
                        ratio,
                        first: Box::new(first),
                        second: Box::new(second),
                    })),
                    Err(second) => Err(Node::Split {
                        ratio,
                        first: Box::new(first),
                        second: Box::new(second),
                    }),
                },
            },
        }
    }

    fn contains(&self, item: &T) -> bool {
        match self {
            Node::Leaf(leaf) => leaf == item,
            Node::Split { first, second, .. } => first.contains(item) || second.contains(item),
        }
    }

    /// Grows `item` by `delta` of the split that created its leaf. Returns whether it was found.
    fn grow(&mut self, item: &T, delta: f32) -> bool {
        let Node::Split {
            ratio,
            first,
            second,
        } = self
        else {
            return false;
        };
        let change = match (&**first, &**second) {
            (Node::Leaf(leaf), _) if leaf == item => delta,
            (_, Node::Leaf(leaf)) if leaf == item => -delta,
            _ => return first.grow(item, delta) || second.grow(item, delta),
        };
        *ratio = (*ratio + change).clamp(MIN_RATIO, MAX_RATIO);
        true
    }

    /// Moves the dividers nearest to `item` by `dx` and `dy` pixels: on each axis the deepest
    /// split on the way to its leaf. Returns which axes are already taken care of.
    fn drag(&mut self, item: &T, area: Rect, gap: i32, dx: f64, dy: f64) -> (bool, bool) {
        let Node::Split {
            ratio,
            first,
            second,
        } = self
        else {
            return (false, false);
        };
        let (a, b) = area.split(*ratio, gap);
        let (mut horizontal, mut vertical) = if first.contains(item) {
            first.drag(item, a, gap, dx, dy)
        } else if second.contains(item) {
            second.drag(item, b, gap, dx, dy)
        } else {
            return (false, false);
        };

        let side_by_side = area.w >= area.h;
        let (done, length, delta) = if side_by_side {
            (&mut horizontal, area.w, dx)
        } else {
            (&mut vertical, area.h, dy)
        };
        if !*done && length > gap {
            let change = delta as f32 / (length - gap) as f32;
            *ratio = (*ratio + change).clamp(MIN_RATIO, MAX_RATIO);
            *done = true;
        }
        (horizontal, vertical)
    }

    fn neighbour(&self, item: &T) -> Option<&T> {
        let Node::Split { first, second, .. } = self else {
            return None;
        };
        match (&**first, &**second) {
            (Node::Leaf(leaf), other) if leaf == item => Some(other.first_leaf()),
            (other, Node::Leaf(leaf)) if leaf == item => Some(other.last_leaf()),
            _ => first.neighbour(item).or_else(|| second.neighbour(item)),
        }
    }
}

/// A tree of tiled items.
#[derive(Debug)]
pub struct Dwindle<T> {
    root: Option<Node<T>>,
}

impl<T> Default for Dwindle<T> {
    fn default() -> Self {
        Self { root: None }
    }
}

impl<T: PartialEq> Dwindle<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// All items in tree order.
    pub fn items(&self) -> Vec<&T> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.push_leaves(&mut out);
        }
        out
    }

    pub fn contains(&self, item: &T) -> bool {
        self.items().contains(&item)
    }

    /// Adds `item` by splitting the leaf of `target`. Without a target, or if the target is not
    /// in the tree, the most recently dwindled leaf is split.
    pub fn insert(&mut self, target: Option<&T>, item: T) {
        let leaves = self.items();
        let index = target
            .and_then(|target| leaves.iter().position(|leaf| *leaf == target))
            .unwrap_or(leaves.len().saturating_sub(1));
        self.root = Some(match self.root.take() {
            None => Node::Leaf(item),
            Some(root) => root.insert_at(&mut { index }, &mut Some(item)),
        });
    }

    /// Removes `item`, its sibling takes over the freed space. Returns whether it was present.
    pub fn remove(&mut self, item: &T) -> bool {
        let Some(root) = self.root.take() else {
            return false;
        };
        match root.remove(item) {
            Ok(rest) => {
                self.root = rest;
                true
            }
            Err(root) => {
                self.root = Some(root);
                false
            }
        }
    }

    /// Exchanges the leaves of `a` and `b`. Returns whether both were present.
    pub fn swap(&mut self, a: &T, b: &T) -> bool {
        let Some(root) = &mut self.root else {
            return false;
        };
        let mut leaves = Vec::new();
        root.push_leaves_mut(&mut leaves);
        let first = leaves.iter().position(|leaf| **leaf == *a);
        let second = leaves.iter().position(|leaf| **leaf == *b);
        let (Some(first), Some(second)) = (first, second) else {
            return false;
        };
        let (low, high) = (first.min(second), first.max(second));
        if low != high {
            let (head, tail) = leaves.split_at_mut(high);
            std::mem::swap(&mut *head[low], &mut *tail[0]);
        }
        true
    }

    /// Grows `item` by `delta` of the area it shares with its sibling, or shrinks it if `delta`
    /// is negative. Returns whether a split changed.
    pub fn grow(&mut self, item: &T, delta: f32) -> bool {
        self.root
            .as_mut()
            .is_some_and(|root| root.grow(item, delta))
    }

    /// Moves the dividers next to `item` by the given number of pixels, as a mouse drag does.
    pub fn drag(&mut self, item: &T, area: Rect, gaps: Gaps, dx: f64, dy: f64) {
        if let Some(root) = &mut self.root {
            root.drag(item, area.shrink(gaps.outer), gaps.inner, dx, dy);
        }
    }

    /// The item closest to `item` in the tree, which is the one that inherits its space when
    /// `item` is removed.
    pub fn neighbour(&self, item: &T) -> Option<&T> {
        self.root.as_ref()?.neighbour(item)
    }

    /// Computes the rectangle of every item. Rectangles are pixel exact: nothing is lost to
    /// rounding, and no two overlap as long as the area is large enough to hold them.
    pub fn arrange(&self, area: Rect, gaps: Gaps) -> Vec<(&T, Rect)> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.arrange(area.shrink(gaps.outer), gaps.inner, &mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect::new(0, 0, 1000, 600);
    const NO_GAPS: Gaps = Gaps { outer: 0, inner: 0 };

    fn rects(tree: &Dwindle<u32>, area: Rect, gaps: Gaps) -> Vec<(u32, Rect)> {
        tree.arrange(area, gaps)
            .into_iter()
            .map(|(item, rect)| (*item, rect))
            .collect()
    }

    fn tree_of(items: &[u32]) -> Dwindle<u32> {
        let mut tree = Dwindle::new();
        for item in items {
            tree.insert(None, *item);
        }
        tree
    }

    fn overlaps(a: Rect, b: Rect) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    #[test]
    fn empty_tree_has_no_tiles() {
        let tree = Dwindle::<u32>::new();
        assert!(tree.is_empty());
        assert!(tree.arrange(AREA, NO_GAPS).is_empty());
        assert_eq!(tree.neighbour(&1), None);
    }

    #[test]
    fn single_window_fills_the_area_inside_the_outer_gap() {
        let tree = tree_of(&[1]);
        let gaps = Gaps {
            outer: 10,
            inner: 5,
        };
        assert_eq!(rects(&tree, AREA, gaps), [(1, Rect::new(10, 10, 980, 580))]);
    }

    #[test]
    fn wide_area_splits_side_by_side() {
        let tree = tree_of(&[1, 2]);
        assert_eq!(
            rects(&tree, AREA, NO_GAPS),
            [
                (1, Rect::new(0, 0, 500, 600)),
                (2, Rect::new(500, 0, 500, 600))
            ]
        );
    }

    #[test]
    fn tall_area_splits_stacked() {
        let tree = tree_of(&[1, 2]);
        assert_eq!(
            rects(&tree, Rect::new(0, 0, 600, 1000), NO_GAPS),
            [
                (1, Rect::new(0, 0, 600, 500)),
                (2, Rect::new(0, 500, 600, 500))
            ]
        );
    }

    #[test]
    fn square_area_splits_side_by_side() {
        let tree = tree_of(&[1, 2]);
        let tiles = rects(&tree, Rect::new(0, 0, 400, 400), NO_GAPS);
        assert_eq!(tiles[1].1, Rect::new(200, 0, 200, 400));
    }

    #[test]
    fn windows_dwindle_into_the_last_leaf() {
        let tree = tree_of(&[1, 2, 3, 4]);
        assert_eq!(
            rects(&tree, Rect::new(0, 0, 800, 800), NO_GAPS),
            [
                (1, Rect::new(0, 0, 400, 800)),
                (2, Rect::new(400, 0, 400, 400)),
                (3, Rect::new(400, 400, 200, 400)),
                (4, Rect::new(600, 400, 200, 400)),
            ]
        );
    }

    #[test]
    fn insert_splits_the_target_only() {
        let mut tree = tree_of(&[1, 2]);
        tree.insert(Some(&1), 3);
        assert_eq!(
            rects(&tree, AREA, NO_GAPS),
            [
                (1, Rect::new(0, 0, 500, 300)),
                (3, Rect::new(0, 300, 500, 300)),
                (2, Rect::new(500, 0, 500, 600)),
            ]
        );
    }

    #[test]
    fn insert_with_unknown_target_uses_the_last_leaf() {
        let mut tree = tree_of(&[1, 2]);
        tree.insert(Some(&99), 3);
        assert_eq!(tree.items(), [&1, &2, &3]);
        assert_eq!(tree.neighbour(&3), Some(&2));
    }

    #[test]
    fn inner_gap_separates_and_nothing_is_lost() {
        let tree = tree_of(&[1, 2]);
        let gaps = Gaps {
            outer: 10,
            inner: 6,
        };
        assert_eq!(
            rects(&tree, AREA, gaps),
            [
                (1, Rect::new(10, 10, 487, 580)),
                (2, Rect::new(503, 10, 487, 580))
            ]
        );
    }

    #[test]
    fn odd_sizes_lose_no_pixel() {
        let tree = tree_of(&[1, 2]);
        let tiles = rects(&tree, Rect::new(0, 0, 1001, 600), NO_GAPS);
        assert_eq!(tiles[0].1.w + tiles[1].1.w, 1001);
        assert_eq!(tiles[1].1.x, tiles[0].1.w);
    }

    #[test]
    fn removing_gives_the_space_to_the_sibling() {
        let mut tree = tree_of(&[1, 2, 3]);
        assert!(tree.remove(&2));
        assert!(!tree.contains(&2));
        assert_eq!(
            rects(&tree, AREA, NO_GAPS),
            [
                (1, Rect::new(0, 0, 500, 600)),
                (3, Rect::new(500, 0, 500, 600))
            ]
        );
    }

    #[test]
    fn removing_the_first_child_promotes_a_whole_subtree() {
        let mut tree = tree_of(&[1, 2, 3]);
        assert!(tree.remove(&1));
        assert_eq!(
            rects(&tree, AREA, NO_GAPS),
            [
                (2, Rect::new(0, 0, 500, 600)),
                (3, Rect::new(500, 0, 500, 600))
            ]
        );
    }

    #[test]
    fn removing_the_last_window_empties_the_tree() {
        let mut tree = tree_of(&[1]);
        assert!(tree.remove(&1));
        assert!(tree.is_empty());
        assert!(!tree.remove(&1));
    }

    #[test]
    fn removing_an_unknown_item_changes_nothing() {
        let mut tree = tree_of(&[1, 2, 3]);
        assert!(!tree.remove(&9));
        assert_eq!(tree.items(), [&1, &2, &3]);
    }

    #[test]
    fn neighbour_is_the_item_next_to_it_in_the_tree() {
        let tree = tree_of(&[1, 2, 3]);
        assert_eq!(tree.neighbour(&1), Some(&2));
        assert_eq!(tree.neighbour(&2), Some(&3));
        assert_eq!(tree.neighbour(&3), Some(&2));
        assert_eq!(tree.neighbour(&9), None);
        assert_eq!(tree_of(&[1]).neighbour(&1), None);
    }

    #[test]
    fn neighbour_of_a_second_child_is_the_nearest_leaf_of_the_first() {
        let mut tree = tree_of(&[1, 2]);
        tree.insert(Some(&1), 3);
        tree.insert(Some(&3), 4);
        tree.remove(&2);
        tree.insert(Some(&1), 5);
        assert_eq!(tree.items(), [&1, &5, &3, &4]);
        assert_eq!(tree.neighbour(&1), Some(&5));
        assert_eq!(tree.neighbour(&4), Some(&3));
    }

    #[test]
    fn tiny_areas_keep_every_size_positive() {
        for (w, h) in [(0, 0), (1, 1), (2, 1), (3, 3), (5, 40)] {
            let tree = tree_of(&[1, 2, 3, 4, 5, 6]);
            for (_, rect) in rects(
                &tree,
                Rect::new(0, 0, w, h),
                Gaps {
                    outer: 10,
                    inner: 5,
                },
            ) {
                assert!(rect.w >= 1 && rect.h >= 1, "{rect:?} in {w}x{h}");
            }
        }
    }

    #[test]
    fn shrink_stays_centred_and_positive() {
        assert_eq!(
            Rect::new(10, 20, 100, 50).shrink(4),
            Rect::new(14, 24, 92, 42)
        );
        let tiny = Rect::new(10, 20, 3, 3).shrink(4);
        assert_eq!((tiny.w, tiny.h), (1, 1));
        assert_eq!((tiny.x, tiny.y), (11, 21));
    }

    #[test]
    fn swap_exchanges_places_and_keeps_the_shape() {
        let mut tree = tree_of(&[1, 2, 3]);
        tree.grow(&1, 0.2);
        let before: Vec<Rect> = rects(&tree, AREA, NO_GAPS).iter().map(|t| t.1).collect();
        assert!(tree.swap(&1, &3));
        assert_eq!(tree.items(), [&3, &2, &1]);
        let after: Vec<Rect> = rects(&tree, AREA, NO_GAPS).iter().map(|t| t.1).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn swap_ignores_unknown_items_and_itself() {
        let mut tree = tree_of(&[1, 2]);
        assert!(!tree.swap(&1, &9));
        assert!(tree.swap(&1, &1));
        assert_eq!(tree.items(), [&1, &2]);
        assert!(!Dwindle::<u32>::new().swap(&1, &2));
    }

    #[test]
    fn grow_favours_the_item_on_either_side_of_the_split() {
        let mut tree = tree_of(&[1, 2]);
        assert!(tree.grow(&1, 0.1));
        assert_eq!(rects(&tree, AREA, NO_GAPS)[0].1.w, 600);
        assert!(tree.grow(&2, 0.2));
        assert_eq!(rects(&tree, AREA, NO_GAPS)[0].1.w, 400);
    }

    #[test]
    fn grow_only_touches_the_parent_split() {
        let mut tree = tree_of(&[1, 2, 3]);
        assert!(tree.grow(&3, 0.1));
        let tiles = rects(&tree, AREA, NO_GAPS);
        assert_eq!(tiles[0].1.w, 500, "root split moved");
        assert_eq!(tiles[1].1.h, 240);
        assert_eq!(tiles[2].1.h, 360);
    }

    #[test]
    fn ratios_stop_at_the_limits() {
        let mut tree = tree_of(&[1, 2]);
        tree.grow(&1, 5.0);
        assert_eq!(rects(&tree, AREA, NO_GAPS)[0].1.w, 900);
        tree.grow(&1, -5.0);
        assert_eq!(rects(&tree, AREA, NO_GAPS)[0].1.w, 100);
        assert!(!tree.grow(&9, 0.1));
        assert!(!tree_of(&[1]).grow(&1, 0.1));
    }

    #[test]
    fn dragging_moves_the_divider_with_the_pointer() {
        let mut tree = tree_of(&[1, 2]);
        tree.drag(&1, AREA, NO_GAPS, 100.0, 0.0);
        assert_eq!(rects(&tree, AREA, NO_GAPS)[0].1.w, 600);
        // The divider follows the pointer no matter on which side the dragged window lies.
        tree.drag(&2, AREA, NO_GAPS, -200.0, 0.0);
        assert_eq!(rects(&tree, AREA, NO_GAPS)[0].1.w, 400);
    }

    #[test]
    fn dragging_acts_on_both_axes_independently() {
        let mut tree = tree_of(&[1, 2, 3]);
        tree.drag(&2, AREA, NO_GAPS, 100.0, 60.0);
        let tiles = rects(&tree, AREA, NO_GAPS);
        assert_eq!(tiles[0].1.w, 600);
        assert_eq!(tiles[1].1.h, 360);
    }

    #[test]
    fn dragging_prefers_the_deepest_split_of_an_axis() {
        // 1 | (2 | (3 over 4)): two side-by-side splits on the way to 3.
        let mut tree = tree_of(&[1, 2, 3, 4]);
        let area = Rect::new(0, 0, 1600, 800);
        let before = rects(&tree, area, NO_GAPS);
        tree.drag(&3, area, NO_GAPS, 80.0, 0.0);
        let after = rects(&tree, area, NO_GAPS);
        assert_eq!(after[0].1, before[0].1, "root split moved");
        assert_eq!(after[1].1.w, before[1].1.w + 80);
        assert_eq!(after[2].1.w, before[2].1.w - 80);
    }

    #[test]
    fn clamped_into_keeps_the_size_and_prefers_the_top_left_corner() {
        let area = Rect::new(100, 100, 500, 400);
        assert_eq!(
            Rect::new(550, 450, 100, 100).clamped_into(area),
            Rect::new(500, 400, 100, 100)
        );
        assert_eq!(
            Rect::new(0, 0, 100, 100).clamped_into(area),
            Rect::new(100, 100, 100, 100)
        );
        assert_eq!(
            Rect::new(0, 0, 900, 900).clamped_into(area),
            Rect::new(100, 100, 900, 900)
        );
    }

    /// Random inserts and removals, checked against the layout invariants.
    #[test]
    fn random_operations_keep_the_layout_valid() {
        let mut seed = 0x2545_f491_u32;
        let mut next = move |limit: u32| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed % limit
        };
        let area = Rect::new(7, 13, 3840, 2160);
        let gaps = Gaps {
            outer: 10,
            inner: 5,
        };
        let mut tree = Dwindle::new();
        let mut members: Vec<u32> = Vec::new();
        let mut id = 0;

        for _ in 0..5000 {
            if members.is_empty() || (members.len() < 24 && next(3) != 0) {
                let target =
                    (!members.is_empty()).then(|| members[next(members.len() as u32) as usize]);
                id += 1;
                tree.insert(target.as_ref(), id);
                members.push(id);
            } else {
                let index = next(members.len() as u32) as usize;
                let item = members.swap_remove(index);
                let neighbour = tree.neighbour(&item).copied();
                assert_eq!(neighbour.is_some(), !members.is_empty());
                assert!(tree.remove(&item));
                assert!(neighbour.is_none_or(|n| tree.contains(&n)));
            }
            if members.len() >= 2 {
                let a = members[next(members.len() as u32) as usize];
                let b = members[next(members.len() as u32) as usize];
                tree.swap(&a, &b);
                tree.grow(&a, next(40) as f32 / 100.0 - 0.2);
                tree.drag(&b, area, gaps, f64::from(next(200)) - 100.0, 0.0);
            }

            let tiles = rects(&tree, area, gaps);
            assert_eq!(tiles.len(), members.len());
            let mut covered: i64 = 0;
            for (i, (item, rect)) in tiles.iter().enumerate() {
                assert!(members.contains(item));
                assert!(rect.w >= 1 && rect.h >= 1);
                assert!(rect.x >= area.x + gaps.outer);
                assert!(rect.y >= area.y + gaps.outer);
                assert!(rect.x + rect.w <= area.x + area.w - gaps.outer);
                assert!(rect.y + rect.h <= area.y + area.h - gaps.outer);
                for (_, other) in &tiles[i + 1..] {
                    assert!(!overlaps(*rect, *other), "{rect:?} overlaps {other:?}");
                }
                covered += i64::from(rect.w) * i64::from(rect.h);
            }
            let usable = i64::from(area.w - 20) * i64::from(area.h - 20);
            assert!(covered <= usable);
        }
    }
}
