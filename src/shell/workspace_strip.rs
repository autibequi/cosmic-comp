// SPDX-License-Identifier: GPL-3.0-only

//! Geometry for the workspace strip in `WorkspaceLayout::Scrolling` mode.
//!
//! In scrolling mode the workspaces of an output form a single horizontal
//! strip: workspace `N` occupies the column at `x = N * column_width`. The
//! viewport shows exactly one column at a time; activating a workspace pans
//! the viewport to that column.
//!
//! This module owns the pure positioning policy. It deliberately knows
//! nothing about shells, outputs or rendering so the geometry can be unit
//! tested in isolation. Today every column has the width of the output; the
//! `ColumnGeometry` abstraction keeps the door open for per-column widths.

use smithay::utils::{Logical, Point, Size};

/// Position and width of one workspace column on the horizontal strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnGeometry {
    /// Horizontal offset of the column's left edge relative to the strip origin.
    pub x: i32,
    /// Width of the column. Equals the output width in the current MVP.
    pub width: i32,
}

impl ColumnGeometry {
    /// Geometry of column `index` on a strip of uniformly sized columns.
    pub fn new(index: usize, width: i32) -> Self {
        Self {
            x: index as i32 * width,
            width,
        }
    }

    /// Geometry of the column currently under `viewport_x`.
    ///
    /// `viewport_x` is clamped to the strip, so it always resolves to a valid
    /// column for any input (e.g. a mid-gesture offset beyond the edges).
    pub fn containing(viewport_x: i32, width: i32) -> Self {
        let clamped = viewport_x.max(0);
        Self::new((clamped / width.max(1)) as usize, width)
    }
}

/// X offset of the viewport origin that makes column `index` fully visible.
///
/// This is the instant (non-animated) positioning used when a workspace is
/// activated via shortcut: `offset = index * width`.
pub fn viewport_offset_for_index(index: usize, width: i32) -> i32 {
    ColumnGeometry::new(index, width).x
}

/// Position of the workspace column relative to the viewport origin, given
/// the viewport x offset. Column 0 with viewport at its own offset yields
/// `(0, 0)`.
pub fn column_position(index: usize, viewport_x: i32, width: i32) -> Point<i32, Logical> {
    Point::from((ColumnGeometry::new(index, width).x - viewport_x, 0))
}

/// Whether index-based navigation past the bounds should wrap around.
/// Returns the target index for a step of `delta` from `current`, or `None`
/// when the step leaves the strip and `wraparound` is disabled.
pub fn stepped_index(current: usize, delta: i32, count: usize, wraparound: bool) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let current = current as i32;
    let count = count as i32;
    let next = current + delta;
    if (0..count).contains(&next) {
        Some(next as usize)
    } else if wraparound {
        Some(next.rem_euclid(count) as usize)
    } else {
        None
    }
}

/// Default viewport position for an output: centered on column 0.
pub fn default_viewport(output_size: Size<i32, Logical>) -> Point<i32, Logical> {
    Point::from((viewport_offset_for_index(0, output_size.w), 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: i32 = 1920;

    #[test]
    fn column_x_is_index_times_width() {
        assert_eq!(ColumnGeometry::new(0, W), ColumnGeometry { x: 0, width: W });
        assert_eq!(ColumnGeometry::new(1, W).x, W);
        assert_eq!(ColumnGeometry::new(7, W).x, 7 * W);
    }

    #[test]
    fn viewport_offset_matches_column_x() {
        for idx in 0..10 {
            assert_eq!(viewport_offset_for_index(idx, W), idx as i32 * W);
            assert_eq!(viewport_offset_for_index(idx, 800), idx as i32 * 800);
        }
    }

    #[test]
    fn column_position_relates_strip_and_viewport() {
        // Viewport on column 2: column 2 is at origin, column 3 one width ahead.
        let vp = viewport_offset_for_index(2, W);
        assert_eq!(column_position(2, vp, W), Point::from((0, 0)));
        assert_eq!(column_position(3, vp, W), Point::from((W, 0)));
        assert_eq!(column_position(1, vp, W), Point::from((-W, 0)));
    }

    #[test]
    fn containing_resolves_clamped_columns() {
        // Inside the strip: index follows the offset.
        assert_eq!(ColumnGeometry::containing(0, W).x, 0);
        assert_eq!(ColumnGeometry::containing(2 * W, W).x, 2 * W);
        assert_eq!(ColumnGeometry::containing(2 * W + W / 2, W).x, 2 * W);
        // Negative offsets clamp to column 0.
        assert_eq!(ColumnGeometry::containing(-500, W).x, 0);
    }

    #[test]
    fn stepped_index_without_wraparound_stops_at_edges() {
        assert_eq!(stepped_index(0, -1, 5, false), None);
        assert_eq!(stepped_index(4, 1, 5, false), None);
        assert_eq!(stepped_index(2, 1, 5, false), Some(3));
        assert_eq!(stepped_index(2, -1, 5, false), Some(1));
    }

    #[test]
    fn stepped_index_with_wraparound_cycles() {
        assert_eq!(stepped_index(0, -1, 5, true), Some(4));
        assert_eq!(stepped_index(4, 1, 5, true), Some(0));
        assert_eq!(stepped_index(0, -2, 5, true), Some(3));
        assert_eq!(stepped_index(0, 5, 5, true), Some(0));
    }

    #[test]
    fn stepped_index_on_empty_strip_is_none() {
        assert_eq!(stepped_index(0, 1, 0, true), None);
    }

    #[test]
    fn default_viewport_is_first_column() {
        assert_eq!(default_viewport(Size::from((W, 1080))), Point::from((0, 0)));
    }
}
