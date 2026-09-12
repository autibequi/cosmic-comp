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

use std::time::{Duration, Instant};

use keyframe::{ease, functions::EaseInOutCubic};
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

/// Continuous viewport offset of the strip for one output.
///
/// This is purely spatial state: `active_space` (the workspace index) remains
/// the single source of truth for which workspace is active; the offset only
/// describes where the viewport currently sits on the strip, possibly between
/// two columns. With `offset == target_offset(active)` the layout is identical
/// to the discrete per-index positioning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StripState {
    /// Current viewport origin x on the strip; `0.0` aligns column 0.
    pub offset: f64,
}

impl Default for StripState {
    fn default() -> Self {
        Self { offset: 0.0 }
    }
}

impl StripState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inclusive upper bound of the offset for a strip of `count` columns.
    pub fn max_offset(count: usize, width: i32) -> f64 {
        count.saturating_sub(1) as f64 * width.max(0) as f64
    }

    /// Offset clamped to the valid strip range `[0, (count - 1) * width]`.
    ///
    /// Wraparound navigation steps through indices; the pan itself always
    /// happens between adjacent columns, never across the wrap, so the clamp
    /// range is enough to keep the viewport on the strip.
    pub fn clamp_offset(offset: f64, count: usize, width: i32) -> f64 {
        offset.clamp(0.0, Self::max_offset(count, width))
    }

    /// Offset that makes column `index` fully visible (the snap target).
    pub fn target_offset(index: usize, width: i32) -> f64 {
        index as f64 * width.max(0) as f64
    }

    /// Snap the offset so column `index` is fully visible.
    pub fn snap_to(&mut self, index: usize, count: usize, width: i32) {
        self.offset = Self::clamp_offset(Self::target_offset(index, width), count, width);
    }

    /// X of column `index` relative to the viewport origin for an arbitrary
    /// (possibly intermediate) offset. Neighbouring columns may be partially
    /// visible at the viewport edges; clipping to the output is the renderer's
    /// job, the geometry just has to expose the partial positions.
    pub fn column_position(index: usize, offset: f64, width: i32) -> f64 {
        index as f64 * width.max(0) as f64 - offset
    }

    /// Index of the leftmost column under the viewport, possibly only
    /// partially visible.
    pub fn first_visible_index(offset: f64, width: i32) -> usize {
        (offset.max(0.0) / width.max(1) as f64).floor() as usize
    }

    /// Offset after the column at `removed_index` was removed.
    ///
    /// Removing a column left of the active viewport shifts the offset one
    /// column width to the left so the viewport stays on the same logical
    /// column; the result is re-clamped to the shrunk strip.
    pub fn offset_after_removal(
        offset: f64,
        removed_index: usize,
        old_active: usize,
        count_after: usize,
        width: i32,
    ) -> f64 {
        let shifted = if removed_index < old_active {
            offset - width.max(0) as f64
        } else {
            offset
        };
        Self::clamp_offset(shifted, count_after, width)
    }

    /// Offset after a column was inserted. Insertion never shifts the
    /// viewport; the offset is only re-clamped to the (grown) strip.
    pub fn offset_after_insert(offset: f64, count_after: usize, width: i32) -> f64 {
        Self::clamp_offset(offset, count_after, width)
    }
}

/// Default viewport position for an output: centered on column 0.
pub fn default_viewport(output_size: Size<i32, Logical>) -> Point<i32, Logical> {
    Point::from((viewport_offset_for_index(0, output_size.w), 0))
}

/// Pure pan animation of the strip viewport offset between two columns.
///
/// Time-based and side-effect free: the animation state only answers
/// "what is the offset at time `now`". The logical `active` workspace has
/// already changed by the time the animation starts — the pan merely makes
/// the visual offset catch up. Retargeting always continues from the
/// current *visual* offset (`value(now)`), never from a previous target, so
/// rapid successive inputs compose without corrupting the state.
#[derive(Debug, Clone, PartialEq)]
pub struct PanAnimation {
    from: f64,
    to: f64,
    start: Instant,
    duration: Duration,
    bounds: (f64, f64),
}

