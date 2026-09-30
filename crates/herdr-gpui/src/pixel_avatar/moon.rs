//! Moons: a cratered sphere lit by a sun whose angle is the phase, rendered
//! to a small image with quantized, dithered light for a pixel-art finish.
//!
//! A space's style picks its crater pattern and tint. The surface, which
//! costs noise per pixel, is computed once per style; a pose only relights
//! it, so rendering a new phase is a cheap pass over 1024 pixels.

use super::style::{AvatarStyle, Hue};
use image::{Rgba, RgbaImage};
use std::{cell::RefCell, collections::HashMap, f32::consts::TAU, rc::Rc};

/// Moons are this many pixels on a side, one per point at sidebar size.
pub(crate) const SIZE: usize = 32;
const CENTRE: f32 = 16.;
const RADIUS: f32 = 12.;
/// The halo reaches this far past the limb, as a fraction of the radius.
const HALO: f32 = 1.3;
/// Working phases step through this many images per sweep.
pub(crate) const PHASES: u8 = 24;
const BAYER: [[f32; 4]; 4] = [
    [0., 8., 2., 10.],
    [12., 4., 14., 6.],
    [3., 11., 1., 9.],
    [15., 7., 13., 5.],
];

/// How a moon is lit, which is all that differs between its poses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Light {
    /// A thin crescent, its dark side faintly lit by earthshine: at rest.
    Crescent,
    /// One step of the terminator's sweep across the face: at work.
    Phase(u8),
    /// A full moon gone red, glowing brighter or dimmer: waiting on you.
    Blood { bright: bool },
    /// Full and haloed: finished.
    Full,
}

/// Everything a light decides about a render.
struct Lighting {
    sun: [f32; 3],
    /// Light on the dark side, so it never goes fully black.
    earthshine: f32,
    /// Replaces the surface colour, keeping its crater shading.
    blood: Option<[f32; 3]>,
    /// Glow past the limb: colour and strength.
    halo: Option<([f32; 3], f32)>,
}

impl Light {
    fn setup(self) -> Lighting {
        let sun = |phase: f32| normalize([phase.sin(), -0.18, phase.cos()]);
        match self {
            Self::Crescent => Lighting {
                sun: sun(2.05),
                earthshine: 0.16,
                blood: None,
                halo: None,
            },
            Self::Phase(step) => Lighting {
                sun: sun(-2.6 + f32::from(step % PHASES) / f32::from(PHASES) * 5.2),
                earthshine: 0.12,
                blood: None,
                halo: None,
            },
            Self::Blood { bright } => Lighting {
                sun: sun(0.),
                earthshine: 0.35,
                blood: Some([180., 58., 69.]),
                halo: Some(([255., 77., 106.], if bright { 0.95 } else { 0.55 })),
            },
            Self::Full => Lighting {
                sun: sun(0.),
                earthshine: 0.12,
                blood: None,
                halo: Some(([255., 244., 214.], 0.6)),
            },
        }
    }
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    [v[0] / length, v[1] / length, v[2] / length]
}

fn hash(x: i32, y: i32, z: i32, seed: u32) -> f32 {
    let mut n = (x as u32).wrapping_mul(374_761_393)
        ^ (y as u32).wrapping_mul(668_265_263)
        ^ (z as u32).wrapping_mul(2_147_483_647)
        ^ seed.wrapping_mul(1_274_126_177);
    n = (n ^ (n >> 13)).wrapping_mul(1_103_515_245);
    n ^= n >> 16;
    n as f32 / u32::MAX as f32
}

fn value_noise(x: f32, y: f32, z: f32, seed: u32) -> f32 {
    let (xi, yi, zi) = (x.floor(), y.floor(), z.floor());
    let smooth = |t: f32| t * t * (3. - 2. * t);
    let (xf, yf, zf) = (smooth(x - xi), smooth(y - yi), smooth(z - zi));
    let (xi, yi, zi) = (xi as i32, yi as i32, zi as i32);
    let corner = |dx, dy, dz| hash(xi + dx, yi + dy, zi + dz, seed);
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    lerp(
        lerp(
            lerp(corner(0, 0, 0), corner(1, 0, 0), xf),
            lerp(corner(0, 1, 0), corner(1, 1, 0), xf),
            yf,
        ),
        lerp(
            lerp(corner(0, 0, 1), corner(1, 0, 1), xf),
            lerp(corner(0, 1, 1), corner(1, 1, 1), xf),
            yf,
        ),
        zf,
    )
}

