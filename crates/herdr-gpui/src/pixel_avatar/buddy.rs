//! Buddies: a friendly shape lit as a solid body, with a face whose
//! expression is the space's status, rendered to a small image with
//! quantized, dithered light for a pixel-art finish.
//!
//! The body is a mask made into a dome: each pixel's height comes from its
//! distance to the edge, and the height's slope is the surface normal the
//! light falls on. Shadows lean cool and highlights warm, a glossy spot and a
//! rim light sell the volume, and a dark outline keeps it legible at 32
//! points.

use super::style::{AvatarStyle, Shape};
use image::{Rgba, RgbaImage};

/// Buddies are this many pixels on a side, one per point at sidebar size.
pub(crate) const SIZE: usize = 32;
const CX: f32 = 16.;
/// The ground line: squash and stretch keep the feet here.
const FEET: f32 = 28.5;
const BAYER: [[f32; 4]; 4] = [
    [0., 8., 2., 10.],
    [12., 4., 14., 6.],
    [3., 11., 1., 9.],
    [15., 7., 13., 5.],
];

/// What a buddy's face and body are doing, which is all that differs
/// between the images of one style.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Look {
    /// At rest, smiling; eyes shut for a blink.
    Idle { blink: bool },
    /// Bouncing through four frames of squash and stretch, eyes darting.
    Working { frame: u8 },
    /// Worried, with a "!" bubble; trembling between two frames.
    Blocked { tremble: bool },
    /// Delighted, with stars that twinkle between two places.
    Done { twinkle: bool },
}

impl Look {
    /// Height over width for the body's squash and stretch.
    fn stretch(self) -> f32 {
        match self {
            Self::Working { frame } => [1.08, 1., 0.9, 1.][usize::from(frame % 4)],
            Self::Done { .. } => 1.03,
            _ => 1.,
        }
    }

    /// Pupils' offset from centre, in pixels.
    fn gaze(self) -> i32 {
        match self {
            Self::Working { frame } => [-1, 0, 1, 0][usize::from(frame % 4)],
            _ => 0,
        }
    }
}

type Color = [f32; 3];

fn rgb(hex: u32) -> Color {
    [
        (hex >> 16) as f32,
        ((hex >> 8) & 0xff) as f32,
        (hex & 0xff) as f32,
    ]
}

fn mix(a: Color, b: Color, t: f32) -> Color {
    [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t)
}

/// The body's ramp, darkest first: shadows lean toward deep blue, light
/// toward warm white, so the colour stays rich in both.
fn ramp(base: Color) -> [Color; 5] {
    let cool = rgb(0x1d1a4a);
    let warm = rgb(0xfff4dc);
    [
        mix(base, cool, 0.62),
        mix(base, cool, 0.32),
        base,
        mix(base, warm, 0.3),
        mix(base, warm, 0.62),
    ]
}

/// Whether the point (`x`, `y`) of an unsquashed body lies inside `shape`.
fn inside(shape: Shape, x: f32, y: f32) -> bool {
    let circle = |cx: f32, cy: f32, r: f32| (x - cx).powi(2) + (y - cy).powi(2) <= r * r;
    let rounded_box = |cx: f32, cy: f32, hw: f32, hh: f32, r: f32| {
        let (dx, dy) = (
            ((x - cx).abs() - hw + r).max(0.),
            ((y - cy).abs() - hh + r).max(0.),
        );
        (x - cx).abs() <= hw && (y - cy).abs() <= hh && dx * dx + dy * dy <= r * r
    };
    match shape {
        Shape::Blob => ((x - CX) / 11.8).powi(2) + ((y - 18.5) / 10.).powi(2) <= 1.,
        Shape::Drop => {
            circle(CX, 20.4, 8.4)
                || ((6. ..=20.4).contains(&y)
                    && (x - CX).abs() <= 8.4 * ((y - 6.) / 14.4).powf(0.8))
        }
        Shape::Peak => {
            let half = 12.5 * (y - 7.) / 21.5;
            (7. ..=28.5).contains(&y)
                && (x - CX).abs() <= half
                && !(y > 25.5 && (x - CX).abs() > half - 2.4 + (28.5 - y) * 0.2)
        }
        Shape::Gem => {
            let (dx, dy) = ((x - CX).abs(), (y - 18.).abs());
            dx <= 10.4 && dy + dx * 0.577 <= 11.2
        }
        Shape::Cloud => {
            circle(10., 20.5, 6.8)
                || circle(22., 20.5, 6.8)
                || circle(CX, 15., 7.6)
                || rounded_box(CX, 23.5, 11.5, 5., 3.)
        }
        Shape::Block => rounded_box(CX, 18.5, 11., 10., 4.5),
    }
}

