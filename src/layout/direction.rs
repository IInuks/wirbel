//! Finding the next window in a direction, from nothing but rectangles.

use super::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    /// Rewrites `rect` as if `self` pointed along the positive x axis.
    fn normalise(self, rect: Rect) -> Rect {
        let Rect { x, y, w, h } = rect;
        match self {
            Direction::Right => Rect::new(x, y, w, h),
            Direction::Left => Rect::new(-x - w, y, w, h),
            Direction::Down => Rect::new(y, x, h, w),
            Direction::Up => Rect::new(-y - h, x, h, w),
        }
    }
}

/// The candidate that lies in `direction` of `from`. A candidate qualifies if it starts past the
/// far edge of `from`, or if it merely overlaps and its centre lies further along, which is how
/// floating windows stay reachable. Candidates side by side with `from` win over diagonal ones,
/// then the nearest, then the best aligned. Ties go to the first candidate.
pub fn find_in_direction<T>(
    from: Rect,
    candidates: impl IntoIterator<Item = (T, Rect)>,
    direction: Direction,
) -> Option<T> {
    let from = direction.normalise(from);
    candidates
        .into_iter()
        .filter_map(|(item, rect)| {
            let rect = direction.normalise(rect);
            let beside = rect.y < from.y + from.h && from.y < rect.y + rect.h;
            let past = rect.x >= from.x + from.w;
            let ahead = rect.center().0 > from.center().0;
            (past || (beside && ahead)).then(|| {
                let distance = (rect.x - (from.x + from.w)).max(0);
                let offset = (rect.center().1 - from.center().1).abs();
                ((!beside, distance, offset), item)
            })
        })
        .min_by_key(|(score, _)| *score)
        .map(|(_, item)| item)
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect::new(x, y, w, h)
    }

    /// 1 fills the left half, 2 the top right quarter, 3 and 4 share the bottom right quarter.
    const TILES: [(u32, Rect); 4] = [
        (1, r(0, 0, 400, 800)),
        (2, r(400, 0, 400, 400)),
        (3, r(400, 400, 200, 400)),
        (4, r(600, 400, 200, 400)),
    ];

    fn go(from: u32, direction: Direction) -> Option<u32> {
        let rect = TILES[from as usize - 1].1;
        find_in_direction(
            rect,
            TILES.into_iter().filter(|(item, _)| *item != from),
            direction,
        )
    }

    #[test]
    fn walks_between_tiles() {
        use Direction::*;
        assert_eq!(go(1, Right), Some(2));
        assert_eq!(go(2, Left), Some(1));
        assert_eq!(go(2, Down), Some(3));
        assert_eq!(go(3, Up), Some(2));
        assert_eq!(go(3, Right), Some(4));
        assert_eq!(go(4, Left), Some(3));
        assert_eq!(go(3, Left), Some(1));
        assert_eq!(go(4, Up), Some(2));
    }

    #[test]
    fn stops_at_the_edge() {
        use Direction::*;
        assert_eq!(go(1, Left), None);
        assert_eq!(go(1, Up), None);
        assert_eq!(go(1, Down), None);
        assert_eq!(go(2, Right), None);
        assert_eq!(go(4, Down), None);
    }

    #[test]
    fn a_wide_tile_does_not_reach_a_tile_below_its_right_end() {
        let wide = r(0, 0, 800, 400);
        let below = r(600, 400, 200, 400);
        assert_eq!(
            find_in_direction(wide, [(1, below)], Direction::Right),
            None
        );
    }

    #[test]
    fn side_by_side_beats_a_nearer_diagonal() {
        let from = r(0, 0, 100, 100);
        let diagonal = (1, r(110, 200, 100, 100));
        let beside = (2, r(500, 0, 100, 100));
        assert_eq!(
            find_in_direction(from, [diagonal, beside], Direction::Right),
            Some(2)
        );
        assert_eq!(
            find_in_direction(from, [diagonal], Direction::Right),
            Some(1)
        );
    }

    #[test]
    fn overlapping_windows_are_reachable_by_their_centre() {
        let tile = r(0, 0, 800, 800);
        let floating = r(500, 300, 200, 200);
        assert_eq!(
            find_in_direction(tile, [(1, floating)], Direction::Right),
            Some(1)
        );
        assert_eq!(
            find_in_direction(floating, [(1, tile)], Direction::Left),
            Some(1)
        );
        assert_eq!(
            find_in_direction(tile, [(1, floating)], Direction::Left),
            None
        );
    }

    #[test]
    fn negative_coordinates_work() {
        let from = r(-500, -300, 100, 100);
        let target = r(-500, -100, 100, 100);
        assert_eq!(
            find_in_direction(from, [(1, target)], Direction::Down),
            Some(1)
        );
        assert_eq!(
            find_in_direction(target, [(1, from)], Direction::Up),
            Some(1)
        );
    }
}
