//! Painting a posed buddy. Each look is rendered once per scale into an image
//! the window's sprite atlas keeps, so a frame paints a few cached images
//! rather than a quad per pixel. The cache is bounded, and images it drops
//! are released from the atlas.

use super::{
    buddy::{self, Look},
    pose::Pose,
    style::AvatarStyle,
};
use gpui::{Bounds, Corners, Pixels, RenderImage, Window, point, px, size};
use image::Frame;
use std::{collections::HashMap, sync::Arc};

/// Rows size avatars in whole multiples of this many points.
pub(crate) const GRID: usize = 16;

/// Enough for every look of a few dozen visible spaces at one scale.
const MAX_IMAGES: usize = 1024;

#[derive(Default)]
pub(crate) struct Images {
    images: HashMap<(AvatarStyle, Look, u32), Arc<RenderImage>>,
}

impl Images {
    fn get(&mut self, window: &mut Window, style: AvatarStyle, look: Look) -> Arc<RenderImage> {
        let scale = window.scale_factor().round().clamp(1., 4.) as u32;
        if self.images.len() >= MAX_IMAGES && !self.images.contains_key(&(style, look, scale)) {
            // Rare and cheap to rebuild: every look re-renders on its next paint.
            for (_, image) in self.images.drain() {
                let _ = window.drop_image(image);
            }
        }
        self.images
            .entry((style, look, scale))
            .or_insert_with(|| {
                let mut pixels = buddy::render(style, look, scale);
                // GPUI's images are BGRA.
                for pixel in pixels.pixels_mut() {
                    pixel.0.swap(0, 2);
                }
                Arc::new(RenderImage::new(vec![Frame::new(pixels)]))
            })
            .clone()
    }
}

/// Paints `style` in the square at the top of `bounds`, posed.
pub(crate) fn paint(
    window: &mut Window,
    images: &mut Images,
    bounds: Bounds<Pixels>,
    style: AvatarStyle,
    pose: &Pose,
) {
    let side = bounds.size.width.min(bounds.size.height);
    let cell = side / buddy::SIZE as f32;
    let scale = window.scale_factor();
    let snap = |value: Pixels| px((f32::from(value) * scale).round() / scale);
    let origin = point(
        snap(bounds.origin.x + cell * pose.dx),
        snap(bounds.origin.y + cell * pose.dy),
    );
    let square = Bounds::new(origin, size(side, side));
    let image = images.get(window, style, pose.look);
    let _ = window.paint_image(square, square, Corners::default(), image, 0, false);
}

/// Paints `style` as a picker shows it: smiling and still.
pub(crate) fn paint_still(
    window: &mut Window,
    images: &mut Images,
    bounds: Bounds<Pixels>,
    style: AvatarStyle,
) {
    paint(window, images, bounds, style, &Pose::still());
}