/// Where the face sits: pointed bodies carry it lower, toward their bulk.
fn face_row(shape: Shape) -> f32 {
    match shape {
        Shape::Drop | Shape::Peak => 19.,
        Shape::Cloud => 18.,
        _ => 17.,
    }
}

/// The body's mask after squash and stretch about the feet.
fn mask(shape: Shape, stretch: f32) -> Vec<bool> {
    (0..SIZE * SIZE)
        .map(|index| {
            let (x, y) = ((index % SIZE) as f32 + 0.5, (index / SIZE) as f32 + 0.5);
            let (sx, sy) = (CX + (x - CX) * stretch, FEET - (FEET - y) / stretch);
            inside(shape, sx, sy)
        })
        .collect()
}

/// A neighbour's offset and the cost of stepping to it.
type Step = (isize, isize, f32);

/// Each inside pixel's distance to the nearest outside one, by two chamfer
/// passes: cheap, and smooth enough to shade a pixel-art dome.
fn distances(mask: &[bool]) -> Vec<f32> {
    const FAR: f32 = 1e6;
    let mut d: Vec<f32> = mask.iter().map(|&m| if m { FAR } else { 0. }).collect();
    let at = |x: isize, y: isize, d: &[f32]| {
        if x < 0 || y < 0 || x >= SIZE as isize || y >= SIZE as isize {
            0.
        } else {
            d[y as usize * SIZE + x as usize]
        }
    };
    let passes: [(&[Step], bool); 2] = [
        (
            &[(-1, 0, 1.), (0, -1, 1.), (-1, -1, 1.414), (1, -1, 1.414)],
            true,
        ),
        (
            &[(1, 0, 1.), (0, 1, 1.), (1, 1, 1.414), (-1, 1, 1.414)],
            false,
        ),
    ];
    for (steps, forward) in passes {
        for i in 0..SIZE * SIZE {
            let i = if forward { i } else { SIZE * SIZE - 1 - i };
            if !mask[i] {
                continue;
            }
            let (x, y) = ((i % SIZE) as isize, (i / SIZE) as isize);
            for &(dx, dy, cost) in steps {
                d[i] = d[i].min(at(x + dx, y + dy, &d) + cost);
            }
        }
    }
    d
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
    v.map(|c| c / length)
}

/// A buddy image: `SIZE` pixels on a side, straight-alpha RGBA.
struct Canvas {
    pixels: Vec<Option<[f32; 4]>>,
}

impl Canvas {
    fn put(&mut self, x: i32, y: i32, color: Color) {
        self.blend(x, y, color, 1.);
    }

    fn blend(&mut self, x: i32, y: i32, color: Color, alpha: f32) {
        if x < 0 || y < 0 || x >= SIZE as i32 || y >= SIZE as i32 {
            return;
        }
        let slot = &mut self.pixels[y as usize * SIZE + x as usize];
        *slot = Some(match *slot {
            Some([r, g, b, a]) => {
                let out = alpha + a * (1. - alpha);
                let c = mix([r, g, b], color, alpha / out.max(1e-6));
                [c[0], c[1], c[2], out]
            }
            None => [color[0], color[1], color[2], alpha],
        });
    }
}

