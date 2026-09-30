//! Painting what the diagram and math engines lay out, as Tern draws its
//! figures. A diagram is painted whole on a canvas, labels included, so it
//! scales to its card as Tern's SVG does; fresh diagrams draw themselves in
//! with Tern's timings. Math glyphs are text at the engine's baselines.

use super::{
    math,
    mermaid::{self, Arrow, Font as LabelFont, Kind, ShapeKind, Stroke, Tone},
    render::{BlockKey, Diagram, Formula, progress},
    tern::{self, color},
    view::ChatPanel,
};
use crate::config::FontConfig;
use gpui::{prelude::*, *};
use std::{
    rc::Rc,
    time::{Duration, Instant},
};

/// The family math is set in; missing glyphs fall back to the UI font.
const MATH_FAMILY: &str = "Times New Roman";
/// Tern's chart text, in chart units: node names and edge labels.
const NODE_TEXT: f32 = 15.;
const EDGE_TEXT: f32 = 13.;
/// Room around a node's name, making Tern's 52-unit boxes for one line.
const NODE_PAD: (f32, f32) = (46., 32.);
/// Room around an edge label, inside its outlined chip.
const LABEL_PAD: (f32, f32) = (11., 1.5);
/// The most a diagram is enlarged to fill its card.
const MAX_SCALE: f32 = 1.25;

fn text_size(font: LabelFont) -> f32 {
    match font {
        LabelFont::Node => NODE_TEXT,
        LabelFont::Edge => EDGE_TEXT,
    }
}

