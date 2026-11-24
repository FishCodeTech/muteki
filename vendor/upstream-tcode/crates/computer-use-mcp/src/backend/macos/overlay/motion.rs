use std::collections::VecDeque;
use std::f64::consts::TAU;
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

pub(super) type Point = (f64, f64);

/// Effect played when the cursor reaches a waypoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Effect {
    Click,
    /// Starts a drag: the cursor shrinks and trails ripples until `Release`.
    Press,
    Release,
}

pub(super) struct Waypoint {
    pub(super) point: Point,
    pub(super) effect: Option<Effect>,
    /// Signalled when the cursor reaches the point; dropping it unsent (the
    /// waypoint was discarded) releases the waiter as well.
    pub(super) reached: Option<SyncSender<()>>,
}

impl Waypoint {
    pub(super) fn new(point: Point, effect: Option<Effect>) -> Self {
        Self {
            point,
            effect,
            reached: None,
        }
    }

    fn arrive(self) -> Option<Effect> {
        if let Some(reached) = self.reached {
            let _ = reached.try_send(());
        }
        self.effect
    }
}

// Actions normally wait for the cursor, so a backlog only builds when they do
// not. Beyond this the oldest waypoints are dropped so the cursor never trails
// the agent by seconds.
const MAX_QUEUED: usize = 6;
const DWELL_AFTER_EFFECT: Duration = Duration::from_millis(120);

#[derive(Clone, Copy)]
enum Ease {
    /// Minimum-jerk profile of an aimed hand movement: slow start, fast
    /// middle, slow landing.
    Aimed,
    Out,
}

impl Ease {
    fn apply(self, t: f64) -> f64 {
        match self {
            Self::Aimed => t * t * t * (10.0 + t * (-15.0 + 6.0 * t)),
            Self::Out => 1.0 - (1.0 - t).powi(3),
        }
    }
}

#[derive(Clone, Copy)]
enum Shape {
    Curve([Point; 4]),
    /// Full turn around `center`, starting and ending on the same point.
    Loop {
        center: Point,
        radius: f64,
        start_angle: f64,
        direction: f64,
    },
}

#[derive(Clone, Copy)]
struct Segment {
    shape: Shape,
    duration: Duration,
    ease: Ease,
}

impl Segment {
    fn sample(&self, t: f64) -> Point {
        let t = self.ease.apply(t.clamp(0.0, 1.0));
        match self.shape {
            Shape::Curve([p0, p1, p2, p3]) => {
                let u = 1.0 - t;
                let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
                (
                    a * p0.0 + b * p1.0 + c * p2.0 + d * p3.0,
                    a * p0.1 + b * p1.1 + c * p2.1 + d * p3.1,
                )
            }
            Shape::Loop {
                center,
                radius,
                start_angle,
                direction,
            } => {
                let angle = start_angle + direction * TAU * t;
                (
                    center.0 + radius * angle.cos(),
                    center.1 + radius * angle.sin(),
                )
            }
        }
    }
}

struct Leg {
    waypoint: Waypoint,
    segments: Vec<Segment>,
    start: Instant,
}

impl Leg {
    fn duration(&self) -> Duration {
        self.segments.iter().map(|segment| segment.duration).sum()
    }

    fn sample(&self, now: Instant) -> Point {
        let mut elapsed = now.saturating_duration_since(self.start);
        for segment in &self.segments {
            if elapsed < segment.duration {
                return segment.sample(elapsed.as_secs_f64() / segment.duration.as_secs_f64());
            }
            elapsed -= segment.duration;
        }
        self.waypoint.point
    }
}

/// Cursor path in AX global coordinates, advanced by the overlay's frame tick.
pub(super) struct Motion {
    resting: Option<Point>,
    active: Option<Leg>,
    queued: VecDeque<Waypoint>,
    hold_until: Option<Instant>,
    pressed: bool,
    rng: Rng,
}

impl Motion {
    pub(super) fn new(seed: u64) -> Self {
        Self {
            resting: None,
            active: None,
            queued: VecDeque::new(),
            hold_until: None,
            pressed: false,
            rng: Rng(seed),
        }
    }

    pub(super) fn position(&self, now: Instant) -> Option<Point> {
        self.active
            .as_ref()
            .map(|leg| leg.sample(now))
            .or(self.resting)
    }

    pub(super) fn is_moving(&self) -> bool {
        self.active.is_some() || !self.queued.is_empty()
    }

    /// Whether a drag holds the button, from its press until its release.
    pub(super) fn is_pressed(&self) -> bool {
        self.pressed
    }