/// The buddy for `style` doing `look`, `scale` device pixels per buddy
/// pixel.
pub(crate) fn render(style: AvatarStyle, look: Look, scale: u32) -> RgbaImage {
    let scale = scale.clamp(1, 4);
    let stretch = look.stretch();
    let mask = mask(style.shape, stretch);
    let dist = distances(&mask);
    let depth = dist.iter().cloned().fold(0., f32::max).max(1.) * 0.85;
    let height: Vec<f32> = dist
        .iter()
        .map(|&d| {
            let t = (d / depth).min(1.);
            (1. - (1. - t) * (1. - t)).sqrt()
        })
        .collect();
    let colors = ramp(rgb(style.hue.base()));
    let outline = mix(colors[0], rgb(0x0b0a1a), 0.55);
    let sun = normalize([-0.55, -0.7, 0.55]);
    let half = normalize([sun[0], sun[1], sun[2] + 1.]);
    let mut canvas = Canvas {
        pixels: vec![None; SIZE * SIZE],
    };
    // A soft contact shadow, drawn first so the body stands on it.
    for y in 26..SIZE as i32 {
        for x in 0..SIZE as i32 {
            let d = ((x as f32 + 0.5 - CX) / (10.5 / stretch.sqrt())).powi(2)
                + ((y as f32 + 0.5 - 29.5) / 2.2).powi(2);
            if d < 1. {
                canvas.blend(x, y, rgb(0x0b0a1a), 0.32 * (1. - d));
            }
        }
    }
    let height_at = |x: i32, y: i32| {
        if x < 0 || y < 0 || x >= SIZE as i32 || y >= SIZE as i32 {
            0.
        } else {
            height[y as usize * SIZE + x as usize]
        }
    };
    for y in 0..SIZE as i32 {
        for x in 0..SIZE as i32 {
            let index = y as usize * SIZE + x as usize;
            if !mask[index] {
                // One pixel of outline wherever the body meets the outside.
                let edge = [(-1, 0), (1, 0), (0, -1), (0, 1)].iter().any(|&(dx, dy)| {
                    let (nx, ny) = (x + dx, y + dy);
                    nx >= 0
                        && ny >= 0
                        && nx < SIZE as i32
                        && ny < SIZE as i32
                        && mask[ny as usize * SIZE + nx as usize]
                });
                if edge {
                    canvas.put(x, y, outline);
                }
                continue;
            }
            let slope = 5.5;
            let normal = normalize([
                -(height_at(x + 1, y) - height_at(x - 1, y)) * slope,
                -(height_at(x, y + 1) - height_at(x, y - 1)) * slope,
                1.,
            ]);
            let lambert = (normal[0] * sun[0] + normal[1] * sun[1] + normal[2] * sun[2]).max(0.);
            let dither = (BAYER[y as usize & 3][x as usize & 3] + 0.5) / 16. - 0.5;
            let light = 0.22 + 0.95 * lambert;
            let step = ((light * 3.4 + dither * 0.55).round() as usize).min(3);
            let mut color = colors[step];
            let specular = (normal[0] * half[0] + normal[1] * half[1] + normal[2] * half[2])
                .max(0.)
                .powi(28);
            if specular > 0.55 {
                color = colors[4];
            }
            if specular > 0.85 {
                color = rgb(0xffffff);
            }
            // A cool rim where the far side turns away, as if lit by the sky.
            if dist[index] <= 1.5 && normal[0] + normal[1] > 0.45 {
                color = mix(color, rgb(0x9fe8ff), 0.45);
            }
            canvas.put(x, y, color);
        }
    }
    face(&mut canvas, style.shape, look, stretch);
    let mut image = RgbaImage::new(SIZE as u32 * scale, SIZE as u32 * scale);
    for (index, pixel) in canvas.pixels.iter().enumerate() {
        let Some([r, g, b, a]) = *pixel else { continue };
        let rgba = Rgba([
            r.round().clamp(0., 255.) as u8,
            g.round().clamp(0., 255.) as u8,
            b.round().clamp(0., 255.) as u8,
            (a * 255.).round().clamp(0., 255.) as u8,
        ]);
        let (x, y) = ((index % SIZE) as u32, (index / SIZE) as u32);
        for dy in 0..scale {
            for dx in 0..scale {
                image.put_pixel(x * scale + dx, y * scale + dy, rgba);
            }
        }
    }
    image
}

const INK: u32 = 0x16142a;
const WHITE: u32 = 0xffffff;