/// The width of one line of `text` in `font` at `size`.
fn measure(window: &Window, text: &str, font: Font, size: f32) -> f32 {
    if text.is_empty() {
        return 0.;
    }
    let run = TextRun {
        len: text.len(),
        font,
        color: black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window.text_system().shape_line(
        SharedString::from(text.to_owned()),
        px(size),
        &[run],
        None,
    );
    f32::from(line.width)
}

fn math_font(ui: &FontConfig, italic: bool, bold: bool) -> Font {
    let mut font = ui.font();
    font.family = MATH_FAMILY.into();
    font.fallbacks = Some(FontFallbacks::from_fonts(vec![ui.family.clone()]));
    font.style = if italic {
        FontStyle::Italic
    } else {
        FontStyle::Normal
    };
    font.weight = if bold {
        FontWeight::BOLD
    } else {
        FontWeight::NORMAL
    };
    font
}

impl ChatPanel {
    pub(super) fn diagram(
        &mut self,
        key: BlockKey,
        source: &str,
        born: Option<Instant>,
        window: &Window,
    ) -> Option<Diagram> {
        let mono = self.mono();
        self.caches
            .diagrams
            .entry(key)
            .or_insert_with(|| {
                let diagram = mermaid::parse(source)?;
                let scene = diagram.layout(|text, font| {
                    let size = text_size(font);
                    let width = text
                        .split('\n')
                        .map(|line| measure(window, line, mono.clone(), size))
                        .fold(0., f32::max);
                    let lines = text.split('\n').count().max(1) as f32;
                    let pad = match font {
                        LabelFont::Node => NODE_PAD,
                        LabelFont::Edge => LABEL_PAD,
                    };
                    (width + pad.0, lines * size * 1.3 + pad.1)
                });
                (scene.width > 0. && scene.height > 0.)
                    .then(|| Rc::new((diagram.kind, scene, born)))
            })
            .clone()
    }

    pub(super) fn formula(
        &mut self,
        key: BlockKey,
        tex: &str,
        display: bool,
        window: &Window,
    ) -> Option<Formula> {
        let ui = self.ui.clone();
        // Tern sets display math at 1.75 of the text around it.
        let size = self.feed_em() * if display { 1.75 } else { 1.2 };
        self.caches
            .formulas
            .entry(key)
            .or_insert_with(|| {
                let formula = math::parse(tex)?;
                let font = math_font(&ui, false, false);
                let id = window.text_system().resolve_font(&font);
                let unit = 100.;
                let metrics = math::Metrics {
                    ascent: f32::from(window.text_system().ascent(id, px(unit))) / unit,
                    descent: f32::from(window.text_system().descent(id, px(unit))).abs() / unit,
                };
                let layout = formula.layout(
                    if display {
                        math::Style::Display
                    } else {
                        math::Style::Inline
                    },
                    size,
                    metrics,
                    |text, size, italic| measure(window, text, math_font(&ui, italic, false), size),
                );
                (layout.width > 0.).then(|| Rc::new((layout, metrics)))
            })
            .clone()
    }

    /// Tern's figure: `⌗ mermaid · flowchart … Copy` over the drawn chart.
    pub(super) fn diagram_card(
        &self,
        key: BlockKey,
        diagram: &Diagram,
        source: &str,
        _: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let kind = match diagram.0 {
            Kind::Flowchart => "flowchart",
            Kind::Sequence => "sequence",
        };
        let id = key.0 * 1000 + key.2 as u64;
        let f = self.feed_em();
        let scene = &diagram.1;
        let widest = (scene.width * MAX_SCALE).min(f * 60.);
        self.figure()
            .child(self.figure_head(
                Some("icons/tern-chart.svg"),
                "mermaid",
                Some(kind),
                (source.to_owned(), id),
                cx,
            ))
            .child(
                div()
                    .flex()
                    .justify_center()
                    .px(px(f * 1.5))
                    .pt(px(f))
                    .pb(px(f * 1.1))
                    .child(
                        div()
                            .w_full()
                            .max_w(px(widest))
                            .aspect_ratio(scene.width / scene.height)
                            .child(chart(diagram.clone(), self.mono()).size_full()),
                    ),
            )
            .into_any_element()
    }
}

fn point_at(origin: Point<Pixels>, scale: f32, (x, y): (f32, f32)) -> Point<Pixels> {
    point(origin.x + px(x * scale), origin.y + px(y * scale))
}

/// `value`'s alpha multiplied by `opacity`.
fn faded(value: u32, opacity: f32) -> Hsla {
    let alpha = (value & 255) as f32 * opacity.clamp(0., 1.);
    color((value & 0xffffff00) | alpha.round() as u32).into()
}

struct Paint {
    fill: u32,
    edge: u32,
    radius: f32,
}

/// How a shape is filled and outlined; nodes that are peers take the
/// accents in turn (`hue`), so a diagram reads at a glance.
fn shape_paint(shape: &mermaid::Shape, hue: Option<u32>) -> Paint {
    let (fill, edge) = match (shape.kind, shape.tone) {
        (ShapeKind::Cluster, _) => (0xffffff05, 0xffffff14),
        (ShapeKind::Note, _) => (0x2a2616ff, 0xc9a42a66),
        (ShapeKind::Activation, _) => (0x1d212bff, tern::NODE_EDGE),
        (_, Tone::Accent) => (tern::NODE_FILL, 0x34e4c999),
        (_, Tone::Muted) => (tern::FIG, 0xffffff24),
        (_, Tone::Normal) => match hue {
            Some(hue) => (
                tern::blend(tern::NODE_FILL, hue, 0.12),
                tern::wash(hue, 0x99),
            ),
            None => (tern::NODE_FILL, tern::NODE_EDGE),
        },
    };
    let radius = match shape.kind {
        ShapeKind::Cluster => 6.,
        ShapeKind::Stadium => shape.height / 2.,
        ShapeKind::Circle => shape.width.min(shape.height) / 2.,
        ShapeKind::Rounded => 8.,
        _ => 4.,
    };
    Paint { fill, edge, radius }
}

fn polygon(window: &mut Window, points: &[Point<Pixels>], fill: Hsla, edge: Hsla, width: f32) {
    let Some((first, rest)) = points.split_first() else {
        return;
    };
    for (mut builder, ink) in [
        (PathBuilder::fill(), fill),
        (PathBuilder::stroke(px(width)), edge),
    ] {
        builder.move_to(*first);
        for point in rest {
            builder.line_to(*point);
        }
        builder.close();
        if let Ok(path) = builder.build() {
            window.paint_path(path, ink);
        }
    }
}

/// A shape grown to `grow` of its size about its centre, at `opacity`.
fn paint_shape(
    window: &mut Window,
    origin: Point<Pixels>,
    scale: f32,
    shape: &mermaid::Shape,
    grow: f32,
    opacity: f32,
    hue: Option<u32>,
) {
    let paint = shape_paint(shape, hue);
    let (cx, cy) = (shape.x + shape.width / 2., shape.y + shape.height / 2.);
    let (w, h) = (shape.width * grow, shape.height * grow);
    let (x, y) = (cx - w / 2., cy - h / 2.);
    let fill = faded(paint.fill, opacity);
    let edge = faded(paint.edge, opacity);
    let stroke = scale.max(1.);
    let bounds = Bounds::new(
        point_at(origin, scale, (x, y)),
        size(px(w * scale), px(h * scale)),
    );
    let boxed = |radius: f32| {
        quad(
            bounds,
            px(radius * grow * scale),
            fill,
            px(stroke),
            edge,
            BorderStyle::Solid,
        )
    };
    let at = |px_: f32, py_: f32| point_at(origin, scale, (x + px_, y + py_));
    match shape.kind {
        ShapeKind::Rect
        | ShapeKind::Activation
        | ShapeKind::Rounded
        | ShapeKind::Note
        | ShapeKind::Cluster
        | ShapeKind::Stadium
        | ShapeKind::Circle => window.paint_quad(boxed(paint.radius)),
        ShapeKind::Subroutine => {
            window.paint_quad(boxed(paint.radius));
            for inset in [6., w - 6.] {
                let mut line = PathBuilder::stroke(px(stroke));
                line.move_to(at(inset, 0.));
                line.line_to(at(inset, h));
                if let Ok(path) = line.build() {
                    window.paint_path(path, edge);
                }
            }
        }
        ShapeKind::Diamond => polygon(
            window,
            &[at(w / 2., 0.), at(w, h / 2.), at(w / 2., h), at(0., h / 2.)],
            fill,
            edge,
            stroke,
        ),
        ShapeKind::Hexagon => {
            let inset = (h / 2.).min(w / 4.);
            polygon(
                window,
                &[
                    at(inset, 0.),
                    at(w - inset, 0.),
                    at(w, h / 2.),
                    at(w - inset, h),
                    at(inset, h),
                    at(0., h / 2.),
                ],
                fill,
                edge,
                stroke,
            );
        }
        ShapeKind::Cylinder => {
            let cap = (h * 0.16).clamp(4., 10.);
            let outline = |builder: &mut PathBuilder| {
                builder.move_to(at(0., cap));
                builder.curve_to(at(w, cap), at(w / 2., -cap));
                builder.line_to(at(w, h - cap));
                builder.curve_to(at(0., h - cap), at(w / 2., h + cap));
                builder.close();
            };
            let mut body = PathBuilder::fill();
            outline(&mut body);
            if let Ok(path) = body.build() {
                window.paint_path(path, fill);
            }
            let mut rim = PathBuilder::stroke(px(stroke));
            outline(&mut rim);
            rim.move_to(at(0., cap));
            rim.curve_to(at(w, cap), at(w / 2., 3. * cap));
            if let Ok(path) = rim.build() {
                window.paint_path(path, edge);
            }
        }
        ShapeKind::Lifeline => {
            let mut line =
                PathBuilder::stroke(px(stroke)).dash_array(&[px(4. * scale), px(4. * scale)]);
            line.move_to(point_at(origin, scale, (shape.x, shape.y)));
            line.line_to(point_at(origin, scale, (shape.x, shape.y + shape.height)));
            if let Ok(path) = line.build() {
                window.paint_path(path, faded(tern::WIRE & 0xffffff99, opacity));
            }
        }
    }
}

/// The first `drawn` fraction of a polyline, measured along its length.
fn partial(points: &[(f32, f32)], drawn: f32) -> Vec<(f32, f32)> {
    if drawn >= 1. {
        return points.to_vec();
    }
    let lengths: Vec<f32> = points
        .windows(2)
        .map(|pair| ((pair[1].0 - pair[0].0).powi(2) + (pair[1].1 - pair[0].1).powi(2)).sqrt())
        .collect();
    let mut left = lengths.iter().sum::<f32>() * drawn.max(0.);
    let mut out = Vec::with_capacity(points.len());
    for (pair, length) in points.windows(2).zip(&lengths) {
        if out.is_empty() {
            out.push(pair[0]);
        }
        if left >= *length {
            out.push(pair[1]);
            left -= length;
        } else {
            let t = if *length > 0. { left / length } else { 0. };
            out.push((
                pair[0].0 + (pair[1].0 - pair[0].0) * t,
                pair[0].1 + (pair[1].1 - pair[0].1) * t,
            ));
            break;
        }
    }
    out
}

/// A connector drawn to `drawn` of its length, and its arrowheads at
/// `tips` opacity.
fn paint_line(
    window: &mut Window,
    origin: Point<Pixels>,
    scale: f32,
    line: &mermaid::Line,
    drawn: f32,
    tips: f32,
) {
    let visible = partial(&line.points, drawn);
    let points: Vec<Point<Pixels>> = visible
        .iter()
        .map(|p| point_at(origin, scale, *p))
        .collect();
    if points.len() < 2 {
        return;
    }
    let width = match line.style {
        Stroke::Thick => 2.4,
        Stroke::Solid | Stroke::Dotted => 1.4,
    } * scale;
    let wire: Hsla = color(tern::WIRE).into();
    let mut builder = PathBuilder::stroke(px(width));
    if line.style == Stroke::Dotted {
        builder = builder.dash_array(&[px(3. * scale), px(3. * scale)]);
    }
    builder.move_to(points[0]);
    for point in &points[1..] {
        builder.line_to(*point);
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, wire);
    }
    let tip = faded(tern::WIRE, tips);
    if let [.., from, to] = line.points.as_slice() {
        arrowhead(window, origin, scale, *from, *to, line.arrow_end, tip);
    }
    if let [to, from, ..] = line.points.as_slice() {
        arrowhead(window, origin, scale, *from, *to, line.arrow_start, tip);
    }
}

