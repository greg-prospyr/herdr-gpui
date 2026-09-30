//! Pixel-art buddies for spaces: a shape and a colour each space keeps, lit
//! as a solid body, whose face and motion follow the daemon's status and
//! when this client saw it change.
//!
//! Poses are pure functions of time, so a frame never depends on the one
//! before it. Each pose also says when it next changes, which is how the
//! window draws buddies only while something moves: smoothly for a moment
//! after a state change, at a pixel-art cadence while a space works, waits
//! on you, or is done, and for a blink now and then at rest.

mod buddy;
mod paint;
mod pose;
mod store;
mod style;

pub(crate) use paint::{GRID, Images, paint, paint_still};
pub(crate) use pose::{Motion, Wake};
pub(crate) use store::AvatarStore;
pub(crate) use style::{AvatarStyle, Hue, Shape};
