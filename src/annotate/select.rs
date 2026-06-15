//! Pure-math helpers for the Select tool: resize-handle geometry, hit-zones, and the
//! resize / endpoint / radius math. Kept GTK-free (like [`super::render`]) so the
//! interaction math can be unit-tested without a compositor.

use crate::capture::region::Rect;

/// Visual side length (document px) of a resize handle square.
pub const HANDLE_DRAW: f64 = 8.0;
/// Half-extent (document px) of a handle's hit zone — the catch area is `2 * HANDLE_HALF_HIT`
/// on a side, slightly larger than the drawn square so handles are easy to grab.
pub const HANDLE_HALF_HIT: f64 = 6.0;
/// Minimum width/height a box shape may be resized to.
pub const MIN_BOX: f64 = 4.0;
/// Minimum length a 2-point shape (Arrow / Line) may be resized to.
pub const MIN_LEN: f64 = 4.0;
/// Minimum radius a Number disc may be resized to.
pub const MIN_RADIUS: f64 = 6.0;

/// The eight resize handles of a box-shaped layer: four corners + four edge midpoints.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BoxHandle {
    NW,
    N,
    NE,
    E,
    SE,
    S,
    SW,
    W,
}

/// Which endpoint of a 2-point shape (Arrow / Line) is being dragged.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    From,
    To,
}

impl BoxHandle {
    /// Handles in hit-test priority order: corners first, so on a tiny rectangle whose handle
    /// catch zones overlap, a press resolves deterministically to a corner (uniform resize)
    /// rather than an edge.
    pub const PRIORITY: [BoxHandle; 8] = [
        BoxHandle::NW,
        BoxHandle::NE,
        BoxHandle::SE,
        BoxHandle::SW,
        BoxHandle::N,
        BoxHandle::E,
        BoxHandle::S,
        BoxHandle::W,
    ];
}

/// Document-space centre point of `handle` on rectangle `r`.
pub fn box_handle_point(r: Rect, handle: BoxHandle) -> (f64, f64) {
    let x = r.x as f64;
    let y = r.y as f64;
    let w = r.w as f64;
    let h = r.h as f64;
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let (rx, by) = (x + w, y + h);
    match handle {
        BoxHandle::NW => (x, y),
        BoxHandle::N => (cx, y),
        BoxHandle::NE => (rx, y),
        BoxHandle::E => (rx, cy),
        BoxHandle::SE => (rx, by),
        BoxHandle::S => (cx, by),
        BoxHandle::SW => (x, by),
        BoxHandle::W => (x, cy),
    }
}

/// All eight handle centres of `r`, in [`BoxHandle::PRIORITY`] order.
pub fn box_handle_points(r: Rect) -> [(BoxHandle, (f64, f64)); 8] {
    BoxHandle::PRIORITY.map(|h| (h, box_handle_point(r, h)))
}

/// `true` if `(x, y)` falls within `handle`'s hit zone on rectangle `r`.
fn handle_hit(r: Rect, handle: BoxHandle, x: f64, y: f64) -> bool {
    let (hx, hy) = box_handle_point(r, handle);
    (x - hx).abs() <= HANDLE_HALF_HIT && (y - hy).abs() <= HANDLE_HALF_HIT
}

/// First box handle whose hit zone contains `(x, y)`, tested in corner-before-edge priority.
pub fn box_handle_at(r: Rect, x: f64, y: f64) -> Option<BoxHandle> {
    BoxHandle::PRIORITY
        .into_iter()
        .find(|&h| handle_hit(r, h, x, y))
}

/// `true` if `(x, y)` falls within the hit zone of a handle centred at `(hx, hy)`. Used for
/// the single-point handles of Arrow / Line endpoints and the Number radius grip.
pub fn point_handle_hit(center: (f64, f64), x: f64, y: f64) -> bool {
    (x - center.0).abs() <= HANDLE_HALF_HIT && (y - center.1).abs() <= HANDLE_HALF_HIT
}

