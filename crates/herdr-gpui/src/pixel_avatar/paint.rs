//! Painting a posed moon. Each pose is rendered once per scale into an image
//! the window's sprite atlas keeps, so a frame paints a few cached images
//! rather than a quad per pixel. The cache is bounded, and images it drops
//! are released from the atlas.

use super::{
    moon::{self, Light},
    pose::Pose,
    style::AvatarStyle,
};
use gpui::{Bounds, Corners, Hsla, Pixels, RenderImage, Window, point, px, quad, rgb, size};
use image::Frame;
use std::{collections::HashMap, sync::Arc};

/// Rows size avatars in whole multiples of this many points.
pub(crate) const GRID: usize = 16;

/// Enough for every pose of a few dozen visible spaces at one scale.
const MAX_IMAGES: usize = 1024;

const RIPPLE: u32 = 0xfff4d6;

#[derive(Default)]
pub(crate) struct Images {
    images: HashMap<(AvatarStyle, Light, u32), Arc<RenderImage>>,
}

impl Images {
    fn get(&mut self, window: &mut Window, style: AvatarStyle, light: Light) -> Arc<RenderImage> {
        let scale = window.scale_factor().round().clamp(1., 4.) as u32;
        if self.images.len() >= MAX_IMAGES && !self.images.contains_key(&(style, light, scale)) {
            // Rare and cheap to rebuild: every pose re-renders on its next paint.
            for (_, image) in self.images.drain() {
                let _ = window.drop_image(image);
            }
        }
        self.images
            .entry((style, light, scale))
            .or_insert_with(|| {
                let mut pixels = moon::render(style, light, scale);
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
    let cell = side / moon::SIZE as f32;
    let scale = window.scale_factor();
    let snap = |value: Pixels| px((f32::from(value) * scale).round() / scale);
    let origin = point(
        snap(bounds.origin.x + cell * pose.dx),
        snap(bounds.origin.y),
    );
    let square = Bounds::new(origin, size(side, side));
    let image = images.get(window, style, pose.light);
    let _ = window.paint_image(square, square, Corners::default(), image, 0, false);
    if let Some(k) = pose.ripple {
        // A ring of light spreading from the limb as it fades.
        let radius = cell * (12. + k * 11.);
        let centre = square.center();
        let ring = Bounds::new(
            point(centre.x - radius, centre.y - radius),
            size(radius * 2., radius * 2.),
        );
        window.paint_quad(quad(
            ring,
            radius,
            Hsla::transparent_black(),
            px(1.5),
            Hsla::from(rgb(RIPPLE)).opacity(1. - k),
            Default::default(),
        ));
    }
}

/// Paints `style` as a picker shows it: three-quarters lit and still.
pub(crate) fn paint_still(
    window: &mut Window,
    images: &mut Images,
    bounds: Bounds<Pixels>,
    style: AvatarStyle,
) {
    paint(window, images, bounds, style, &Pose::still());
}