    /// Returns effects that happen immediately because the cursor had no
    /// position yet and appears on its first waypoint.
    ///
    /// A leg that ends in an effect finishes before later waypoints start, so
    /// each effect lands where its action happened; a plain glide is
    /// redirected from wherever it currently is.
    pub(super) fn push(&mut self, now: Instant, waypoints: Vec<Waypoint>) -> Vec<Effect> {
        let mut waypoints = waypoints.into_iter();
        let mut fired = Vec::new();
        if self.resting.is_none() && self.active.is_none() {
            let Some(first) = waypoints.next() else {
                return fired;
            };
            self.resting = Some(first.point);
            fired.extend(self.arrive(first, now));
        } else if self.queued.is_empty()
            && self
                .active
                .as_ref()
                .is_some_and(|leg| leg.waypoint.effect.is_none())
        {
            self.resting = self.position(now);
            if let Some(leg) = self.active.take() {
                leg.waypoint.arrive();
            }
        }
        self.queued.extend(waypoints);
        while self.queued.len() > MAX_QUEUED {
            self.queued.pop_front();
        }
        self.start_next(now);
        fired
    }

    /// Advances to `now` and returns the effects reached on the way.
    pub(super) fn advance(&mut self, now: Instant) -> Vec<Effect> {
        let mut fired = Vec::new();
        loop {
            if let Some(leg) = self.active.take_if(|leg| now >= leg.start + leg.duration()) {
                self.resting = Some(leg.waypoint.point);
                fired.extend(self.arrive(leg.waypoint, now));
            }
            if self.active.is_some() || !self.start_next(now) {
                return fired;
            }
        }
    }

    /// Stops where the cursor is and forgets pending waypoints.
    pub(super) fn halt(&mut self, now: Instant) {
        self.resting = self.position(now);
        self.active = None;
        self.queued.clear();
        self.hold_until = None;
        self.pressed = false;
    }

    fn arrive(&mut self, waypoint: Waypoint, now: Instant) -> Option<Effect> {
        let effect = waypoint.arrive();
        if let Some(effect) = effect {
            self.hold_until = Some(now + DWELL_AFTER_EFFECT);
            match effect {
                Effect::Press => self.pressed = true,
                Effect::Release => self.pressed = false,
                Effect::Click => {}
            }
        }
        effect
    }

    fn start_next(&mut self, now: Instant) -> bool {
        if self.active.is_some() || self.hold_until.is_some_and(|until| now < until) {
            return false;
        }
        let Some(waypoint) = self.queued.pop_front() else {
            return false;
        };
        let from = self.resting.unwrap_or(waypoint.point);
        self.active = Some(Leg {
            segments: plan(from, waypoint.point, self.pressed, &mut self.rng),
            waypoint,
            start: now,
        });
        true
    }
}

/// Plans a hand-like path: a gently bowed, minimum-jerk stroke whose duration
/// follows Fitts' law, sometimes preceded by a hesitation (a small backswing
/// or a loop on the spot) and sometimes overshooting before settling. A drag
/// is slower and steadier, with neither.
fn plan(from: Point, to: Point, dragging: bool, rng: &mut Rng) -> Vec<Segment> {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let distance = dx.hypot(dy);
    if distance < 1.0 {
        return Vec::new();
    }
    let along = (dx / distance, dy / distance);
    let across = (-along.1, along.0);
    let offset = |point: Point, axis: Point, amount: f64| {
        (point.0 + axis.0 * amount, point.1 + axis.1 * amount)
    };
    let mut segments = Vec::new();
    let mut start = from;

    if !dragging && distance > 60.0 {
        let roll = rng.unit();
        if roll < 0.12 {
            let radius = rng.range(4.0, 8.0);
            let side = rng.sign();
            let center = offset(from, across, radius * side);
            segments.push(Segment {
                shape: Shape::Loop {
                    center,
                    radius,
                    start_angle: (from.1 - center.1).atan2(from.0 - center.0),
                    direction: rng.sign(),
                },
                duration: Duration::from_secs_f64(rng.range(0.18, 0.26)),
                ease: Ease::Aimed,
            });
        } else if roll < 0.32 {
            let back = offset(from, along, -rng.range(4.0, 10.0));
            let back = offset(back, across, rng.range(-3.0, 3.0));
            segments.push(Segment {
                shape: Shape::Curve([from, from, back, back]),
                duration: Duration::from_secs_f64(rng.range(0.07, 0.11)),
                ease: Ease::Out,
            });
            start = back;
        }
    }

    let fitts = 0.1 + 0.085 * (1.0 + distance / 16.0).log2();
    let mut seconds = (fitts * rng.range(0.9, 1.1)).clamp(0.16, 0.75);
    if dragging {
        seconds = (seconds * 1.3).min(0.9);
    }
    let overshoot = !dragging && distance > 150.0 && rng.unit() < 0.3;
    let end = if overshoot {
        let past = offset(to, along, distance * rng.range(0.02, 0.05));
        offset(past, across, rng.range(-4.0, 4.0))
    } else {
        to
    };
    let (sx, sy) = (end.0 - start.0, end.1 - start.1);
    let bow = distance
        * rng.sign()
        * if dragging {
            rng.range(0.01, 0.05)
        } else {
            rng.range(0.04, 0.14)
        };
    let c1 = offset((start.0 + sx * 0.3, start.1 + sy * 0.3), across, bow);
    let c2 = offset(
        (start.0 + sx * 0.75, start.1 + sy * 0.75),
        across,
        bow * rng.range(0.2, 0.7),
    );
    segments.push(Segment {
        shape: Shape::Curve([start, c1, c2, end]),
        duration: Duration::from_secs_f64(if overshoot { seconds * 0.9 } else { seconds }),
        ease: Ease::Aimed,
    });
    if overshoot {
        segments.push(Segment {
            shape: Shape::Curve([end, end, to, to]),
            duration: Duration::from_secs_f64(rng.range(0.1, 0.16)),
            ease: Ease::Aimed,
        });
    }
    segments
}