fn face(canvas: &mut Canvas, shape: Shape, look: Look, stretch: f32) {
    let row = (FEET - (FEET - face_row(shape)) * stretch).round() as i32;
    let (ink, white) = (rgb(INK), rgb(WHITE));
    let eyes = [12, 19];
    // Rosy cheeks, softly, below and outside the eyes.
    for x in [9, 10, 22, 23] {
        canvas.blend(x, row + 4, rgb(0xff7a9a), 0.55);
    }
    match look {
        Look::Idle { blink: true } => {
            for left in eyes {
                for dx in 0..3 {
                    canvas.put(left + dx, row + 2, ink);
                }
            }
            smile(canvas, row + 5, 2, ink);
        }
        Look::Done { twinkle } => {
            // Happy arcs: ^ ^
            for left in eyes {
                canvas.put(left, row + 2, ink);
                canvas.put(left + 1, row + 1, ink);
                canvas.put(left + 2, row + 2, ink);
            }
            smile(canvas, row + 5, 3, ink);
            canvas.put(15, row + 6, rgb(0xff5d7a));
            canvas.put(16, row + 6, rgb(0xff5d7a));
            let stars: [(i32, i32); 2] = if twinkle {
                [(4, 5), (27, 11)]
            } else {
                [(27, 4), (5, 12)]
            };
            for (x, y) in stars {
                star(canvas, x, y);
            }
        }
        Look::Blocked { tremble } => {
            let shift = i32::from(tremble);
            for (i, left) in eyes.into_iter().enumerate() {
                let left = left + shift;
                eye(canvas, left, row, 0, white, ink);
                // Worried brows slope up toward the middle.
                let (outer, inner) = if i == 0 {
                    (left, left + 2)
                } else {
                    (left + 2, left)
                };
                canvas.put(outer, row - 1, ink);
                canvas.put(inner, row - 2, ink);
                canvas.put((outer + inner) / 2, row - 2, ink);
            }
            // A small "o" of dismay.
            for (x, y) in [(15, row + 5), (16, row + 5), (15, row + 6), (16, row + 6)] {
                canvas.put(x + shift, y, ink);
            }
            // A sweat drop at the temple.
            canvas.put(24 + shift, row - 3, rgb(0x9fe8ff));
            canvas.put(24 + shift, row - 2, rgb(0x6cc8ff));
            canvas.put(25 + shift, row - 2, rgb(0x9fe8ff));
            bubble(canvas);
        }
        Look::Idle { blink: false } | Look::Working { .. } => {
            for left in eyes {
                eye(canvas, left, row, look.gaze(), white, ink);
            }
            if matches!(look, Look::Working { .. }) {
                // Concentrating: a straight little mouth, tongue peeking out.
                canvas.put(15, row + 5, ink);
                canvas.put(16, row + 5, ink);
                canvas.put(17, row + 6, rgb(0xff5d7a));
            } else {
                smile(canvas, row + 5, 2, ink);
            }
        }
    }
}

/// An eye: a white 3 by 4 with a 2 by 2 pupil looking `gaze` pixels aside,
/// and a glint.
fn eye(canvas: &mut Canvas, left: i32, row: i32, gaze: i32, white: Color, ink: Color) {
    for dy in 0..4 {
        for dx in 0..3 {
            canvas.put(left + dx, row + dy, white);
        }
    }
    let pupil = left + gaze.clamp(-1, 1).max(0);
    for dy in 1..3 {
        for dx in 0..2 {
            canvas.put(pupil + dx, row + dy + 1, ink);
        }
    }
    canvas.put(pupil + 1, row + 1, white);
}

/// A smile `half` pixels either side of centre, corners raised.
fn smile(canvas: &mut Canvas, row: i32, half: i32, ink: Color) {
    for x in 16 - half..16 + half {
        canvas.put(x, row + 1, ink);
    }
    canvas.put(15 - half, row, ink);
    canvas.put(16 + half, row, ink);
}

/// A four-point twinkle.
fn star(canvas: &mut Canvas, x: i32, y: i32) {
    let gold = rgb(0xffe27a);
    canvas.put(x, y, rgb(WHITE));
    for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
        canvas.put(x + dx, y + dy, gold);
    }
}