/// Tern's arrowhead: 8 units long and 9 wide, its tip on the target.
fn arrowhead(
    window: &mut Window,
    origin: Point<Pixels>,
    scale: f32,
    from: (f32, f32),
    tip: (f32, f32),
    arrow: Arrow,
    ink: Hsla,
) {
    let (dx, dy) = (tip.0 - from.0, tip.1 - from.1);
    let length = (dx * dx + dy * dy).sqrt();
    if length <= f32::EPSILON || arrow == Arrow::None {
        return;
    }
    let (ux, uy) = (dx / length, dy / length);
    let (nx, ny) = (-uy, ux);
    let at = |along: f32, across: f32| {
        point_at(
            origin,
            scale,
            (
                tip.0 - ux * along + nx * across,
                tip.1 - uy * along + ny * across,
            ),
        )
    };
    let stroke = 1.4 * scale;
    match arrow {
        Arrow::None => {}
        Arrow::Filled => {
            let mut head = PathBuilder::fill();
            head.move_to(at(0., 0.));
            head.line_to(at(8., 4.5));
            head.line_to(at(8., -4.5));
            head.close();
            if let Ok(path) = head.build() {
                window.paint_path(path, ink);
            }
        }
        Arrow::Open => {
            let mut head = PathBuilder::stroke(px(stroke));
            head.move_to(at(8., 4.5));
            head.line_to(at(0., 0.));
            head.line_to(at(8., -4.5));
            if let Ok(path) = head.build() {
                window.paint_path(path, ink);
            }
        }
        Arrow::Cross => {
            let mut head = PathBuilder::stroke(px(stroke));
            head.move_to(at(7., 3.5));
            head.line_to(at(0., -3.5));
            head.move_to(at(7., -3.5));
            head.line_to(at(0., 3.5));
            if let Ok(path) = head.build() {
                window.paint_path(path, ink);
            }
        }
        Arrow::Circle => {
            let r = 3.5 * scale;
            let centre = at(3.5, 0.);
            window.paint_quad(quad(
                Bounds::new(
                    point(centre.x - px(r), centre.y - px(r)),
                    size(px(r * 2.), px(r * 2.)),
                ),
                px(r),
                ink,
                px(0.),
                ink,
                BorderStyle::Solid,
            ));
        }
    }
}

