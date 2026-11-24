//! Two-finger gestures for Tcode's mobile platform backends.
//!
//! gpui-fast's portable recognizer follows one touch at a time and recognizes
//! no pinch, so a second finger is resolved here, before gpui sees it: the
//! first finger's gesture is cancelled, and the pair then moves as a pan of its
//! midpoint and scales as a pinch about it.

use gpui::{
    Modifiers, PinchEvent, Pixels, PlatformInput, Point, ScrollDelta, ScrollWheelEvent, TouchEvent,
    TouchId, TouchPhase,
};

#[derive(Default)]
pub struct TwoFingerGestures {
    /// Fingers down, in the order they landed; the first two make the pinch.
    touches: Vec<(TouchId, Point<Pixels>)>,
    state: State,
}

#[derive(Default)]
enum State {
    /// One finger: its touches reach gpui's recognizer unchanged.
    #[default]
    Forwarding,
    Pinching {
        /// Where the pinch began; its pan stays with the element found there,
        /// as gpui keeps a one-finger pan with the element it began on.
        origin: Point<Pixels>,
        midpoint: Point<Pixels>,
        span: f32,
        moved: bool,
    },
    /// A pinch ended with fingers still down. They are held back until all
    /// lift, so the finger left behind does not turn into a pan or a tap.
    Draining,
}

impl TwoFingerGestures {
    /// Resolves the touches the platform reported together into the input
    /// gpui receives, in order.
    pub fn handle(&mut self, events: impl IntoIterator<Item = TouchEvent>) -> Vec<PlatformInput> {
        let mut inputs = Vec::new();
        for event in events {
            self.touch(event, &mut inputs);
        }
        self.flush_pan(&mut inputs);
        inputs
    }

    fn touch(&mut self, event: TouchEvent, inputs: &mut Vec<PlatformInput>) {
        match event.phase {
            TouchPhase::Started => {
                self.touches.push((event.id, event.position));
                match self.state {
                    State::Forwarding if self.touches.len() == 1 => {
                        inputs.push(PlatformInput::Touch(event));
                    }
                    State::Forwarding => {
                        let (first, position) = self.touches[0];
                        inputs.push(PlatformInput::Touch(TouchEvent {
                            id: first,
                            phase: TouchPhase::Cancelled,
                            position,
                            predicted_position: None,
                            force: None,
                        }));
                        let (midpoint, span) = self.pair();
                        self.state = State::Pinching {
                            origin: midpoint,
                            midpoint,
                            span,
                            moved: false,
                        };
                        inputs.push(scroll(midpoint, Point::default(), TouchPhase::Started));
                        inputs.push(pinch(midpoint, 0., TouchPhase::Started));
                    }
                    State::Pinching { .. } | State::Draining => {}
                }
            }
            TouchPhase::Moved => {
                if let Some(touch) = self.touches.iter_mut().find(|(id, _)| *id == event.id) {
                    touch.1 = event.position;
                }
                match &mut self.state {
                    State::Forwarding => inputs.push(PlatformInput::Touch(event)),
                    State::Pinching { moved, .. } => *moved = true,
                    State::Draining => {}
                }
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                let index = self.touches.iter().position(|(id, _)| *id == event.id);
                match self.state {
                    // A release is forwarded even for a finger not tracked
                    // since a reset, so gpui's recognizer never waits on it.
                    State::Forwarding => inputs.push(PlatformInput::Touch(event)),
                    State::Pinching { .. } if index.is_some_and(|index| index < 2) => {
                        self.flush_pan(inputs);
                        if let State::Pinching {
                            origin, midpoint, ..
                        } = self.state
                        {
                            inputs.push(scroll(origin, Point::default(), TouchPhase::Ended));
                            inputs.push(pinch(midpoint, 0., TouchPhase::Ended));
                        }
                        self.state = State::Draining;
                    }
                    State::Pinching { .. } | State::Draining => {}
                }
                if let Some(index) = index {
                    self.touches.remove(index);
                }
                if self.touches.is_empty() {
                    self.state = State::Forwarding;
                }
            }
        }
    }

    fn flush_pan(&mut self, inputs: &mut Vec<PlatformInput>) {
        let State::Pinching { moved: true, .. } = self.state else {
            return;
        };
        let (next_midpoint, next_span) = self.pair();
        let State::Pinching {
            origin,
            midpoint,
            span,
            moved,
        } = &mut self.state
        else {
            return;
        };
        *moved = false;
        inputs.push(scroll(
            *origin,
            next_midpoint - *midpoint,
            TouchPhase::Moved,
        ));
        if *span > 0. && next_span > 0. {
            inputs.push(pinch(
                next_midpoint,
                next_span / *span - 1.,
                TouchPhase::Moved,
            ));
        }
        *span = next_span;
        *midpoint = next_midpoint;
    }

