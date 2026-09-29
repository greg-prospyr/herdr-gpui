//! How an avatar stands at a moment in time, and when that next changes.
//!
//! Status comes from the daemon's snapshot; this only remembers when this
//! client saw it change, to play a short smooth "moment": a hop when an agent
//! finishes, a shake when it blocks, a nudge when it starts work. After that
//! each status loops at a pixel-art cadence on a clock shared by every
//! avatar, so the working ones step together and share their redraws.

use super::style::seed;
use herdr_client::protocol::AgentStatus;
use std::{
    f32::consts::PI,
    time::{Duration, Instant},
};

/// A finished agent's hop, then the squash as it lands.
const HOP: Duration = Duration::from_millis(560);
const LAND: Duration = Duration::from_millis(200);
const SHAKE: Duration = Duration::from_millis(900);
const NUDGE: Duration = Duration::from_millis(420);
/// A working agent's reading glance steps at 8 frames a second.
const WORK_STEP: u128 = 125;
/// Blocked and done agents pulse their mark twice a second.
const PULSE_STEP: u128 = 500;
const BLINK: u128 = 150;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Eyes {
    /// Looking left of centre when negative, right when positive, in pixels.
    Open {
        look: i8,
    },
    Closed,
}

/// A mark drawn beside the head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mark {
    /// Waiting on you: an exclamation point.
    Alert,
    /// Finished: a twinkle, in one of two places.
    Sparkle(u8),
}

/// Offsets are in sprite pixels and may be fractional: moments move smoothly,
/// loops move whole pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Pose {
    pub dx: f32,
    pub dy: f32,
    /// Height over width about the feet: below 1 squashes, above 1 stretches.
    pub stretch: f32,
    pub eyes: Eyes,
    pub mark: Option<Mark>,
    /// An agent whose status is unknown is drawn faded.
    pub faded: bool,
}

impl Pose {
    /// Standing still, eyes open: how pickers show a style.
    pub(crate) fn still() -> Self {
        Self {
            dx: 0.,
            dy: 0.,
            stretch: 1.,
            eyes: Eyes::Open { look: 0 },
            mark: None,
            faded: false,
        }
    }
}

/// When the pose next changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wake {
    /// Moving smoothly: draw the next display frame.
    Frame,
    After(Duration),
    Never,
}

