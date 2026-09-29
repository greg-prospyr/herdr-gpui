//! Painting a posed sprite as quads. A row of one tone is one quad, so an
//! avatar costs a few dozen quads a frame, and corners snap to device pixels
//! so the pixel art stays crisp between whole-pixel poses.

use super::{
    pose::{Eyes, Mark, Pose},
    sprite::{Tone, sprite},
    style::AvatarStyle,
};
use gpui::{Bounds, Hsla, Pixels, Window, fill, point, px, rgb};

/// Sprites are this many pixels on a side.
pub(crate) const GRID: usize = 16;

const EYE: u32 = 0x1b1b1f;
const ALERT: u32 = 0xf38ba8;
const SPARKLE: u32 = 0xf9e2af;

/// Paints `style` standing still, as a picker shows it.
pub(crate) fn paint_still(window: &mut Window, bounds: Bounds<Pixels>, style: AvatarStyle) {
    paint(window, bounds, style, &Pose::still());
}

/// Paints `style` in the square at the top of `bounds`, posed.
pub(crate) fn paint(window: &mut Window, bounds: Bounds<Pixels>, style: AvatarStyle, pose: &Pose) {
    let sprite = sprite(style.shape);
    let [body, light, shade] = style.hue.tones();
    let side = f32::from(bounds.size.width.min(bounds.size.height));
    let cell = side / GRID as f32;
    let scale = window.scale_factor();
    let snap = |value: f32| (value * scale).round() / scale;
    let alpha = if pose.faded { 0.45 } else { 1. };
    let left = f32::from(bounds.origin.x) + pose.dx * cell;
    let top = f32::from(bounds.origin.y) + pose.dy * cell;
    // Squash and stretch about the feet and the vertical centre line, keeping
    // the body's area roughly constant.
    let (sx, sy) = (1. / pose.stretch.max(0.5), pose.stretch);
    let centre = left + side / 2.;
    let feet = top + side;
    let mut rect = |x: f32, y: f32, width: f32, color: u32, posed: bool| {
        let (x0, y0, w, h) = if posed {
            (
                centre + (x - GRID as f32 / 2.) * cell * sx,
                feet - (GRID as f32 - y) * cell * sy,
                width * cell * sx,
                cell * sy,
            )
        } else {
            (left + x * cell, top + y * cell, width * cell, cell)
        };
        let (x0, y0) = (snap(x0), snap(y0));
        let (x1, y1) = (snap(x0 + w), snap(y0 + h));
        if x1 > x0 && y1 > y0 {
            window.paint_quad(fill(
                Bounds::from_corners(point(px(x0), px(y0)), point(px(x1), px(y1))),
                Hsla::from(rgb(color)).opacity(alpha),
            ));
        }
    };
    for y in 0..GRID {
        for (start, length, tone) in sprite.runs(y) {
            let color = match tone {
                Tone::Body => body,
                Tone::Light => light,
                Tone::Shade => shade,
            };
            rect(start as f32, y as f32, length as f32, color, true);
        }
    }
    for (x, y) in sprite.eyes {
        let (x, y) = (f32::from(x), f32::from(y));
        match pose.eyes {
            Eyes::Open { look } => {
                rect(x + f32::from(look), y, 1., EYE, true);
                rect(x + f32::from(look), y + 1., 1., EYE, true);
            }
            Eyes::Closed => rect(x, y + 1., 1., EYE, true),
        }
    }
    // Marks stay upright beside the head rather than squashing with it.
    match pose.mark {
        Some(Mark::Alert) => {
            for y in [0., 1., 2., 4.] {
                rect(14., y, 1., ALERT, false);
            }
        }
        Some(Mark::Sparkle(phase)) => {
            let (x, y) = if phase == 0 { (13., 2.) } else { (2., 3.) };
            rect(x, y - 1., 1., SPARKLE, false);
            rect(x - 1., y, 3., SPARKLE, false);
            rect(x, y + 1., 1., SPARKLE, false);
        }
        None => {}
    }
}