/// A speech bubble with a red "!" at the top right: this one needs you.
fn bubble(canvas: &mut Canvas) {
    let (paper, edge, red) = (rgb(0xfff8f0), rgb(INK), rgb(0xf0405e));
    for y in 0..8 {
        for x in 24..31 {
            let corner = (x == 24 || x == 30) && (y == 0 || y == 7);
            if corner {
                continue;
            }
            let border = x == 24 || x == 30 || y == 0 || y == 7;
            canvas.put(x, y, if border { edge } else { paper });
        }
    }
    canvas.put(25, 8, edge);
    canvas.put(26, 8, paper);
    canvas.put(26, 9, edge);
    for y in 2..5 {
        canvas.put(27, y, red);
    }
    canvas.put(27, 6, red);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::pixel_avatar::Hue;

    fn style(shape: Shape) -> AvatarStyle {
        AvatarStyle {
            shape,
            hue: Hue::Cobalt,
        }
    }

    fn opaque(image: &RgbaImage) -> usize {
        image.pixels().filter(|p| p[3] == 255).count()
    }

    /// Writes every shape in every kind of look to `BUDDY_SHEET` as a PNG on a
    /// dark ground, for looking at the art itself.
    #[test]
    #[ignore = "writes a sprite sheet for review; set BUDDY_SHEET"]
    fn write_sprite_sheet() {
        let path = std::env::var("BUDDY_SHEET").unwrap_or_else(|_| "buddies.png".into());
        let looks = [
            Look::Idle { blink: false },
            Look::Idle { blink: true },
            Look::Working { frame: 0 },
            Look::Working { frame: 2 },
            Look::Blocked { tremble: false },
            Look::Done { twinkle: false },
        ];
        let hues = [
            Hue::Tangerine,
            Hue::Grape,
            Hue::Berry,
            Hue::Cobalt,
            Hue::Lagoon,
            Hue::Honey,
        ];
        let (scale, cell) = (4u32, 34u32 * 4);
        let mut sheet = RgbaImage::from_pixel(
            cell * looks.len() as u32,
            cell * Shape::ALL.len() as u32,
            Rgba([17, 21, 28, 255]),
        );
        for (row, shape) in Shape::ALL.into_iter().enumerate() {
            for (column, look) in looks.into_iter().enumerate() {
                let image = render(
                    AvatarStyle {
                        shape,
                        hue: hues[row],
                    },
                    look,
                    scale,
                );
                for (x, y, pixel) in image.enumerate_pixels() {
                    if pixel[3] == 0 {
                        continue;
                    }
                    let (sx, sy) = (column as u32 * cell + 4 + x, row as u32 * cell + 4 + y);
                    let under = *sheet.get_pixel(sx, sy);
                    let a = f32::from(pixel[3]) / 255.;
                    let blend =
                        |i: usize| (f32::from(pixel[i]) * a + f32::from(under[i]) * (1. - a)) as u8;
                    sheet.put_pixel(sx, sy, Rgba([blend(0), blend(1), blend(2), 255]));
                }
            }
        }
        sheet.save(path).unwrap();
    }

    #[test]
    fn every_shape_is_a_lit_body_with_distinct_tones() {
        for shape in Shape::ALL {
            let image = render(style(shape), Look::Idle { blink: false }, 1);
            let body = opaque(&image);
            assert!((260..=620).contains(&body), "{shape:?}: {body}");
            let tones: std::collections::HashSet<_> = image
                .pixels()
                .filter(|p| p[3] == 255)
                .map(|p| (p[0], p[1], p[2]))
                .collect();
            // Ramp, highlight, rim, outline, eyes, cheeks: real shading.
            assert!(tones.len() >= 8, "{shape:?}: {}", tones.len());
        }
    }

    #[test]
    fn the_light_comes_from_the_top_left() {
        let image = render(style(Shape::Blob), Look::Idle { blink: false }, 1);
        let brightness = |x: u32, y: u32| {
            let p = image.get_pixel(x, y);
            u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2])
        };
        assert!(brightness(9, 12) > brightness(24, 25));
    }

    #[test]
    fn expressions_differ_and_working_squashes_and_stretches() {
        let blob = style(Shape::Blob);
        let looks = [
            Look::Idle { blink: false },
            Look::Idle { blink: true },
            Look::Working { frame: 0 },
            Look::Blocked { tremble: false },
            Look::Done { twinkle: false },
        ];
        let images: Vec<_> = looks.iter().map(|&look| render(blob, look, 1)).collect();
        for (i, a) in images.iter().enumerate() {
            for b in &images[i + 1..] {
                assert_ne!(a, b);
            }
        }
        let tall = |image: &RgbaImage| {
            (0..SIZE as u32)
                .filter(|&y| (0..SIZE as u32).any(|x| image.get_pixel(x, y)[3] == 255))
                .count()
        };
        let stretched = render(blob, Look::Working { frame: 0 }, 1);
        let squashed = render(blob, Look::Working { frame: 2 }, 1);
        assert!(tall(&stretched) > tall(&squashed));
    }

    #[test]
    fn a_blocked_buddy_raises_a_red_bubble_and_scale_repeats_pixels() {
        let blocked = render(style(Shape::Gem), Look::Blocked { tremble: false }, 1);
        let red = blocked.get_pixel(27, 3);
        assert!(red[0] > 200 && red[1] < 100, "{red:?}");
        let one = render(style(Shape::Gem), Look::Done { twinkle: true }, 1);
        let two = render(style(Shape::Gem), Look::Done { twinkle: true }, 2);
        assert_eq!(two.dimensions(), (64, 64));
        for (x, y) in [(16, 18), (4, 5), (12, 20)] {
            assert_eq!(two.get_pixel(x * 2 + 1, y * 2 + 1), one.get_pixel(x, y));
        }
    }
}
