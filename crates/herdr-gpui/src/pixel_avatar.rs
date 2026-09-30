//! Pixel-art moons for spaces: a crater pattern and a tint each space keeps,
//! lit by a phase that follows the daemon's status and when this client saw
//! it change.
//!
//! Poses are pure functions of time, so a frame never depends on the one
//! before it. Each pose also says when it next changes, which is how the
//! window draws moons only while something moves: smoothly for a moment
//! after a state change, a few times a second while a space works or waits
//! on you, and not at all while every moon rests or is full.

mod moon;
mod paint;
mod pose;
mod store;
mod style;

pub(crate) use paint::{GRID, Images, paint, paint_still};
pub(crate) use pose::{Motion, Wake};
pub(crate) use store::AvatarStore;
pub(crate) use style::{AvatarStyle, Hue, Shape};