impl PanAnimation {
    /// Animate the offset from `from` to the target of column `index`,
    /// clamped to the strip of `count` columns of `width`.
    pub fn new(
        from: f64,
        index: usize,
        count: usize,
        width: i32,
        duration: Duration,
        now: Instant,
    ) -> Self {
        let to = StripState::clamp_offset(StripState::target_offset(index, width), count, width);
        Self {
            from,
            to,
            start: now,
            duration,
            bounds: (0.0, StripState::max_offset(count, width)),
        }
    }

    /// Retarget the pan to column `index`, continuing from the offset the
    /// animation has visually reached at `now`. Callers must apply the
    /// retargeted animation; the old one is superseded (no state is shared).
    pub fn retarget(
        &self,
        index: usize,
        count: usize,
        width: i32,
        duration: Duration,
        now: Instant,
    ) -> Self {
        Self::new(self.value(now), index, count, width, duration, now)
    }

    /// Visual offset of the viewport at `now`, eased and clamped to the
    /// strip bounds. Always converges to `to` once `now >= start + duration`.
    pub fn value(&self, now: Instant) -> f64 {
        let progress = (now.saturating_duration_since(self.start).as_secs_f64()
            / self.duration.as_secs_f64())
        .clamp(0.0, 1.0);
        let eased = ease(EaseInOutCubic, 0.0, 1.0, progress as f32) as f64;
        let value = self.from + (self.to - self.from) * eased;
        value.clamp(self.bounds.0, self.bounds.1)
    }

    /// Whether the animation has converged to its target at `now`. A pan with
    /// no distance to cover (already on target) is done immediately.
    pub fn is_done(&self, now: Instant) -> bool {
        self.from == self.to || now.saturating_duration_since(self.start) >= self.duration
    }

    /// Final offset the animation converges to.
    pub fn target(&self) -> f64 {
        self.to
    }
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
    fn intermediate_offset_places_neighbours_partially_visible() {
        // Viewport halfway between columns 1 and 2: both are half visible.
        let offset = 1.5 * W as f64;
        let p1 = StripState::column_position(1, offset, W);
        let p2 = StripState::column_position(2, offset, W);
        assert_eq!(p1, -(W as f64) / 2.0);
        assert_eq!(p2, (W as f64) / 2.0);
        // The leftmost (partially visible) column is still column 1, and both
        // neighbours straddle the viewport edges `[0, W]`.
        assert_eq!(StripState::first_visible_index(offset, W), 1);
        assert!(p1 < 0.0 && p1 + W as f64 > 0.0);
        assert!(p2 > 0.0 && p2 < W as f64);
    }

    #[test]
    fn snapped_offset_matches_discrete_layout() {
        // With offset == target, the continuous geometry must be identical to
        // the S1 discrete positioning.
        for idx in 0..5usize {
            let offset = StripState::target_offset(idx, W);
            for col in 0..5usize {
                let discrete = column_position(col, viewport_offset_for_index(idx, W), W);
                assert_eq!(
                    StripState::column_position(col, offset, W),
                    discrete.x as f64
                );
            }
        }
    }

    #[test]
    fn clamp_offset_bounds_the_strip() {
        assert_eq!(StripState::clamp_offset(-100.0, 5, W), 0.0);
        assert_eq!(
            StripState::clamp_offset(4.0 * W as f64, 5, W),
            4.0 * W as f64
        );
        assert_eq!(
            StripState::clamp_offset(99.0 * W as f64, 5, W),
            4.0 * W as f64
        );
        // Degenerate strips clamp to 0.
        assert_eq!(StripState::clamp_offset(50.0, 1, W), 0.0);
        assert_eq!(StripState::clamp_offset(50.0, 0, W), 0.0);
    }

    #[test]
    fn snap_to_lands_on_active_column() {
        let mut strip = StripState::new();
        assert_eq!(strip.offset, 0.0);
        strip.snap_to(3, 5, W);
        assert_eq!(strip.offset, 3.0 * W as f64);
        // Beyond the last column clamps back onto the strip.
        strip.snap_to(9, 5, W);
        assert_eq!(strip.offset, 4.0 * W as f64);
    }

