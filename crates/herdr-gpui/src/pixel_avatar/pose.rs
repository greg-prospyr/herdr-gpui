//! How a buddy stands at a moment in time, and when that next changes.
//!
//! Status comes from the daemon's snapshot; this only remembers when this
//! client saw it change, to play a short smooth "moment": a shake when a
//! space blocks, a hop when it finishes. After that each status loops at a
//! pixel-art cadence on a clock shared by every buddy, so they share
//! redraws: bouncing at work, trembling when it needs you, twinkling when
//! done, and a blink now and then at rest.

use super::buddy::Look;
use herdr_client::protocol::AgentStatus;
use std::time::{Duration, Instant};

const SHAKE: Duration = Duration::from_millis(900);
const HOP: Duration = Duration::from_millis(700);
/// A working buddy bounces at eight frames a second.
const BOUNCE: u128 = 125;
const TREMBLE: u128 = 250;
const TWINKLE: u128 = 600;
/// Blinks come every few seconds, in one of three slots so that neighbours
/// do not all blink at once, yet redraws stay shared.
const BLINK_PERIOD: u128 = 5_100;
const BLINK: u128 = 150;

/// Offsets are in buddy pixels and may be fractional during a moment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Pose {
    pub look: Look,
    pub dx: f32,
    pub dy: f32,
}

impl Pose {
    /// Smiling at rest: how pickers show a style.
    pub(crate) fn still() -> Self {
        Self {
            look: Look::Idle { blink: false },
            dx: 0.,
            dy: 0.,
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
    /// Which blink slot this buddy uses.
    slot: u8,
}

impl Motion {
    pub(crate) fn new(key: &str, status: AgentStatus) -> Self {
        let slot = key.bytes().fold(0u32, |hash, byte| {
            hash.wrapping_mul(31).wrapping_add(u32::from(byte))
        }) % 3;
        Self {
            status,
            changed: None,
            slot: slot as u8,
        }
    }

    fn blink_phase(&self, tick: u128) -> u128 {
        (tick + u128::from(self.slot) * BLINK_PERIOD / 3) % BLINK_PERIOD
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
            AgentStatus::Done => HOP,
            _ => return None,
        };
        let elapsed = self.since_change(now)?;
        (elapsed < length).then_some(elapsed.as_secs_f32() / length.as_secs_f32())
    }

    /// The pose at `now`, with loops timed from the shared `clock` start.
    pub(crate) fn pose(&self, now: Instant, clock: Instant) -> Pose {
        let tick = now.saturating_duration_since(clock).as_millis();
        let look = match self.status {
            AgentStatus::Working => Look::Working {
                frame: (tick / BOUNCE % 4) as u8,
            },
            AgentStatus::Blocked => Look::Blocked {
                tremble: !(tick / TREMBLE).is_multiple_of(2),
            },
            AgentStatus::Done => Look::Done {
                twinkle: !(tick / TWINKLE).is_multiple_of(2),
            },
            AgentStatus::Idle | AgentStatus::Unknown => Look::Idle {
                blink: self.blink_phase(tick) >= BLINK_PERIOD - BLINK,
            },
        };
        let moment = self.moment(now);
        let (dx, dy) = match (self.status, moment) {
            (AgentStatus::Blocked, Some(k)) => {
                ((k * std::f32::consts::TAU * 6.).sin() * 1.6 * (1. - k), 0.)
            }
            (AgentStatus::Done, Some(k)) => (0., -5. * (std::f32::consts::PI * k).sin()),
            _ => (0., 0.),
        };
        Pose { look, dx, dy }
    }

    /// When `pose` next returns something different.
    pub(crate) fn wake(&self, now: Instant, clock: Instant) -> Wake {
        if self.moment(now).is_some() {
            return Wake::Frame;
        }
        let tick = now.saturating_duration_since(clock).as_millis();
        let until = |step: u128| step - tick % step;
        let millis = match self.status {
            AgentStatus::Working => until(BOUNCE),
            AgentStatus::Blocked => until(TREMBLE),
            AgentStatus::Done => until(TWINKLE),
            AgentStatus::Idle | AgentStatus::Unknown => {
                let phase = self.blink_phase(tick);
                if phase >= BLINK_PERIOD - BLINK {
                    BLINK_PERIOD - phase
                } else {
                    BLINK_PERIOD - BLINK - phase
                }
            }
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
    fn working_bounces_through_four_frames_on_the_shared_clock() {
        let clock = Instant::now();
        let a = Motion::new("a", AgentStatus::Working);
        let frames: Vec<_> = (0..4)
            .map(|i| a.pose(at(clock, i * 125 + 5), clock).look)
            .collect();
        assert_eq!(
            frames,
            (0..4)
                .map(|frame| Look::Working { frame })
                .collect::<Vec<_>>()
        );
        let b = Motion::new("b", AgentStatus::Working);
        assert_eq!(a.pose(at(clock, 600), clock), b.pose(at(clock, 600), clock));
        assert_eq!(
            a.wake(at(clock, 130), clock),
            Wake::After(Duration::from_millis(120))
        );
    }

    #[test]
    fn idle_buddies_blink_briefly_in_staggered_slots() {
        let clock = Instant::now();
        let slots: std::collections::HashSet<_> = (0..30)
            .map(|i| Motion::new(&format!("w{i}"), AgentStatus::Idle).slot)
            .collect();
        assert_eq!(slots.len(), 3);
        let motion = Motion::new("w1", AgentStatus::Idle);
        let blinking = (0..BLINK_PERIOD as u64)
            .step_by(10)
            .filter(|&t| motion.pose(at(clock, t), clock).look == Look::Idle { blink: true })
            .count();
        assert!((14..=16).contains(&blinking), "{blinking}");
        assert!(matches!(motion.wake(clock, clock), Wake::After(_)));
    }

    #[test]
    fn blocking_shakes_then_trembles_and_finishing_hops_then_twinkles() {
        let clock = Instant::now();
        let mut motion = Motion::new("p", AgentStatus::Working);
        motion.observe(AgentStatus::Blocked, clock);
        assert_eq!(motion.wake(clock, clock), Wake::Frame);
        let shaking = (1..9).map(|i| motion.pose(at(clock, i * 20), clock).dx.abs());
        assert!(shaking.fold(0f32, f32::max) > 0.8);
        let after = at(clock, 1_000);
        assert_eq!(motion.pose(after, clock).dx, 0.);
        assert_eq!(
            motion.wake(after, clock),
            Wake::After(Duration::from_millis(250))
        );
        motion.observe(AgentStatus::Done, clock);
        let apex = motion.pose(at(clock, 350), clock);
        assert!(apex.dy < -4.9, "{apex:?}");
        let settled = motion.pose(at(clock, 800), clock);
        assert_eq!(settled.dy, 0.);
        assert!(matches!(settled.look, Look::Done { .. }));
        assert_eq!(
            motion.since_change(at(clock, 60_000)),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn the_first_status_plays_no_moment() {
        let clock = Instant::now();
        for status in [AgentStatus::Blocked, AgentStatus::Done] {
            assert_ne!(Motion::new("p", status).wake(clock, clock), Wake::Frame);
        }
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
