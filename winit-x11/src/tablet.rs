//! Pen data of a tablet from the valuators of XInput 2.

use winit_core::event::{Force, TabletToolButton, TabletToolData, TabletToolTilt};

/// One valuator: its number and its range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Axis {
    pub number: i32,
    pub min: f64,
    pub max: f64,
}

impl Axis {
    /// The value as a share from 0 to 1 of the range.
    fn share(self, value: f64) -> Option<f64> {
        let span = self.max - self.min;
        if !value.is_finite() || !span.is_finite() || span <= 0.0 {
            return None;
        }

        Some(((value - self.min) / span).clamp(0.0, 1.0))
    }
}

/// The valuators of a pen that carry its pressure and tilt.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct TabletAxes {
    pub pressure: Option<Axis>,
    pub tilt_x: Option<Axis>,
    pub tilt_y: Option<Axis>,
    /// The last values, since an event carries only the valuators that changed.
    last: [Option<f64>; 3],
}

impl TabletAxes {
    /// Takes the valuators of one event, as number and value, and returns the
    /// data of the pen.
    pub(crate) fn data(&mut self, values: impl IntoIterator<Item = (i32, f64)>) -> TabletToolData {
        let axes = [self.pressure, self.tilt_x, self.tilt_y];
        for (number, value) in values {
            for (axis, last) in axes.iter().zip(self.last.iter_mut()) {
                if axis.is_some_and(|axis| axis.number == number) {
                    *last = Some(value);
                }
            }
        }

        let share = |axis: Option<Axis>, last: Option<f64>| axis.zip(last).and_then(|(axis, value)| axis.share(value));
        let [pressure, tilt_x, tilt_y] = self.last;
        let degrees = |share: f64| (share * 180.0 - 90.0).round() as i8;
        let tilt = match (share(self.tilt_x, tilt_x), share(self.tilt_y, tilt_y)) {
            (None, None) => None,
            (x, y) => Some(TabletToolTilt {
                x: x.map_or(0, degrees),
                y: y.map_or(0, degrees),
            }),
        };

        TabletToolData {
            force: share(self.pressure, pressure).map(Force::Normalized),
            tilt,
            ..TabletToolData::default()
        }
    }
}

/// The button of a pen an X button number stands for: 1 is the tip, 2 the
/// first button on the side, 3 the second one.
pub(crate) fn button(detail: u32) -> Option<TabletToolButton> {
    match detail {
        1 => Some(TabletToolButton::Contact),
        2 => Some(TabletToolButton::Barrel),
        3 => Some(TabletToolButton::Other(1)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axes() -> TabletAxes {
        TabletAxes {
            pressure: Some(Axis { number: 2, min: 0.0, max: 2048.0 }),
            tilt_x: Some(Axis { number: 3, min: -64.0, max: 63.0 }),
            tilt_y: Some(Axis { number: 4, min: -64.0, max: 63.0 }),
            ..TabletAxes::default()
        }
    }

    #[test]
    fn pressure_and_tilt_are_shares_of_their_range() {
        let mut axes = axes();
        let data = axes.data([(0, 10.0), (2, 1024.0), (3, 63.0), (4, -64.0)]);
        assert_eq!(data.force, Some(Force::Normalized(0.5)));
        assert_eq!(data.tilt, Some(TabletToolTilt { x: 90, y: -90 }));
    }

    #[test]
    fn a_valuator_left_out_keeps_its_last_value() {
        let mut axes = axes();
        axes.data([(2, 2048.0)]);
        let data = axes.data([(0, 1.0)]);
        assert_eq!(data.force, Some(Force::Normalized(1.0)));
        assert_eq!(data.tilt, None);
    }

    #[test]
    fn a_device_without_ranges_reports_nothing() {
        let mut axes = TabletAxes {
            pressure: Some(Axis { number: 2, min: 5.0, max: 5.0 }),
            ..TabletAxes::default()
        };
        assert_eq!(axes.data([(2, 5.0)]).force, None);
    }

    #[test]
    fn the_tip_and_two_side_buttons_are_named() {
        assert_eq!(button(1), Some(TabletToolButton::Contact));
        assert_eq!(button(2), Some(TabletToolButton::Barrel));
        assert_eq!(button(3), Some(TabletToolButton::Other(1)));
        assert_eq!(button(4), None);
    }
}