    #[test]
    fn removal_before_active_shifts_offset_left() {
        // Viewport on column 2, remove column 0: same logical column is now
        // index 1, so the offset shifts one width left.
        let offset = StripState::target_offset(2, W);
        assert_eq!(
            StripState::offset_after_removal(offset, 0, 2, 4, W),
            StripState::target_offset(1, W)
        );
        // Removing a column at or after the active viewport keeps the offset.
        assert_eq!(StripState::offset_after_removal(offset, 3, 2, 4, W), offset);
        // Intermediate offsets shift by exactly one width too.
        let mid = 1.5 * W as f64;
        assert_eq!(
            StripState::offset_after_removal(mid, 0, 2, 4, W),
            mid - W as f64
        );
        // Clamp keeps the result on the shrunken strip.
        assert_eq!(StripState::offset_after_removal(0.0, 0, 0, 3, W), 0.0);
    }

    #[test]
    fn insert_never_shifts_the_viewport() {
        let offset = StripState::target_offset(2, W);
        assert_eq!(StripState::offset_after_insert(offset, 6, W), offset);
        // An offset past the (hypothetically shrunk) strip is re-clamped.
        assert_eq!(
            StripState::offset_after_insert(4.0 * W as f64, 3, W),
            2.0 * W as f64
        );
    }

    #[test]
    fn default_viewport_is_first_column() {
        assert_eq!(default_viewport(Size::from((W, 1080))), Point::from((0, 0)));
    }

    mod pan {
        use std::time::Duration;

        use super::*;

        const DUR: Duration = Duration::from_millis(200);
        const COUNT: usize = 5;

        #[test]
        fn identity_when_already_on_target() {
            let start = Instant::now();
            let anim = PanAnimation::new(2.0 * W as f64, 2, COUNT, W, DUR, start);
            assert!(anim.is_done(start));
            assert_eq!(anim.value(start), 2.0 * W as f64);
            assert_eq!(anim.target(), 2.0 * W as f64);
        }

        #[test]
        fn converges_to_target() {
            let start = Instant::now();
            let anim = PanAnimation::new(0.0, 3, COUNT, W, DUR, start);
            assert!(!anim.is_done(start));
            assert_eq!(anim.value(start), 0.0, "starts at `from`");
            let mid = anim.value(start + DUR / 2);
            assert!(mid > 0.0 && mid < 3.0 * W as f64, "mid={mid}");
            assert!(anim.is_done(start + DUR));
            assert_eq!(anim.value(start + DUR), 3.0 * W as f64);
            assert_eq!(anim.value(start + 2 * DUR), 3.0 * W as f64);
        }

        #[test]
        fn retarget_continues_from_current_offset() {
            let start = Instant::now();
            let anim = PanAnimation::new(0.0, 3, COUNT, W, DUR, start);
            let now = start + DUR / 2;
            let current = anim.value(now);
            // Reverse direction mid-flight towards column 0.
            let retargeted = anim.retarget(0, COUNT, W, DUR, now);
            assert_eq!(retargeted.value(now), current);
            assert_eq!(retargeted.target(), 0.0);
            assert!(!retargeted.is_done(now));
            assert_eq!(retargeted.value(now + DUR), 0.0);
        }

        #[test]
        fn retarget_from_finished_animation_is_identity() {
            let start = Instant::now();
            let anim = PanAnimation::new(0.0, 2, COUNT, W, DUR, start);
            let now = start + 2 * DUR;
            let retargeted = anim.retarget(2, COUNT, W, DUR, now);
            assert!(retargeted.is_done(now));
            assert_eq!(retargeted.value(now), anim.value(now));
        }

        #[test]
        fn value_is_clamped_to_strip_bounds() {
            let start = Instant::now();
            let max = StripState::max_offset(COUNT, W);
            // Target clamped when the index is past the last column.
            let anim = PanAnimation::new(0.0, 42, COUNT, W, DUR, start);
            assert_eq!(anim.target(), max);
            for i in 0..=10u32 {
                let t = start + DUR * i / 10;
                let v = anim.value(t);
                assert!((0.0..=max).contains(&v), "t={t:?} v={v}");
            }
            // A single-column strip always clamps to 0.
            let anim = PanAnimation::new(1.5 * W as f64, 0, 1, W, DUR, start);
            assert_eq!(anim.target(), 0.0);
            assert_eq!(anim.value(start + DUR), 0.0);
        }
    }
}