/// Resize `orig` by moving `handle` to document point `(mx, my)`. The opposite edge(s) stay
/// anchored; edge handles move only their own axis. The result is normalized (always
/// non-negative `w`/`h`) so dragging a handle past the opposite edge flips cleanly, and each
/// axis is clamped to at least [`MIN_BOX`] (anchored on the fixed edge).
pub fn resize_box(orig: Rect, handle: BoxHandle, mx: f64, my: f64) -> Rect {
    let mut left = orig.x as f64;
    let mut top = orig.y as f64;
    let mut right = orig.right() as f64;
    let mut bottom = orig.bottom() as f64;

    // Move the edge(s) governed by this handle to the cursor.
    match handle {
        BoxHandle::W | BoxHandle::NW | BoxHandle::SW => left = mx,
        BoxHandle::E | BoxHandle::NE | BoxHandle::SE => right = mx,
        _ => {}
    }
    match handle {
        BoxHandle::N | BoxHandle::NW | BoxHandle::NE => top = my,
        BoxHandle::S | BoxHandle::SW | BoxHandle::SE => bottom = my,
        _ => {}
    }

    // Normalize so width/height are non-negative (handles drag-past-opposite-edge flips).
    let (mut x0, mut x1) = (left.min(right), left.max(right));
    let (mut y0, mut y1) = (top.min(bottom), top.max(bottom));

    // Enforce minimums, anchoring the edge that wasn't moved. After normalization the moving
    // edge is whichever of x0/x1 (resp. y0/y1) the cursor pushed; clamp the span by pushing
    // the *near* edge out so the fixed (far) edge stays put. We approximate "fixed edge" as
    // the original anchor: for W/E handles the opposite horizontal edge; for N/S the opposite
    // vertical edge. Using the normalized span keeps a collapsed drag from inverting madly.
    if x1 - x0 < MIN_BOX {
        // Decide which side is anchored from the handle.
        match handle {
            BoxHandle::W | BoxHandle::NW | BoxHandle::SW => x0 = x1 - MIN_BOX,
            BoxHandle::E | BoxHandle::NE | BoxHandle::SE => x1 = x0 + MIN_BOX,
            _ => x1 = x0 + MIN_BOX,
        }
    }
    if y1 - y0 < MIN_BOX {
        match handle {
            BoxHandle::N | BoxHandle::NW | BoxHandle::NE => y0 = y1 - MIN_BOX,
            BoxHandle::S | BoxHandle::SW | BoxHandle::SE => y1 = y0 + MIN_BOX,
            _ => y1 = y0 + MIN_BOX,
        }
    }

    let x = x0.round() as i32;
    let y = y0.round() as i32;
    let w = (x1 - x0).round().max(MIN_BOX) as u32;
    let h = (y1 - y0).round().max(MIN_BOX) as u32;
    Rect { x, y, w, h }
}

/// Move one endpoint of a 2-point shape to `(mx, my)`, keeping the other fixed. If the result
/// would be shorter than [`MIN_LEN`], the dragged endpoint is pushed out along the segment so
/// the shape stays grabbable. Returns the new `(from, to)`.
pub fn set_endpoint(
    from: (f64, f64),
    to: (f64, f64),
    which: Endpoint,
    mx: f64,
    my: f64,
) -> ((f64, f64), (f64, f64)) {
    let (anchor, mut moving) = match which {
        Endpoint::From => (to, (mx, my)),
        Endpoint::To => (from, (mx, my)),
    };
    let dx = moving.0 - anchor.0;
    let dy = moving.1 - anchor.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < MIN_LEN {
        if len > f64::EPSILON {
            let scale = MIN_LEN / len;
            moving = (anchor.0 + dx * scale, anchor.1 + dy * scale);
        } else {
            // Degenerate (cursor exactly on the anchor): nudge horizontally so the shape
            // doesn't collapse to a zero-length, unselectable segment.
            moving = (anchor.0 + MIN_LEN, anchor.1);
        }
    }
    match which {
        Endpoint::From => (moving, to),
        Endpoint::To => (from, moving),
    }
}

