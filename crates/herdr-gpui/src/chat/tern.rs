//! The chat's look: Tern's native surface, value for value. Colors are
//! `0xRRGGBBAA`, sizes are in ems of the pane's font as Tern states them, and
//! motion uses Tern's curves and timings.

use gpui::{App, Rgba, rgba};
use std::{borrow::Cow, time::Duration};

pub(super) const SANS: &str = "Geist";

/// The chat pane's font size for a UI font of `ui`: a few points larger, so
/// long replies read comfortably. Every other size is in ems of this.
pub(crate) fn pane_em(ui: f32) -> f32 {
    ui + 3.
}
pub(super) const MONO: &str = "Geist Mono";

/// The pane.
pub(super) const TILE: u32 = 0x151820ff;
pub(super) const TILE_EDGE: u32 = 0xffffff09;
pub(super) const TILE_FOCUS: u32 = 0x1ba6e080;
pub(super) const INK: u32 = 0xc8cad0ff;
pub(super) const INK_2: u32 = 0x979ca7ff;
pub(super) const DIM: u32 = 0x5f646eff;
pub(super) const LINE: u32 = 0xffffff12;
pub(super) const TEAL: u32 = 0x34e4c9ff;
/// Tern's other accents, used to tell things apart at a glance.
pub(super) const PERI: u32 = 0x8f9bffff;
pub(super) const PINK: u32 = 0xe58ad6ff;
pub(super) const AMBER: u32 = 0xe5b454ff;
pub(super) const BRIGHT: u32 = 0xeef0f3ff;
/// Agents in their own colours.
pub(super) const CLAUDE: u32 = 0xe8906bff;
pub(super) const CODEX: u32 = 0x8f9bffff;

/// Accents in turn, for things that are peers: diagram nodes.
pub(super) fn accent(index: usize) -> u32 {
    [TEAL, PERI, PINK, AMBER][index % 4]
}

/// `value` at `alpha` (0–255), for washes and outlines of an accent.
pub(super) fn wash(value: u32, alpha: u32) -> u32 {
    (value & 0xffffff00) | alpha.min(255)
}

/// The `NATIVE` tag.
pub(super) const TAG_FILL: u32 = 0x34e4c91a;
pub(super) const TAG_EDGE: u32 = 0x34e4c952;

/// Figures: code and diagrams.
pub(super) const FIG: u32 = 0x0c0e13ff;
pub(super) const FIG_EDGE: u32 = 0xffffff0f;
pub(super) const FIG_RULE: u32 = 0xffffff0d;

/// Tool cards.
pub(super) const CARD: u32 = 0xffffff06;
pub(super) const CARD_EDGE: u32 = 0xffffff0f;

pub(super) const RUN_FILL: u32 = 0xa076ff24;
pub(super) const RUN_INK: u32 = 0xb996ffff;
pub(super) const DONE_FILL: u32 = 0x48d48414;
pub(super) const DONE_EDGE: u32 = 0x48d48473;
pub(super) const DONE_INK: u32 = 0x52d68cff;
pub(super) const FAIL_FILL: u32 = 0xe2525e1a;
pub(super) const FAIL_EDGE: u32 = 0xe2525e73;
pub(super) const FAIL_INK: u32 = 0xff7a85ff;

/// Diffs.
pub(super) const DEL_FILL: u32 = 0xe2525e1a;
pub(super) const DEL_BAR: u32 = 0xe2525eff;
pub(super) const DEL_MARK: u32 = 0xe2525e4d;
pub(super) const ADD_FILL: u32 = 0x5cc43a1a;
pub(super) const ADD_BAR: u32 = 0x5cc43aff;
pub(super) const ADD_MARK: u32 = 0x5cc43a47;
pub(super) const ADD_MARK_INK: u32 = 0xd9f5cfff;

/// Tables.
pub(super) const TH_RULE: u32 = 0xffffff24;
pub(super) const TD_RULE: u32 = 0xffffff0d;
pub(super) const CODE_FILL: u32 = 0x34e4c914;
pub(super) const CODE_INK: u32 = 0x6fe0cfff;

/// Diagrams.
pub(super) const NODE_FILL: u32 = 0x161920ff;
pub(super) const NODE_EDGE: u32 = 0xffffff3d;
pub(super) const NODE_INK: u32 = 0xe8eaeeff;
pub(super) const LABEL_EDGE: u32 = 0xffffff1f;
pub(super) const WIRE: u32 = 0x8d939eff;

pub(super) const COMPOSER_EDGE: u32 = 0xffffff17;

/// Terminal colors, as Tern's syntax classes name them.
pub(super) const GREEN: u32 = 0x5cc43aff;
pub(super) const BLUE: u32 = 0x1f9fdcff;
pub(super) const ORANGE: u32 = 0xd69b45ff;
pub(super) const RED: u32 = 0xe2525eff;
pub(super) const YELLOW: u32 = 0xc9a42aff;
pub(super) const CYAN: u32 = 0x38b9daff;
pub(super) const MAGENTA: u32 = 0xc47ee0ff;
pub(super) const GRAY: u32 = 0x666b75ff;

