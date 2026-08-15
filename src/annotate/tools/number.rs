use crate::annotate::{Tool, ToolKind};
use crate::capture::region::Rect;

#[derive(Debug, Clone)]
pub struct NumberTool {
    pub center: (f64, f64),
    pub radius: f64,
    pub value: u32,
    pub fill: [f32; 4],
    pub text_color: [f32; 4],
}

impl Tool for NumberTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Number
    }
    fn bounds(&self) -> Rect {
        Rect {
            x: (self.center.0 - self.radius).floor() as i32,
            y: (self.center.1 - self.radius).floor() as i32,
            w: (self.radius * 2.0).ceil() as u32,
            h: (self.radius * 2.0).ceil() as u32,
        }
    }
    fn hit_test(&self, x: f64, y: f64) -> bool {
        let dx = x - self.center.0;
        let dy = y - self.center.1;
        (dx * dx + dy * dy).sqrt() <= self.radius
    }
    fn translate(&mut self, dx: f64, dy: f64) {
        self.center = (self.center.0 + dx, self.center.1 + dy);
    }
    fn clone_box(&self) -> Box<dyn Tool> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    fn tool() -> NumberTool {
        NumberTool {
            center: (50.0, 60.0),
            radius: 12.0,
            value: 3,
            fill: [1.0, 0.0, 0.0, 1.0],
            text_color: [1.0, 1.0, 1.0, 1.0],
        }
    }

    #[test]
    fn reports_its_kind() {
        assert_eq!(tool().kind(), ToolKind::Number);
    }

    #[rstest]
    #[case((50.0, 60.0), 12.0, Rect { x: 38, y: 48, w: 24, h: 24 })]
    // Fractional centre/radius: origin floors, extent ceils, so the box never clips the circle.
    #[case((50.5, 60.5), 12.25, Rect { x: 38, y: 48, w: 25, h: 25 })]
    #[case((5.0, 5.0), 10.0, Rect { x: -5, y: -5, w: 20, h: 20 })]
    fn bounds_floor_the_origin_and_ceil_the_extent(
        #[case] center: (f64, f64),
        #[case] radius: f64,
        #[case] expected: Rect,
    ) {
        let t = NumberTool {
            center,
            radius,
            ..tool()
        };
        assert_eq!(t.bounds(), expected);
    }

    /// Unlike every box-shaped tool, the number badge hit-tests against a *circle*.
    #[rstest]
    #[case::centre(50.0, 60.0, true)]
    #[case::on_the_radius(62.0, 60.0, true)]
    #[case::just_outside_the_radius(62.1, 60.0, false)]
    // Inside the bounding box but outside the inscribed circle.
    #[case::bounding_box_corner(62.0, 72.0, false)]
    fn hit_test_is_circular(#[case] x: f64, #[case] y: f64, #[case] expected: bool) {
        let t = tool();
        assert_eq!(t.hit_test(x, y), expected);
        if !expected {
            // The corner case above is only interesting if the box would have accepted it.
            let b = t.bounds();
            assert!(x <= b.right() as f64 + 0.2 && y <= b.bottom() as f64 + 0.2);
        }
    }

    #[test]
    fn translate_moves_the_centre_only() {
        let mut t = tool();
        t.translate(-10.5, 4.25);
        assert_eq!(t.center, (39.5, 64.25));
        assert_eq!(t.radius, 12.0);
    }

    #[test]
    fn clone_box_preserves_the_value() {
        let mut t = tool();
        assert!(t.as_any_mut().downcast_mut::<NumberTool>().is_some());
        let cloned = t.clone_box();
        assert_eq!(
            cloned.as_any().downcast_ref::<NumberTool>().unwrap().value,
            3
        );
    }
}