/// New Number radius from a drag of its east grip to `(mx, my)`: the distance from `center`,
/// floored at [`MIN_RADIUS`].
pub fn new_radius(center: (f64, f64), mx: f64, my: f64) -> f64 {
    let dx = mx - center.0;
    let dy = my - center.1;
    (dx * dx + dy * dy).sqrt().max(MIN_RADIUS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    fn rect() -> Rect {
        Rect {
            x: 10,
            y: 20,
            w: 100,
            h: 80,
        }
    }

    #[test]
    fn handle_points_sit_on_the_rectangle() {
        let r = rect();
        assert_eq!(box_handle_point(r, BoxHandle::NW), (10.0, 20.0));
        assert_eq!(box_handle_point(r, BoxHandle::N), (60.0, 20.0));
        assert_eq!(box_handle_point(r, BoxHandle::NE), (110.0, 20.0));
        assert_eq!(box_handle_point(r, BoxHandle::E), (110.0, 60.0));
        assert_eq!(box_handle_point(r, BoxHandle::SE), (110.0, 100.0));
        assert_eq!(box_handle_point(r, BoxHandle::S), (60.0, 100.0));
        assert_eq!(box_handle_point(r, BoxHandle::SW), (10.0, 100.0));
        assert_eq!(box_handle_point(r, BoxHandle::W), (10.0, 60.0));
    }

    #[test]
    fn box_handle_at_picks_the_nearest_handle() {
        let r = rect();
        assert_eq!(box_handle_at(r, 10.0, 20.0), Some(BoxHandle::NW));
        assert_eq!(box_handle_at(r, 110.0, 100.0), Some(BoxHandle::SE));
        // Inside the body, far from any handle → no handle.
        assert_eq!(box_handle_at(r, 60.0, 60.0), None);
    }

    #[test]
    fn box_handle_at_prefers_corner_over_edge_on_overlap() {
        // A tiny rect where corner and edge catch zones overlap near a corner.
        let r = Rect {
            x: 0,
            y: 0,
            w: 6,
            h: 6,
        };
        // The NW corner (0,0) wins over the W edge (0,3) and N edge (3,0) at the origin.
        assert_eq!(box_handle_at(r, 0.0, 0.0), Some(BoxHandle::NW));
    }

    #[test]
    fn resize_se_grows_from_nw_anchor() {
        let r = resize_box(rect(), BoxHandle::SE, 210.0, 220.0);
        assert_eq!(r.x, 10);
        assert_eq!(r.y, 20);
        assert_eq!(r.w, 200);
        assert_eq!(r.h, 200);
    }

    #[test]
    fn resize_nw_keeps_se_anchored() {
        // Moving NW to (60,70) should keep SE at (110,100).
        let r = resize_box(rect(), BoxHandle::NW, 60.0, 70.0);
        assert_eq!(r.x, 60);
        assert_eq!(r.y, 70);
        assert_eq!(r.right(), 110);
        assert_eq!(r.bottom(), 100);
    }

    #[test]
    fn resize_east_edge_locks_vertical_axis() {
        // Dragging the E edge changes width only; y/h stay.
        let r = resize_box(rect(), BoxHandle::E, 200.0, 999.0);
        assert_eq!(r.y, 20);
        assert_eq!(r.h, 80);
        assert_eq!(r.right(), 200);
    }

    #[test]
    fn resize_flips_past_opposite_edge() {
        // Drag the E handle to the left of the W edge (x=10) → normalized with positive width.
        let r = resize_box(rect(), BoxHandle::E, -40.0, 60.0);
        assert!(r.w >= MIN_BOX as u32);
        assert_eq!(r.x, -40);
        assert_eq!(r.right(), 10);
    }

    #[rstest]
    #[case(BoxHandle::SE)]
    #[case(BoxHandle::NW)]
    #[case(BoxHandle::N)]
    #[case(BoxHandle::E)]
    fn resize_never_collapses_below_minimum(#[case] handle: BoxHandle) {
        // Collapse the drag onto the anchor; both dimensions must stay >= MIN_BOX.
        let r = resize_box(rect(), handle, 10.0, 20.0);
        assert!(r.w >= MIN_BOX as u32, "width {} < MIN_BOX", r.w);
        assert!(r.h >= MIN_BOX as u32, "height {} < MIN_BOX", r.h);
    }

    #[test]
    fn set_endpoint_moves_only_the_dragged_end() {
        let (from, to) = set_endpoint((0.0, 0.0), (100.0, 0.0), Endpoint::To, 50.0, 50.0);
        assert_eq!(from, (0.0, 0.0));
        assert_eq!(to, (50.0, 50.0));
    }

    #[test]
    fn set_endpoint_enforces_minimum_length() {
        // Drop `to` right on `from`; the segment must keep at least MIN_LEN.
        let (from, to) = set_endpoint((10.0, 10.0), (100.0, 10.0), Endpoint::To, 10.0, 10.0);
        let len = ((to.0 - from.0).powi(2) + (to.1 - from.1).powi(2)).sqrt();
        assert!(len >= MIN_LEN - 1e-9, "len {len} < MIN_LEN");
    }

    #[test]
    fn new_radius_is_distance_floored_at_minimum() {
        assert_eq!(new_radius((0.0, 0.0), 30.0, 40.0), 50.0);
        assert_eq!(new_radius((0.0, 0.0), 1.0, 0.0), MIN_RADIUS);
    }
}
