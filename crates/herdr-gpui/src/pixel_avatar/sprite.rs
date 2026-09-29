//! Shapes rasterized to a 16 by 16 pixel grid, once per process. Each pixel
//! is empty or one of three tones, which is all a pixel-art body needs to
//! read as rounded: a lit upper-left edge and a shaded rim.

use super::{paint::GRID, style::Shape};
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tone {
    Body,
    Light,
    Shade,
}

pub(super) struct Sprite {
    pixels: [[Option<Tone>; GRID]; GRID],
    /// Top pixel of each eye; eyes are one pixel wide and two tall.
    pub(super) eyes: [(i8, i8); 2],
}

impl Sprite {
    #[cfg(test)]
    pub(super) fn tone(&self, x: usize, y: usize) -> Option<Tone> {
        self.pixels[y][x]
    }

    /// Horizontal runs of one tone in row `y`, as `(start, length, tone)`, so
    /// a row paints as a few quads rather than one per pixel.
    pub(super) fn runs(&self, y: usize) -> impl Iterator<Item = (usize, usize, Tone)> + '_ {
        let row = &self.pixels[y];
        let mut x = 0;
        std::iter::from_fn(move || {
            while x < GRID && row[x].is_none() {
                x += 1;
            }
            let tone = row.get(x).copied().flatten()?;
            let start = x;
            while x < GRID && row[x] == Some(tone) {
                x += 1;
            }
            Some((start, x - start, tone))
        })
    }
}

static SPRITES: LazyLock<[Sprite; 6]> = LazyLock::new(|| Shape::ALL.map(rasterize));

pub(super) fn sprite(shape: Shape) -> &'static Sprite {
    let index = Shape::ALL
        .iter()
        .position(|candidate| *candidate == shape)
        .unwrap_or_default();
    &SPRITES[index]
}

const CLOUD: [&str; GRID] = [
    "................",
    "................",
    "......####......",
    ".....######.....",
    "..###.####.###..",
    ".##############.",
    ".##############.",
    ".##############.",
    ".##############.",
    ".##############.",
    ".##############.",
    ".##############.",
    "..############..",
    "...##########...",
    "................",
    "................",
];

/// Whether the pixel centred at (`x`, `y`) lies inside `shape`.
fn inside(shape: Shape, x: f32, y: f32) -> bool {
    let circle = |cx: f32, cy: f32, r: f32| (x - cx).powi(2) + (y - cy).powi(2) <= r * r;
    match shape {
        Shape::Blob => ((x - 8.) / 7.3).powi(2) + ((y - 9.) / 6.3).powi(2) <= 1.,
        Shape::Drop => {
            circle(8., 9.7, 5.5)
                || ((1.2..=9.7).contains(&y)
                    && (x - 8.).abs() <= 5.5 * ((y - 1.2) / 8.5).powf(0.85))
        }
        Shape::Peak => {
            let half = 7.8 * (y - 1.2) / 12.4;
            (1.2..=14.4).contains(&y)
                && (x - 8.).abs() <= half
                // Rounded base corners.
                && !(y > 12.8 && (x - 8.).abs() > half - 1.2)
        }
        Shape::Gem => {
            let (dx, dy) = ((x - 8.).abs(), (y - 8.5).abs());
            dx <= 6.5 && dy + dx * 0.577 <= 7.4
        }
        // Three lobes do not survive sixteen pixels as circles, so the cloud
        // is drawn by hand.
        Shape::Cloud => {
            CLOUD
                .get(y as usize)
                .and_then(|row| row.as_bytes().get(x as usize))
                == Some(&b'#')
        }
        Shape::Block => {
            let (dx, dy) = ((x - 8.).abs(), (y - 8.5).abs());
            dx <= 6.6 && dy <= 6.1 && dx + dy <= 10.6
        }
    }
}

