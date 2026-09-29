//! Pixel-art agent avatars: a shape and a hue each agent keeps, rasterized
//! once into small sprites, posed from the daemon's status and when this
//! client saw it change.
//!
//! Poses are pure functions of time, so a frame never depends on the one
//! before it. Each pose also says when it next changes, which is how the
//! window draws avatars only while something moves: smoothly for a moment
//! after a state change, at a pixel-art cadence while an agent works, for a
//! blink now and then when idle, and not at all when every avatar is still.

mod paint;
mod pose;
mod sprite;
mod store;
mod style;

pub(crate) use paint::{GRID, paint, paint_still};
pub(crate) use pose::{Motion, Wake};
pub(crate) use store::AvatarStore;
pub(crate) use style::{AvatarStyle, Hue, Shape};