impl Wake {
    /// The sooner of two wakes.
    pub(crate) fn min(self, other: Self) -> Self {
        match (self, other) {
            (Self::Frame, _) | (_, Self::Frame) => Self::Frame,
            (Self::Never, wake) | (wake, Self::Never) => wake,
            (Self::After(a), Self::After(b)) => Self::After(a.min(b)),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Motion {
    status: AgentStatus,
    /// When this client saw the status change; `None` for the status an
    /// agent already had when first seen, which plays no moment.
    changed: Option<Instant>,
    seed: u32,
}

impl Motion {
    pub(crate) fn new(key: &str, status: AgentStatus) -> Self {
        Self {
            status,
            changed: None,
            seed: seed(key),
        }
    }

    /// Records the daemon's latest status for this agent.
    pub(crate) fn observe(&mut self, status: AgentStatus, now: Instant) {
        if status != self.status {
            self.status = status;
            self.changed = Some(now);
        }
    }

    fn moment(&self, now: Instant) -> Option<f32> {
        let length = match self.status {
            AgentStatus::Done => HOP + LAND,
            AgentStatus::Blocked => SHAKE,
            AgentStatus::Working => NUDGE,
            AgentStatus::Idle | AgentStatus::Unknown => return None,
        };
        let elapsed = now.saturating_duration_since(self.changed?);
        (elapsed < length).then_some(elapsed.as_secs_f32())
    }

    fn blink_period(&self) -> u128 {
        4_000 + u128::from(self.seed % 12) * 250
    }

    /// The pose at `now`, with loops timed from the shared `clock` start.
    pub(crate) fn pose(&self, now: Instant, clock: Instant) -> Pose {
        let tick = now.saturating_duration_since(clock).as_millis();
        let mut pose = Pose::still();
        match self.status {
            AgentStatus::Working => {
                let step = tick / WORK_STEP % 4;
                pose.eyes = Eyes::Open {
                    look: [-1, 0, 1, 0][step as usize],
                };
                pose.dy = if step % 2 == 1 { -1. } else { 0. };
            }
            AgentStatus::Blocked => {
                pose.mark = (tick / PULSE_STEP).is_multiple_of(2).then_some(Mark::Alert);
            }
            AgentStatus::Done => {
                pose.mark = Some(Mark::Sparkle((tick / PULSE_STEP % 2) as u8));
            }
            AgentStatus::Idle => {
                let period = self.blink_period();
                if tick % period >= period - BLINK {
                    pose.eyes = Eyes::Closed;
                }
            }
            AgentStatus::Unknown => pose.faded = true,
        }
        let Some(t) = self.moment(now) else {
            return pose;
        };
        match self.status {
            AgentStatus::Done => {
                let hop = HOP.as_secs_f32();
                if t < hop {
                    pose.dy = -4. * (PI * t / hop).sin();
                    pose.stretch = 1. + 0.12 * (PI * t / hop).sin();
                } else {
                    pose.dy = 0.;
                    pose.stretch = 1. - 0.2 * (PI * (t - hop) / LAND.as_secs_f32()).sin();
                }
            }
            AgentStatus::Blocked => {
                let k = t / SHAKE.as_secs_f32();
                pose.dx = (k * 2. * PI * 6.).sin() * 1.4 * (1. - k);
            }
            AgentStatus::Working => {
                pose.dy = -1.6 * (PI * t / NUDGE.as_secs_f32()).sin();
            }
            AgentStatus::Idle | AgentStatus::Unknown => {}
        }
        pose
    }

    /// When `pose` next returns something different.
    pub(crate) fn wake(&self, now: Instant, clock: Instant) -> Wake {
        if self.moment(now).is_some() {
            return Wake::Frame;
        }
        let tick = now.saturating_duration_since(clock).as_millis();
        let until = |step: u128| step - tick % step;
        let millis = match self.status {
            AgentStatus::Working => until(WORK_STEP),
            AgentStatus::Blocked | AgentStatus::Done => until(PULSE_STEP),
            AgentStatus::Idle => {
                let period = self.blink_period();
                let phase = tick % period;
                if phase >= period - BLINK {
                    period - phase
                } else {
                    period - BLINK - phase
                }
            }
            AgentStatus::Unknown => return Wake::Never,
        };
        Wake::After(Duration::from_millis(millis.max(1) as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(clock: Instant, millis: u64) -> Instant {
        clock + Duration::from_millis(millis)
    }

    #[test]
    fn the_status_an_agent_starts_with_plays_no_moment() {
        let clock = Instant::now();
        for status in [
            AgentStatus::Done,
            AgentStatus::Blocked,
            AgentStatus::Working,
        ] {
            let motion = Motion::new("p", status);
            assert_ne!(motion.wake(clock, clock), Wake::Frame, "{status:?}");
        }
    }

    #[test]
    fn finishing_hops_smoothly_then_lands_and_settles() {
        let clock = Instant::now();
        let mut motion = Motion::new("p", AgentStatus::Working);
        motion.observe(AgentStatus::Done, clock);
        assert_eq!(motion.wake(clock, clock), Wake::Frame);
        let apex = motion.pose(at(clock, 280), clock);
        assert!(apex.dy < -3.9, "{apex:?}");
        let landing = motion.pose(at(clock, 660), clock);
        assert_eq!(landing.dy, 0.);
        assert!(landing.stretch < 0.85, "{landing:?}");
        let settled = at(clock, 800);
        assert!(matches!(motion.wake(settled, clock), Wake::After(_)));
        let rest = motion.pose(settled, clock);
        assert_eq!((rest.dy, rest.stretch), (0., 1.));
        assert!(matches!(rest.mark, Some(Mark::Sparkle(_))));
    }

    #[test]
    fn blocking_shakes_and_then_pulses_an_alert() {
        let clock = Instant::now();
        let mut motion = Motion::new("p", AgentStatus::Working);
        motion.observe(AgentStatus::Blocked, clock);
        let shaking = (1..9).map(|i| motion.pose(at(clock, i * 20), clock).dx.abs());
        assert!(shaking.fold(0f32, f32::max) > 0.8);
        let after = at(clock, 1_000);
        assert_eq!(motion.pose(after, clock).dx, 0.);
        let marks: Vec<_> = [1_000, 1_500]
            .map(|millis| motion.pose(at(clock, millis), clock).mark)
            .into();
        assert!(marks.contains(&Some(Mark::Alert)) && marks.contains(&None));
        assert_eq!(
            motion.wake(at(clock, 1_100), clock),
            Wake::After(Duration::from_millis(400))
        );
    }

    #[test]
    fn working_steps_its_glance_on_the_shared_clock() {
        let clock = Instant::now();
        let a = Motion::new("a", AgentStatus::Working);
        let b = Motion::new("b", AgentStatus::Working);
        let looks: Vec<_> = (0..4)
            .map(|step| a.pose(at(clock, step * 125 + 10), clock).eyes)
            .collect();
        assert_eq!(
            looks,
            [-1, 0, 1, 0].map(|look| Eyes::Open { look }).to_vec()
        );
        let now = at(clock, 1_010);
        assert_eq!(a.pose(now, clock), b.pose(now, clock));
        assert_eq!(a.wake(now, clock), Wake::After(Duration::from_millis(115)));
        assert_eq!(a.pose(now, clock).dy.fract(), 0.);
    }

    #[test]
    fn idle_avatars_blink_briefly_at_their_own_pace() {
        let clock = Instant::now();
        let motion = Motion::new("idle", AgentStatus::Idle);
        let period = motion.blink_period() as u64;
        assert_eq!(motion.pose(clock, clock).eyes, Eyes::Open { look: 0 });
        assert_eq!(
            motion.wake(clock, clock),
            Wake::After(Duration::from_millis(period - 150))
        );
        let closed = at(clock, period - 100);
        assert_eq!(motion.pose(closed, clock).eyes, Eyes::Closed);
        assert_eq!(
            motion.wake(closed, clock),
            Wake::After(Duration::from_millis(100))
        );
        let periods: std::collections::HashSet<_> = (0..20)
            .map(|i| Motion::new(&format!("p{i}"), AgentStatus::Idle).blink_period())
            .collect();
        assert!(periods.len() > 4);
    }

    #[test]
    fn unknown_status_rests_faded_and_never_wakes() {
        let clock = Instant::now();
        let motion = Motion::new("p", AgentStatus::Unknown);
        assert!(motion.pose(clock, clock).faded);
        assert_eq!(motion.wake(clock, clock), Wake::Never);
    }

    #[test]
    fn the_soonest_wake_wins() {
        let (a, b) = (
            Wake::After(Duration::from_millis(5)),
            Wake::After(Duration::from_millis(9)),
        );
        assert_eq!(a.min(b), a);
        assert_eq!(Wake::Never.min(b), b);
        assert_eq!(b.min(Wake::Frame), Wake::Frame);
        assert_eq!(Wake::Never.min(Wake::Never), Wake::Never);
    }
}