/// SplitMix64: the path only needs to look varied, and a seed keeps tests
/// deterministic.
struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (z ^ (z >> 31)) as f64 / u64::MAX as f64
    }

    fn range(&mut self, low: f64, high: f64) -> f64 {
        low + (high - low) * self.unit()
    }

    fn sign(&mut self) -> f64 {
        if self.unit() < 0.5 { -1.0 } else { 1.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_millis(16);

    fn run_until_still(motion: &mut Motion, start: Instant) -> Vec<(Point, Effect)> {
        let mut now = start;
        let mut effects = Vec::new();
        while motion.is_moving() {
            now += FRAME;
            for effect in motion.advance(now) {
                effects.push((motion.position(now).unwrap(), effect));
            }
        }
        effects
    }

    #[test]
    fn each_effect_lands_on_its_own_action_point_in_action_order() {
        let now = Instant::now();
        let mut motion = Motion::new(7);
        let (reached, arrival) = std::sync::mpsc::sync_channel(1);
        let mut click = Waypoint::new((400.0, 300.0), Some(Effect::Click));
        click.reached = Some(reached);
        assert_eq!(
            motion.push(now, vec![Waypoint::new((10.0, 10.0), Some(Effect::Click))]),
            vec![Effect::Click],
            "the first action appears on its point instead of gliding from nowhere"
        );
        motion.push(now, vec![click]);
        assert!(
            arrival.try_recv().is_err(),
            "an action waits for its cursor"
        );
        motion.push(now, vec![Waypoint::new((50.0, 600.0), None)]);
        motion.push(
            now,
            vec![
                Waypoint::new((700.0, 100.0), Some(Effect::Press)),
                Waypoint::new((900.0, 200.0), Some(Effect::Release)),
            ],
        );
        assert_eq!(
            run_until_still(&mut motion, now),
            vec![
                ((400.0, 300.0), Effect::Click),
                ((700.0, 100.0), Effect::Press),
                ((900.0, 200.0), Effect::Release),
            ]
        );
        assert!(arrival.try_recv().is_ok());
    }

    #[test]
    fn a_new_action_redirects_a_plain_glide_from_where_it_is() {
        let now = Instant::now();
        let mut motion = Motion::new(11);
        motion.push(now, vec![Waypoint::new((0.0, 0.0), None)]);
        motion.push(now, vec![Waypoint::new((1_000.0, 0.0), None)]);
        let midway = now + Duration::from_millis(250);
        motion.advance(midway);
        let before = motion.position(midway).unwrap();
        assert!(before != (0.0, 0.0) && before != (1_000.0, 0.0));
        motion.push(midway, vec![Waypoint::new((0.0, 500.0), None)]);
        assert_eq!(motion.position(midway), Some(before), "no jump on redirect");
        run_until_still(&mut motion, midway);
        assert_eq!(
            motion.position(midway + Duration::from_secs(5)),
            Some((0.0, 500.0))
        );
    }

    #[test]
    fn glides_accelerate_and_land_softly_without_straying_from_the_route() {
        for seed in 0..200 {
            let now = Instant::now();
            let mut motion = Motion::new(seed);
            motion.push(now, vec![Waypoint::new((100.0, 100.0), None)]);
            motion.push(now, vec![Waypoint::new((900.0, 500.0), None)]);
            let mut samples = vec![(100.0, 100.0)];
            let mut time = now;
            while motion.is_moving() {
                time += FRAME;
                motion.advance(time);
                samples.push(motion.position(time).unwrap());
            }
            let steps: Vec<f64> = samples
                .windows(2)
                .map(|pair| (pair[1].0 - pair[0].0).hypot(pair[1].1 - pair[0].1))
                .collect();
            let peak = steps.iter().cloned().fold(0.0, f64::max);
            let landing = steps[steps.len().saturating_sub(2)..]
                .iter()
                .cloned()
                .fold(0.0, f64::max);
            assert!(landing < peak * 0.35, "seed {seed}: landing is not eased");
            assert!(steps.len() > 8, "seed {seed}: the glide is visible");
            let (ax, ay, bx, by): (f64, f64, f64, f64) = (100.0, 100.0, 900.0, 500.0);
            let length = (bx - ax).hypot(by - ay);
            for (x, y) in &samples {
                let off_route = ((bx - ax) * (ay - y) - (ax - x) * (by - ay)).abs() / length;
                assert!(off_route < length * 0.15 + 12.0, "seed {seed}: strayed");
            }
            assert_eq!(samples.last(), Some(&(900.0, 500.0)));
        }
    }
}