/// Text centred on `centre`, one line per `\n`.
#[allow(clippy::too_many_arguments)]
fn paint_label(
    window: &mut Window,
    cx: &mut App,
    font: &Font,
    text: &str,
    centre: Point<Pixels>,
    size: f32,
    ink: Hsla,
) {
    let lines: Vec<&str> = text.split('\n').collect();
    let height = size * 1.3;
    let top = centre.y - px(height * lines.len() as f32 / 2.);
    for (index, line) in lines.into_iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        let run = TextRun {
            len: line.len(),
            font: font.clone(),
            color: ink,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let shaped = window.text_system().shape_line(
            SharedString::from(line.to_owned()),
            px(size),
            &[run],
            None,
        );
        let origin = point(
            centre.x - shaped.width / 2.,
            top + px(height * index as f32),
        );
        let _ = shaped.paint(origin, px(height), TextAlign::Left, None, window, cx);
    }
}

/// A laid-out diagram scaled to its bounds. When it is fresh, nodes pop in
/// 90 ms apart, then edges draw from their source, their tips fade in, and
/// their labels appear: Tern's timings.
fn chart(diagram: Diagram, font: Font) -> Canvas<()> {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, cx| {
            let (_, scene, born) = &*diagram;
            let scale = f32::from(bounds.size.width) / scene.width.max(1.);
            let origin = bounds.origin;
            let mut moving = false;
            let mut clock = |delay_ms: u64, duration_ms: u64| {
                let t = progress(
                    *born,
                    Duration::from_millis(delay_ms),
                    Duration::from_millis(duration_ms),
                );
                moving |= t.is_some();
                t.unwrap_or(1.)
            };
            // Containers and lifelines behind everything, fading in first.
            let backdrop = tern::ease(clock(0, 400));
            for shape in scene
                .shapes
                .iter()
                .filter(|shape| matches!(shape.kind, ShapeKind::Cluster | ShapeKind::Lifeline))
            {
                paint_shape(window, origin, scale, shape, 1., backdrop, None);
            }
            for (index, line) in scene.lines.iter().enumerate() {
                let step = index.min(8) as u64 * 100;
                let drawn = tern::ease_out(clock(320 + step, 420));
                let tips = tern::ease(clock(640 + step, 200));
                paint_line(window, origin, scale, line, drawn, tips);
            }
            for (index, shape) in scene
                .shapes
                .iter()
                .filter(|shape| !matches!(shape.kind, ShapeKind::Cluster | ShapeKind::Lifeline))
                .enumerate()
            {
                let t = clock(index.min(8) as u64 * 90, 887);
                let grow = 0.55 + 0.45 * tern::pop(t);
                let opacity = (t / 0.3).min(1.);
                paint_shape(
                    window,
                    origin,
                    scale,
                    shape,
                    grow,
                    opacity,
                    Some(tern::accent(index)),
                );
            }
            let labels = tern::ease(clock(520, 260));
            let node_pop = tern::pop(clock(0, 887));
            for label in &scene.labels {
                let centre = point_at(origin, scale, (label.x, label.y));
                let (size, ink, opacity) = match label.font {
                    LabelFont::Node => (NODE_TEXT, tern::NODE_INK, (node_pop).min(1.)),
                    LabelFont::Edge => (EDGE_TEXT, tern::INK_2, labels),
                };
                if label.backdrop {
                    let width = label
                        .text
                        .split('\n')
                        .map(|line| measure(window, line, font.clone(), size * scale))
                        .fold(0., f32::max)
                        + LABEL_PAD.0 * scale;
                    let height =
                        (label.text.split('\n').count() as f32 * size * 1.3 + LABEL_PAD.1) * scale;
                    window.paint_quad(quad(
                        Bounds::new(
                            point(centre.x - px(width / 2.), centre.y - px(height / 2.)),
                            size_of(width, height),
                        ),
                        px(3. * scale),
                        faded(tern::FIG, opacity),
                        px(scale.max(1.)),
                        faded(tern::LABEL_EDGE, opacity),
                        BorderStyle::Solid,
                    ));
                }
                paint_label(
                    window,
                    cx,
                    &font,
                    &label.text,
                    centre,
                    size * scale,
                    faded(ink, opacity),
                );
            }
            if moving {
                window.request_animation_frame();
            }
        },
    )
}

