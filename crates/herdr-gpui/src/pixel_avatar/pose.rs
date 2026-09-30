//! How a moon stands at a moment in time, and when that next changes.
//!
//! Status comes from the daemon's snapshot; this only remembers when this
//! client saw it change, to play a short smooth "moment": a shake when a
//! space blocks, a ripple of light when it finishes. After that each status
//! holds a phase: a still crescent at rest, a sweeping terminator at work, a
//! pulsing blood moon when it needs you, a full moon when done. Only work
//! and blocking move, on a clock shared by every moon, so they share redraws.

use super::moon::{self, Light};
use herdr_client::protocol::AgentStatus;
use std::time::{Duration, Instant};

const SHAKE: Duration = Duration::from_millis(900);
const RIPPLE: Duration = Duration::from_millis(900);
/// A blood moon brightens and dims twice a second.
const PULSE: u128 = 500;

/// Offsets are in moon pixels and may be fractional during a moment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Pose {
    pub light: Light,
    pub dx: f32,
    /// How far a finished moon's ripple has spread, 0 to 1, while it plays.
    pub ripple: Option<f32>,
}

impl Pose {
    /// A moon three-quarters lit: how pickers show a style.
    pub(crate) fn still() -> Self {
        Self {
            light: Light::Phase(moon::PHASES / 3),
            dx: 0.,
            ripple: None,
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
    /// When this client saw the status change; `None` for the status a space
    /// already had when first seen, which plays no moment.
    changed: Option<Instant>,
}

impl Motion {
    pub(crate) fn new(status: AgentStatus) -> Self {
        Self {
            status,
            changed: None,
        }
    }

    /// Records the daemon's latest status for this space.
    pub(crate) fn observe(&mut self, status: AgentStatus, now: Instant) {
        if status != self.status {
            self.status = status;
            self.changed = Some(now);
        }
    }

    /// How long ago this client saw the status change, if it did.
    pub(crate) fn since_change(&self, now: Instant) -> Option<Duration> {
        self.changed.map(|at| now.saturating_duration_since(at))
    }

    fn moment(&self, now: Instant) -> Option<f32> {
        let length = match self.status {
            AgentStatus::Blocked => SHAKE,
            AgentStatus::Done => RIPPLE,
            _ => return None,
        };
        let elapsed = self.since_change(now)?;
        (elapsed < length).then_some(elapsed.as_secs_f32() / length.as_secs_f32())
    }

    /// The pose at `now`, with loops timed from the shared `clock` start.
    pub(crate) fn pose(&self, now: Instant, clock: Instant) -> Pose {
        let tick = now.saturating_duration_since(clock).as_millis();
        let light = match self.status {
            AgentStatus::Working => Light::Phase(moon::phase_at(tick).0),
            AgentStatus::Blocked => Light::Blood {
                bright: (tick / PULSE).is_multiple_of(2),
            },
            AgentStatus::Done => Light::Full,
            AgentStatus::Idle | AgentStatus::Unknown => Light::Crescent,
        };
        let moment = self.moment(now);
        Pose {
            light,
            dx: match (self.status, moment) {
                (AgentStatus::Blocked, Some(k)) => {
                    (k * std::f32::consts::TAU * 6.).sin() * 1.6 * (1. - k)
                }
                _ => 0.,
            },
            ripple: moment.filter(|_| self.status == AgentStatus::Done),
        }
    }

    /// When `pose` next returns something different.
    pub(crate) fn wake(&self, now: Instant, clock: Instant) -> Wake {
        if self.moment(now).is_some() {
            return Wake::Frame;
        }
        let tick = now.saturating_duration_since(clock).as_millis();
        let millis = match self.status {
            AgentStatus::Working => moon::phase_at(tick).1,
            AgentStatus::Blocked => PULSE - tick % PULSE,
            // A crescent and a full moon hold still: no redraws at all.
            AgentStatus::Done | AgentStatus::Idle | AgentStatus::Unknown => return Wake::Never,
        };
        Wake::After(Duration::from_millis(millis.max(1) as u64))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn at(clock: Instant, millis: u64) -> Instant {
        clock + Duration::from_millis(millis)
    }

    #[test]
    fn resting_and_finished_moons_hold_still() {
        let clock = Instant::now();
        for (status, light) in [
            (AgentStatus::Idle, Light::Crescent),
            (AgentStatus::Unknown, Light::Crescent),
            (AgentStatus::Done, Light::Full),
        ] {
            let motion = Motion::new(status);
            assert_eq!(motion.pose(at(clock, 1234), clock).light, light);
            assert_eq!(
                motion.wake(at(clock, 1234), clock),
                Wake::Never,
                "{status:?}"
            );
        }
    }

    #[test]
    fn working_sweeps_the_terminator_on_the_shared_clock() {
        let clock = Instant::now();
        let motion = Motion::new(AgentStatus::Working);
        assert_eq!(motion.pose(clock, clock).light, Light::Phase(0));
        assert_eq!(motion.pose(at(clock, 250), clock).light, Light::Phase(1));
        assert_eq!(
            motion.wake(at(clock, 250), clock),
            Wake::After(Duration::from_millis(230))
        );
    }

    #[test]
    fn blocking_shakes_then_the_blood_moon_pulses() {
        let clock = Instant::now();
        let mut motion = Motion::new(AgentStatus::Working);
        motion.observe(AgentStatus::Blocked, clock);
        assert_eq!(motion.wake(clock, clock), Wake::Frame);
        let shaking = (1..9).map(|i| motion.pose(at(clock, i * 20), clock).dx.abs());
        assert!(shaking.fold(0f32, f32::max) > 0.8);
        let after = at(clock, 1_000);
        assert_eq!(motion.pose(after, clock).dx, 0.);
        assert_eq!(
            motion.pose(after, clock).light,
            Light::Blood { bright: true }
        );
        assert_eq!(
            motion.pose(at(clock, 1_500), clock).light,
            Light::Blood { bright: false }
        );
        assert_eq!(
            motion.wake(after, clock),
            Wake::After(Duration::from_millis(500))
        );
    }

    #[test]
    fn finishing_ripples_once_and_the_first_status_plays_nothing() {
        let clock = Instant::now();
        let mut motion = Motion::new(AgentStatus::Working);
        motion.observe(AgentStatus::Done, clock);
        let ripple = motion.pose(at(clock, 450), clock).ripple.unwrap();
        assert!(ripple > 0.4 && ripple < 0.6, "{ripple}");
        assert_eq!(motion.pose(at(clock, 950), clock).ripple, None);
        assert!(
            Motion::new(AgentStatus::Done)
                .pose(clock, clock)
                .ripple
                .is_none()
        );
        assert_eq!(
            motion.since_change(at(clock, 60_000)),
            Some(Duration::from_secs(60))
        );
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
    }
}