    fn pair(&self) -> (Point<Pixels>, f32) {
        let (a, b) = (self.touches[0].1, self.touches[1].1);
        let offset = b - a;
        (
            a + offset / 2.,
            f32::from(offset.x).hypot(f32::from(offset.y)),
        )
    }
}

fn scroll(position: Point<Pixels>, delta: Point<Pixels>, touch_phase: TouchPhase) -> PlatformInput {
    PlatformInput::ScrollWheel(ScrollWheelEvent {
        position,
        delta: ScrollDelta::Pixels(delta),
        modifiers: Modifiers::default(),
        touch_phase,
    })
}

fn pinch(position: Point<Pixels>, delta: f32, phase: TouchPhase) -> PlatformInput {
    PlatformInput::Pinch(PinchEvent {
        position,
        delta,
        modifiers: Modifiers::default(),
        phase,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px};

    fn touch(id: u64, phase: TouchPhase, x: f32, y: f32) -> TouchEvent {
        TouchEvent {
            id: TouchId(id),
            phase,
            position: point(px(x), px(y)),
            predicted_position: None,
            force: None,
        }
    }

    /// What each input says, so a step reads as the events gpui will see.
    fn describe(inputs: Vec<PlatformInput>) -> Vec<String> {
        inputs
            .into_iter()
            .map(|input| match input {
                PlatformInput::Touch(event) => format!(
                    "touch {} {:?} at {:?}",
                    event.id.0,
                    event.phase,
                    (f32::from(event.position.x), f32::from(event.position.y))
                ),
                PlatformInput::ScrollWheel(event) => {
                    let delta = event.delta.pixel_delta(px(1.));
                    format!(
                        "scroll {:?} by {:?} at {:?}",
                        event.touch_phase,
                        (f32::from(delta.x), f32::from(delta.y)),
                        (f32::from(event.position.x), f32::from(event.position.y))
                    )
                }
                PlatformInput::Pinch(event) => format!(
                    "pinch {:?} by {} at {:?}",
                    event.phase,
                    event.delta,
                    (f32::from(event.position.x), f32::from(event.position.y))
                ),
                other => panic!("unexpected input {other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_second_finger_turns_the_touch_into_a_pinch_until_every_finger_lifts() {
        let mut gestures = TwoFingerGestures::default();

        // One finger is gpui's to recognize, untouched.
        assert_eq!(
            describe(gestures.handle([touch(1, TouchPhase::Started, 100., 100.)])),
            ["touch 1 Started at (100.0, 100.0)"]
        );
        assert_eq!(
            describe(gestures.handle([touch(1, TouchPhase::Moved, 100., 110.)])),
            ["touch 1 Moved at (100.0, 110.0)"]
        );

        // The second finger cancels the first finger's tap or pan, and the
        // pair begins at its midpoint.
        assert_eq!(
            describe(gestures.handle([touch(2, TouchPhase::Started, 200., 110.)])),
            [
                "touch 1 Cancelled at (100.0, 110.0)",
                "scroll Started by (0.0, 0.0) at (150.0, 110.0)",
                "pinch Started by 0 at (150.0, 110.0)",
            ]
        );

        // UIKit reports only the fingers that moved. Spreading one finger to
        // twice the span pans by the midpoint's travel and scales by 2 about
        // where the midpoint now is.
        assert_eq!(
            describe(gestures.handle([touch(2, TouchPhase::Moved, 300., 110.)])),
            [
                "scroll Moved by (50.0, 0.0) at (150.0, 110.0)",
                "pinch Moved by 1 at (200.0, 110.0)",
            ]
        );

        // A batch in which both fingers move resolves into one step.
        assert_eq!(
            describe(gestures.handle([
                touch(1, TouchPhase::Moved, 100., 160.),
                touch(2, TouchPhase::Moved, 300., 160.),
            ])),
            [
                "scroll Moved by (0.0, 50.0) at (150.0, 110.0)",
                "pinch Moved by 0 at (200.0, 160.0)",
            ]
        );

        // Lifting either finger ends the pinch; the one left behind neither
        // pans nor taps.
        assert_eq!(
            describe(gestures.handle([touch(1, TouchPhase::Ended, 100., 160.)])),
            [
                "scroll Ended by (0.0, 0.0) at (150.0, 110.0)",
                "pinch Ended by 0 at (200.0, 160.0)",
            ]
        );
        assert!(
            gestures
                .handle([touch(2, TouchPhase::Moved, 250., 160.)])
                .is_empty()
        );
        assert!(
            gestures
                .handle([touch(2, TouchPhase::Ended, 250., 160.)])
                .is_empty()
        );

        // Once every finger is up, the next touch is gpui's again.
        assert_eq!(
            describe(gestures.handle([touch(3, TouchPhase::Started, 10., 10.)])),
            ["touch 3 Started at (10.0, 10.0)"]
        );
    }
}