pub(super) fn color(value: u32) -> Rgba {
    rgba(value)
}

/// `a` to `b` at `t`, channel by channel.
pub(super) fn blend(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0., 1.);
    let channel = |shift: u32| {
        let from = ((a >> shift) & 255) as f32;
        let to = ((b >> shift) & 255) as f32;
        ((from + (to - from) * t).round() as u32) << shift
    };
    channel(24) | channel(16) | channel(8) | channel(0)
}

/// Motion.
pub(super) const RISE: Duration = Duration::from_millis(520);
pub(super) const CHUNK: Duration = Duration::from_millis(1100);
pub(super) const POP: Duration = Duration::from_millis(887);
pub(super) const DOTS: Duration = Duration::from_millis(1200);
pub(super) const SHIMMER: Duration = Duration::from_millis(2200);
pub(super) const PULSE: Duration = Duration::from_millis(1100);
pub(super) const INK_IN: Duration = Duration::from_millis(900);

/// `cubic-bezier(x1, y1, x2, y2)` at `t`, solved for x by bisection.
pub(super) fn bezier(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    let curve = |a: f32, b: f32, s: f32| {
        let u = 1. - s;
        3. * u * u * s * a + 3. * u * s * s * b + s * s * s
    };
    let (mut low, mut high) = (0., 1.);
    let mut s = t;
    for _ in 0..24 {
        let x = curve(x1, x2, s);
        if (x - t).abs() < 1e-4 {
            break;
        }
        if x < t {
            low = s;
        } else {
            high = s;
        }
        s = (low + high) / 2.;
    }
    curve(y1, y2, s)
}

/// `--st-ease-out`.
pub(super) fn ease_out(t: f32) -> f32 {
    bezier(0.16, 1., 0.3, 1., t)
}

/// `--st-ease`.
pub(super) fn ease(t: f32) -> f32 {
    bezier(0.2, 0.7, 0.2, 1., t)
}

/// `--tn-pop`: a spring that overshoots to about 1.08 and settles.
pub(super) fn pop(t: f32) -> f32 {
    const POINTS: [f32; 36] = [
        0., 0.042, 0.1462, 0.2846, 0.4353, 0.5825, 0.7158, 0.829, 0.9197, 0.9877, 1.0351, 1.0646,
        1.0796, 1.0835, 1.0796, 1.0707, 1.059, 1.0464, 1.0341, 1.0231, 1.0138, 1.0063, 1.0007,
        0.9969, 0.9945, 0.9933, 0.993, 0.9934, 0.9942, 0.9951, 0.9962, 0.9972, 0.9981, 0.9989,
        0.9995, 1.,
    ];
    let at = t.clamp(0., 1.) * (POINTS.len() - 1) as f32;
    let index = (at.floor() as usize).min(POINTS.len() - 2);
    let fraction = at - index as f32;
    POINTS[index] + (POINTS[index + 1] - POINTS[index]) * fraction
}

/// Tern's `NATIVE` tag: teal mono capitals in an outlined chip, sized for a
/// pane whose font is `size`.
pub(crate) fn native_tag(size: f32) -> gpui::Div {
    use gpui::{ParentElement, Styled, div, px};
    let tag = pane_em(size) * 0.95 * 0.78;
    div()
        .flex_none()
        .px(px(tag * 0.5))
        .py(px(tag * 0.2))
        .rounded(px(tag * 0.4))
        .bg(color(TAG_FILL))
        .border_1()
        .border_color(color(TAG_EDGE))
        .text_color(color(TEAL))
        .font_family(MONO)
        .text_size(px(tag))
        .child("NATIVE")
}

/// Loads the faces Tern is set in. Geist and Geist Mono are free under the
/// SIL Open Font License, bundled with it in `assets/fonts/geist`; where
/// they fail to load, text falls back to the configured UI font.
pub(crate) fn install_fonts(cx: &mut App) {
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(include_bytes!(
            "../../../../assets/fonts/geist/Geist-Regular.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../../../assets/fonts/geist/Geist-Medium.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../../../assets/fonts/geist/Geist-SemiBold.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../../../assets/fonts/geist/Geist-Italic.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../../../assets/fonts/geist/GeistMono-Regular.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../../../assets/fonts/geist/GeistMono-Medium.ttf"
        )),
    ];
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        tracing::warn!(%error, "could not load the chat's Geist fonts");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_start_at_zero_end_at_one_and_the_pop_overshoots() {
        for curve in [ease_out, ease, pop] {
            assert!(curve(0.).abs() < 1e-3);
            assert!((curve(1.) - 1.).abs() < 1e-3);
        }
        assert!(ease_out(0.2) > 0.5, "ease-out front-loads its motion");
        assert!(pop(0.37) > 1.07, "the pop overshoots before settling");
        assert_eq!(blend(0x000000ff, 0xffffffff, 0.5), 0x808080ff);
        assert_eq!(blend(0x11223344, 0x55667788, 0.), 0x11223344);
    }
}