/// Noise over the sphere, seamless in longitude.
fn sphere_noise(lon: f32, lat: f32, scale: f32, seed: u32) -> f32 {
    let (x, y, z) = (
        lon.cos() * lat.cos() * scale + 9.,
        lon.sin() * lat.cos() * scale + 9.,
        lat.sin() * scale + 9.,
    );
    0.58 * value_noise(x, y, z, seed)
        + 0.29 * value_noise(x * 2.1, y * 2.1, z * 2.1, seed + 17)
        + 0.13 * value_noise(x * 4.3, y * 4.3, z * 4.3, seed + 31)
}

/// Lunar grey leaning toward the chosen hue.
fn tint(hue: Hue) -> [f32; 3] {
    let base = hue.base();
    let channel = |shift: u32| ((base >> shift) & 0xff) as f32;
    let grey = [195., 198., 207.];
    let hue = [channel(16), channel(8), channel(0)];
    [0, 1, 2].map(|i| grey[i] + (hue[i] - grey[i]) * 0.3)
}

/// A pixel of the moon's face: its surface normal and albedo.
#[derive(Clone, Copy)]
struct Texel {
    normal: [f32; 3],
    albedo: [f32; 3],
    /// Darkening a blood moon keeps from its craters.
    shade: f32,
}

struct Surface {
    texels: Vec<Option<Texel>>,
}

fn surface(style: AvatarStyle) -> Surface {
    let seed = style.shape as u32 * 7919 + 17;
    let base = tint(style.hue);
    let spin = (seed % 9) as f32 * 0.7;
    let craters: Vec<(f32, f32, f32)> = (0..7)
        .map(|i| {
            (
                hash(i, 1, 0, seed) * TAU,
                (hash(i, 2, 0, seed) - 0.5) * 2.4,
                0.18 + hash(i, 3, 0, seed) * 0.32,
            )
        })
        .collect();
    let texels = (0..SIZE * SIZE)
        .map(|index| {
            let (x, y) = ((index % SIZE) as f32 + 0.5, (index / SIZE) as f32 + 0.5);
            let (nx, ny) = ((x - CENTRE) / RADIUS, (y - CENTRE) / RADIUS);
            let d2 = nx * nx + ny * ny;
            if d2 > 1. {
                return None;
            }
            let nz = (1. - d2).sqrt();
            let lon = nx.atan2(nz) + spin;
            let lat = (-ny).asin();
            let mut shade = 1. - 0.2 * sphere_noise(lon, lat, 3., seed);
            for &(clon, clat, r) in &craters {
                let dlon = (lon - clon).sin().atan2((lon - clon).cos());
                let d = (dlon * lat.cos()).hypot(lat - clat) / r;
                if d < 0.78 {
                    shade *= 0.72;
                } else if d < 1. {
                    shade = shade * 0.86 + 0.14 * (255. / base[0].max(1.));
                }
            }
            Some(Texel {
                normal: [nx, ny, nz],
                albedo: base.map(|c| c * shade),
                shade,
            })
        })
        .collect();
    Surface { texels }
}

thread_local! {
    static SURFACES: RefCell<HashMap<AvatarStyle, Rc<Surface>>> = RefCell::default();
}

/// A style's surface, computed on first use. Styles are few, so the map
/// stays small without eviction.
fn cached_surface(style: AvatarStyle) -> Rc<Surface> {
    SURFACES.with(|surfaces| {
        surfaces
            .borrow_mut()
            .entry(style)
            .or_insert_with(|| Rc::new(surface(style)))
            .clone()
    })
}

/// The moon for `style` under `light`, `scale` device pixels per moon pixel,
/// as straight-alpha RGBA with a transparent background.
pub(crate) fn render(style: AvatarStyle, light: Light, scale: u32) -> RgbaImage {
    let scale = scale.clamp(1, 4);
    let surface = cached_surface(style);
    let Lighting {
        sun,
        earthshine: ambient,
        blood,
        halo,
    } = light.setup();
    let side = SIZE as u32 * scale;
    let mut image = RgbaImage::new(side, side);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let pixel = match surface.texels[y * SIZE + x] {
                Some(texel) => {
                    let n = texel.normal;
                    let lambert = (n[0] * sun[0] + n[1] * sun[1] + n[2] * sun[2]).max(0.);
                    let dither = (BAYER[y & 3][x & 3] + 0.5) / 16. - 0.5;
                    let step = ((lambert * 6. + dither * 0.85).round() / 6.).clamp(0., 1.15);
                    let light = ambient + (1. - ambient) * step;
                    let albedo = blood.map_or(texel.albedo, |red| red.map(|c| c * texel.shade));
                    let [r, g, b] = albedo.map(|c| (c * light).round().clamp(0., 255.) as u8);
                    Rgba([r, g, b, 255])
                }
                None => {
                    let d = ((x as f32 + 0.5 - CENTRE).hypot(y as f32 + 0.5 - CENTRE)) / RADIUS;
                    match halo {
                        Some((color, strength)) if d < HALO => {
                            let alpha = (1. - (d - 1.) / (HALO - 1.)).powi(2) * strength;
                            let [r, g, b] = color.map(|c| c as u8);
                            Rgba([r, g, b, (alpha * 255.).round() as u8])
                        }
                        _ => continue,
                    }
                }
            };
            for dy in 0..scale {
                for dx in 0..scale {
                    image.put_pixel(x as u32 * scale + dx, y as u32 * scale + dy, pixel);
                }
            }
        }
    }
    image
}