fn size_of(width: f32, height: f32) -> Size<Pixels> {
    size(px(width), px(height))
}

/// Typeset math: glyph runs as text at the engine's baselines, rules and
/// radicals painted. `reveal` is Tern's ink-in: the formula uncovers from
/// left to right while it firms from faint.
pub(super) fn formula(
    layout: &math::Layout,
    metrics: math::Metrics,
    ui: &FontConfig,
    reveal: f32,
) -> AnyElement {
    let height = layout.ascent + layout.descent;
    let opacity = 0.4 + 0.6 * reveal;
    let mut glyphs = Vec::new();
    let mut strokes = Vec::new();
    for item in &layout.items {
        match item {
            math::Item::Glyphs {
                text,
                x,
                baseline,
                size,
                italic,
                bold,
            } => {
                // GPUI centres the font's ascent and descent in the line box
                // and sets the baseline below the ascent.
                let padding = (1. - metrics.ascent - metrics.descent) * size / 2.;
                let top = layout.ascent + baseline - padding - metrics.ascent * size;
                glyphs.push(
                    div()
                        .absolute()
                        .left(px(*x))
                        .top(px(top))
                        .h(px(*size))
                        .text_size(px(*size))
                        .line_height(px(*size))
                        .whitespace_nowrap()
                        .font(math_font(ui, *italic, *bold))
                        .child(text.clone())
                        .into_any_element(),
                );
            }
            math::Item::Rule { .. } | math::Item::Radical { .. } => strokes.push(item.clone()),
        }
    }
    let ascent = layout.ascent;
    let width = layout.width;
    div()
        .relative()
        .flex_none()
        .w(px(width))
        .h(px(height))
        .text_color(color(tern::INK))
        .opacity(opacity)
        .child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    let origin = point(bounds.origin.x, bounds.origin.y + px(ascent));
                    let ink: Hsla = color(tern::INK).into();
                    for item in &strokes {
                        match item {
                            math::Item::Rule {
                                x,
                                y,
                                width,
                                height,
                            } => window.paint_quad(fill(
                                Bounds::new(
                                    point_at(origin, 1., (*x, *y)),
                                    size(px(*width), px(height.max(0.8))),
                                ),
                                ink,
                            )),
                            math::Item::Radical {
                                x,
                                y,
                                width,
                                height,
                            } => {
                                // The engine's contract: a short rise, the
                                // V's bottom, then up to where the overbar starts.
                                let mut check = PathBuilder::stroke(px(1.1));
                                let rise = (height * 0.45).min(width * 0.9);
                                check.move_to(point_at(origin, 1., (*x, y + height - rise)));
                                check.line_to(point_at(origin, 1., (x + width * 0.4, y + height)));
                                check.line_to(point_at(origin, 1., (x + width, *y)));
                                if let Ok(path) = check.build() {
                                    window.paint_path(path, ink);
                                }
                            }
                            math::Item::Glyphs { .. } => {}
                        }
                    }
                },
            )
            .absolute()
            .size_full(),
        )
        .children(glyphs)
        .when(reveal < 1., |math| {
            // The uncovered edge: the rest is still behind the pane.
            math.child(
                div()
                    .absolute()
                    .top(px(-height * 0.2))
                    .bottom(px(-height * 0.2))
                    .left(px(width * reveal))
                    .right_0()
                    .bg(color(tern::TILE)),
            )
        })
        .into_any_element()
}