fn rasterize(shape: Shape) -> Sprite {
    let mut mask = [[false; GRID]; GRID];
    for (y, row) in mask.iter_mut().enumerate() {
        for (x, pixel) in row.iter_mut().enumerate() {
            *pixel = inside(shape, x as f32 + 0.5, y as f32 + 0.5);
        }
    }
    let filled = |x: isize, y: isize| {
        usize::try_from(x)
            .ok()
            .zip(usize::try_from(y).ok())
            .and_then(|(x, y)| mask.get(y).and_then(|row| row.get(x)))
            .copied()
            .unwrap_or(false)
    };
    let top = mask.iter().position(|row| row.contains(&true)).unwrap_or(0);
    let bottom = mask
        .iter()
        .rposition(|row| row.contains(&true))
        .unwrap_or(GRID - 1);
    let middle = (top + bottom) / 2;
    let mut pixels = [[None; GRID]; GRID];
    for (y, row) in pixels.iter_mut().enumerate() {
        for (x, pixel) in row.iter_mut().enumerate() {
            let (sx, sy) = (x as isize, y as isize);
            if !filled(sx, sy) {
                continue;
            }
            let edge_below = !filled(sx, sy + 1) || (y > middle && !filled(sx + 1, sy));
            let edge_above = (!filled(sx, sy - 1) || !filled(sx - 1, sy)) && x < 9 && y <= middle;
            *pixel = Some(if edge_below {
                Tone::Shade
            } else if edge_above {
                Tone::Light
            } else {
                Tone::Body
            });
        }
    }
    // Eyes sit a little above the body's middle, as on a face, and a touch
    // left of centre, which reads as looking toward the list's text.
    let eye_row = match shape {
        // Narrow tops push a pointed body's face toward its wider base.
        Shape::Drop | Shape::Peak => top + (bottom - top) * 11 / 20,
        _ => top + (bottom - top) * 9 / 20,
    }
    .max(top + 2) as i8;
    Sprite {
        pixels,
        eyes: [(6, eye_row), (9, eye_row)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shape_is_a_single_body_with_room_for_its_eyes() {
        for shape in Shape::ALL {
            let sprite = sprite(shape);
            let count = (0..GRID)
                .flat_map(|y| (0..GRID).map(move |x| (x, y)))
                .filter(|&(x, y)| sprite.tone(x, y).is_some())
                .count();
            assert!((90..=230).contains(&count), "{shape:?}: {count}");
            for (x, y) in sprite.eyes {
                for dy in 0..2 {
                    let (x, y) = (x as usize, (y + dy) as usize);
                    assert!(sprite.tone(x, y).is_some(), "{shape:?} eye at {x},{y}");
                }
            }
            // Pixel art reads as shaded only when all three tones appear.
            for tone in [Tone::Body, Tone::Light, Tone::Shade] {
                assert!(
                    (0..GRID).any(|y| sprite.runs(y).any(|run| run.2 == tone)),
                    "{shape:?} lacks {tone:?}"
                );
            }
        }
    }

    #[test]
    #[ignore = "prints every sprite for eyeballing"]
    fn print_sprites() {
        for shape in Shape::ALL {
            let sprite = sprite(shape);
            println!("{shape:?}");
            for y in 0..GRID {
                let row: String = (0..GRID)
                    .map(|x| {
                        if sprite.eyes.iter().any(|&(ex, ey)| {
                            ex as usize == x && (ey as usize == y || ey as usize + 1 == y)
                        }) {
                            return '@';
                        }
                        match sprite.tone(x, y) {
                            None => '.',
                            Some(Tone::Body) => '#',
                            Some(Tone::Light) => '+',
                            Some(Tone::Shade) => '=',
                        }
                    })
                    .collect();
                println!("{row}");
            }
        }
    }

    #[test]
    fn runs_cover_exactly_the_filled_pixels() {
        let sprite = sprite(Shape::Cloud);
        for y in 0..GRID {
            let mut covered = [None; GRID];
            for (start, length, tone) in sprite.runs(y) {
                assert!(length > 0);
                for slot in &mut covered[start..start + length] {
                    assert!(slot.is_none());
                    *slot = Some(tone);
                }
            }
            for (x, slot) in covered.iter().enumerate() {
                assert_eq!(*slot, sprite.tone(x, y), "{x},{y}");
            }
        }
    }

    #[test]
    fn shapes_differ_from_each_other() {
        let masks: Vec<Vec<bool>> = Shape::ALL
            .iter()
            .map(|shape| {
                let sprite = sprite(*shape);
                (0..GRID * GRID)
                    .map(|i| sprite.tone(i % GRID, i / GRID).is_some())
                    .collect()
            })
            .collect();
        for (i, a) in masks.iter().enumerate() {
            for b in &masks[i + 1..] {
                let differing = a.iter().zip(b).filter(|(a, b)| a != b).count();
                assert!(differing > 12, "{differing}");
            }
        }
    }
}
