use crate::outline::Frame;

#[derive(Clone, Copy)]
pub(super) struct DisplayGeometry {
    pub(super) ax: Frame,
    pub(super) appkit: Frame,
}

/// Converts an AX global point (y-down) into AppKit global coordinates
/// (y-up), using the display containing the point as the flip axis.
pub(super) fn ax_screen_to_appkit(point: (f64, f64), display: DisplayGeometry) -> (f64, f64) {
    (
        display.appkit.x + (point.0 - display.ax.x),
        display.appkit.y + display.ax.y + display.ax.h - point.1,
    )
}

pub(super) fn is_finite_point(point: (f64, f64)) -> bool {
    point.0.is_finite() && point.1.is_finite()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flips_ax_geometry_on_the_containing_display() {
        let display = DisplayGeometry {
            ax: Frame {
                x: 0.0,
                y: 0.0,
                w: 1_440.0,
                h: 900.0,
            },
            appkit: Frame {
                x: 0.0,
                y: 0.0,
                w: 1_440.0,
                h: 900.0,
            },
        };

        assert_eq!(ax_screen_to_appkit((100.0, 250.0), display), (100.0, 650.0));
        let offset_display = DisplayGeometry {
            ax: Frame {
                x: 1_440.0,
                y: 100.0,
                w: 1_920.0,
                h: 1_080.0,
            },
            appkit: Frame {
                x: 1_440.0,
                y: -280.0,
                w: 1_920.0,
                h: 1_080.0,
            },
        };
        assert_eq!(
            ax_screen_to_appkit((1_500.0, 200.0), offset_display),
            (1_500.0, 700.0)
        );
    }
}