/// The working sweep's step at `millis` into the shared clock, and how long
/// until the next one. A full sweep takes about six seconds.
pub(crate) fn phase_at(millis: u128) -> (u8, u128) {
    const STEP: u128 = 240;
    let step = (millis / STEP % u128::from(PHASES)) as u8;
    (step, STEP - millis % STEP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pixel_avatar::Shape;

    const STYLE: AvatarStyle = AvatarStyle {
        shape: Shape::Gem,
        hue: Hue::Slate,
    };

    fn lit(image: &RgbaImage, threshold: u8) -> usize {
        image
            .pixels()
            .filter(|p| p[3] == 255 && p[0].max(p[1]).max(p[2]) > threshold)
            .count()
    }

    #[test]
    fn the_phase_decides_how_much_of_the_face_is_lit() {
        let full = render(STYLE, Light::Full, 1);
        let crescent = render(STYLE, Light::Crescent, 1);
        let face = full.pixels().filter(|p| p[3] == 255).count();
        assert!((400..=460).contains(&face), "{face}");
        assert!(lit(&full, 90) > face * 2 / 3, "{}", lit(&full, 90));
        assert!(lit(&crescent, 90) < face / 4, "{}", lit(&crescent, 90));
        // Earthshine keeps the dark side visible rather than black.
        assert!(crescent.pixels().filter(|p| p[3] == 255).all(|p| p[0] > 12));
    }

    #[test]
    fn a_blood_moon_is_red_and_only_full_and_blood_moons_glow() {
        let blood = render(STYLE, Light::Blood { bright: true }, 1);
        let red = blood
            .pixels()
            .filter(|p| p[3] == 255 && p[0] > p[1] + 40 && p[0] > p[2] + 30)
            .count();
        assert!(red > 350, "{red}");
        let halo = |image: &RgbaImage| image.pixels().filter(|p| p[3] > 0 && p[3] < 255).count();
        assert!(halo(&blood) > 50);
        assert!(halo(&render(STYLE, Light::Full, 1)) > 50);
        assert_eq!(halo(&render(STYLE, Light::Crescent, 1)), 0);
        let dim = render(STYLE, Light::Blood { bright: false }, 1);
        let alpha = |image: &RgbaImage| image.pixels().map(|p| u32::from(p[3])).sum::<u32>();
        assert!(alpha(&blood) > alpha(&dim));
    }

    #[test]
    fn the_working_sweep_moves_the_terminator_and_loops() {
        let lit_at = |step| lit(&render(STYLE, Light::Phase(step), 1), 120);
        let counts: Vec<_> = (0..PHASES).map(lit_at).collect();
        assert!(counts.iter().max() > counts.iter().min());
        assert_eq!(lit_at(PHASES), counts[0]);
        assert_eq!(phase_at(0), (0, 240));
        assert_eq!(phase_at(250), (1, 230));
        assert_eq!(phase_at(240 * u128::from(PHASES)).0, 0);
    }

    #[test]
    fn styles_differ_and_scale_repeats_each_pixel() {
        let other = AvatarStyle {
            shape: Shape::Cloud,
            hue: Hue::Tangerine,
        };
        assert_ne!(render(STYLE, Light::Full, 1), render(other, Light::Full, 1));
        let one = render(STYLE, Light::Full, 1);
        let two = render(STYLE, Light::Full, 2);
        assert_eq!(two.dimensions(), (64, 64));
        for (x, y) in [(16, 16), (10, 20), (3, 3)] {
            assert_eq!(two.get_pixel(x * 2 + 1, y * 2 + 1), one.get_pixel(x, y));
        }
    }
}
