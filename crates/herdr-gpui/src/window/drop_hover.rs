//! What a file drag over the terminal shows before it lands: the pane or popup
//! it would drop into, framed, and a card in it saying what the drop will do,
//! with a preview when the first file is an image. Both come from the drop's
//! own hit test and [`DropAction`], so the signal never disagrees with the drop.
//!
//! The view renders again only when the target changes or a preview arrives,
//! not on every pointer move: the card sits in its target, and the platform
//! already draws the dragged files under the pointer. Motion is a function of
//! time, so frames are requested only while something is still moving.

use super::{
    HerdrWindow,
    file_drop::DropAction,
    image_source::{self, Source, Thumbnail},
    transfers::byte_size,
};
use crate::{
    config::corners,
    fonts::StyledFont,
    motion::{self, ENTER, LEAVE},
    terminal::InputTarget,
};
use gpui::{prelude::*, *};
use std::{path::PathBuf, time::Instant};

/// How far the card sinks while hidden, rising into place as it appears.
const RISE: f32 = 8.;
/// The card's widest; a narrower target gets a card that fits inside it.
const CARD_WIDTH: f32 = 300.;
/// Room the card leaves between itself and the frame on each side.
const CARD_MARGIN: f32 = 12.;
/// A target smaller than this shows only its frame.
const MIN_CARD_WIDTH: f32 = 160.;
const MIN_CARD_HEIGHT: f32 = 72.;
/// A target shorter than this shows the card without its picture.
const MIN_PREVIEW_TARGET_HEIGHT: f32 = 280.;
const PREVIEW_HEIGHT: f32 = 168.;

#[derive(Default)]
pub(crate) struct DropHover {
    /// The files of the drag over this window, from its first move to its end.
    files: Option<DraggedFiles>,
    /// Where they would drop now.
    over: Option<Over>,
    /// The last frame, fading after the drag moved off, dropped, or left.
    fading: Option<Fading>,
}

struct DraggedFiles {
    paths: Vec<PathBuf>,
    /// The first file's image format, such as `png`, when it is one the image
    /// bridge recognizes.
    format: Option<&'static str>,
    preview: Preview,
}

enum Preview {
    /// Nothing to show: the first file is not an image, or it could not be
    /// read or decoded, and the card names it without a picture.
    Absent,
    /// Read and decoded on the background executor; dropping it abandons the work.
    Loading { _task: Task<()> },
    Ready {
        thumbnail: Thumbnail,
        since: Instant,
    },
}

struct Over {
    target: InputTarget,
    /// The target's cells, relative to the terminal's paint origin.
    bounds: Bounds<Pixels>,
    /// Where and how visible the frame was when it started toward `bounds`.
    from: Bounds<Pixels>,
    from_opacity: f32,
    since: Instant,
}

impl Over {
    fn progress(&self, now: Instant) -> f32 {
        motion::progress(self.since, now, ENTER).unwrap_or(1.)
    }

    fn frame(&self, now: Instant) -> Bounds<Pixels> {
        mix(self.from, self.bounds, self.progress(now))
    }

    fn opacity(&self, now: Instant) -> f32 {
        self.from_opacity + (1. - self.from_opacity) * self.progress(now)
    }
}

struct Fading {
    bounds: Bounds<Pixels>,
    opacity: f32,
    since: Instant,
    /// The ended drag's files, so its card fades with the frame.
    files: Option<DraggedFiles>,
}

impl Fading {
    fn opacity(&self, now: Instant) -> Option<f32> {
        motion::progress(self.since, now, LEAVE).map(|k| self.opacity * (1. - k))
    }
}

/// One frame of the hover, as render draws it.
struct Shown<'a> {
    bounds: Bounds<Pixels>,
    opacity: f32,
    files: Option<&'a DraggedFiles>,
}

impl DropHover {
    /// Frames `hit`, or fades the frame when there is none. Returns whether
    /// anything visible changed, so pointer moves within a target stay free.
    fn aim(&mut self, hit: Option<(InputTarget, Bounds<Pixels>)>, now: Instant) -> bool {
        let Some((target, bounds)) = hit else {
            return self.leave(now);
        };
        if self
            .over
            .as_ref()
            .is_some_and(|over| over.target == target && over.bounds == bounds)
        {
            return false;
        }
        // Start from wherever the frame is drawn now, so moving between
        // targets glides rather than jumps; a fresh frame grows from itself.
        let (from, from_opacity) = match (&self.over, &self.fading) {
            (Some(over), _) => (over.frame(now), over.opacity(now)),
            (None, Some(fading)) => (fading.bounds, fading.opacity(now).unwrap_or(0.)),
            (None, None) => (bounds, 0.),
        };
        self.fading = None;
        self.over = Some(Over {
            target,
            bounds,
            from,
            from_opacity,
            since: now,
        });
        true
    }

    /// Fades the frame out; its card stays while the drag goes on.
    fn leave(&mut self, now: Instant) -> bool {
        let Some(over) = self.over.take() else {
            return false;
        };
        self.fading = Some(Fading {
            bounds: over.frame(now),
            opacity: over.opacity(now),
            since: now,
            files: None,
        });
        true
    }

    /// The drag dropped or left the window. Returns whether it was showing.
    fn end(&mut self, now: Instant) -> bool {
        let showing = self.files.is_some() || self.over.is_some();
        self.leave(now);
        if let Some(fading) = &mut self.fading {
            fading.files = self.files.take();
        }
        self.files = None;
        showing
    }

    /// Forgets finished motion and says whether any is still going.
    fn tick(&mut self, now: Instant) -> bool {
        if self
            .fading
            .as_ref()
            .is_some_and(|fading| fading.opacity(now).is_none())
        {
            self.fading = None;
        }
        let picture = |files: Option<&DraggedFiles>| {
            files.is_some_and(|files| {
                matches!(files.preview, Preview::Ready { since, .. }
                    if motion::progress(since, now, ENTER).is_some())
            })
        };
        self.fading.is_some()
            || self
                .over
                .as_ref()
                .is_some_and(|over| over.progress(now) < 1.)
            || picture(self.files.as_ref())
    }

    fn shown(&self, now: Instant) -> Option<Shown<'_>> {
        if let Some(over) = &self.over {
            return Some(Shown {
                bounds: over.frame(now),
                opacity: over.opacity(now),
                files: self.files.as_ref(),
            });
        }
        let fading = self.fading.as_ref()?;
        Some(Shown {
            bounds: fading.bounds,
            opacity: fading.opacity(now)?,
            files: fading.files.as_ref().or(self.files.as_ref()),
        })
    }
}

#[cfg(test)]
impl DropHover {
    pub(super) fn target(&self) -> Option<&InputTarget> {
        self.over.as_ref().map(|over| &over.target)
    }

    pub(super) fn dragging(&self) -> bool {
        self.files.is_some()
    }

    pub(super) fn previewed(&self) -> bool {
        self.files.as_ref().is_some_and(|files| {
            matches!(&files.preview, Preview::Ready { thumbnail, .. } if thumbnail.image.is_some())
        })
    }
}

fn mix(from: Bounds<Pixels>, to: Bounds<Pixels>, k: f32) -> Bounds<Pixels> {
    let m = |a: Pixels, b: Pixels| a + (b - a) * k;
    Bounds::new(
        point(m(from.origin.x, to.origin.x), m(from.origin.y, to.origin.y)),
        size(
            m(from.size.width, to.size.width),
            m(from.size.height, to.size.height),
        ),
    )
}

/// The card's promise, in the words of what the drop then does.
fn action_text(action: DropAction, count: usize, host: &str) -> String {
    match (action, count) {
        (DropAction::PastePaths, 1) => "Drop to paste its path".into(),
        (DropAction::PastePaths, count) => format!("Drop to paste {count} paths"),
        (DropAction::SendImage, _) => format!("Drop to send to {host}"),
        (DropAction::CopyFiles, 1) => format!("Drop to copy to {host}"),
        (DropAction::CopyFiles, count) => format!("Drop to copy {count} files to {host}"),
    }
}

/// `PNG · 1920 × 1080 · 2.4 MiB`, with whatever of it is known so far.
fn image_details(format: &str, thumbnail: Option<&Thumbnail>) -> String {
    let mut details = vec![if format == "jpg" {
        "JPEG".to_owned()
    } else {
        format.to_ascii_uppercase()
    }];
    if let Some(thumbnail) = thumbnail {
        if let Some((width, height)) = thumbnail.dimensions {
            details.push(format!("{width} \u{d7} {height}"));
        }
        details.push(byte_size(thumbnail.bytes));
    }
    details.join(" \u{b7} ")
}

impl HerdrWindow {
    /// Follows an external file drag across the window. GPUI reports every
    /// pointer move during the drag here, over the terminal or not.
    pub(super) fn file_drag_move(
        &mut self,
        event: &DragMoveEvent<ExternalPaths>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = Instant::now();
        let same = self
            .drop_hover
            .files
            .as_ref()
            .is_some_and(|files| files.paths == event.drag(cx).paths());
        if !same {
            let paths = event.drag(cx).paths().to_vec();
            self.drop_hover.files = Some(self.dragged_files(paths, cx));
        }
        let hit = self
            .drop_hover
            .files
            .as_ref()
            .filter(|files| !files.paths.is_empty())
            .and_then(|_| self.file_drop_hit(event.event.position))
            .map(|hit| (hit.target, hit.bounds));
        if self.drop_hover.aim(hit, now) || !same {
            cx.notify();
        }
    }

    pub(super) fn end_drop_hover(&mut self, cx: &mut Context<Self>) {
        if self.drop_hover.end(Instant::now()) {
            cx.notify();
        }
    }

    /// An exit or drop clears GPUI's drag and redraws the window; there is no
    /// separate event for it, so render tells the hover here. Returns whether
    /// anything is still moving.
    pub(super) fn settle_drop_hover(&mut self, dragging: bool, now: Instant) -> bool {
        if !dragging {
            self.drop_hover.end(now);
        }
        self.drop_hover.tick(now)
    }

    fn dragged_files(&self, paths: Vec<PathBuf>, cx: &mut Context<Self>) -> DraggedFiles {
        let Some(Source::File { path, extension }) =
            paths.first().and_then(|path| image_source::from_path(path))
        else {
            return DraggedFiles {
                paths,
                format: None,
                preview: Preview::Absent,
            };
        };
        let load = cx.background_spawn(async move { image_source::thumbnail(&path) });
        let task = cx.spawn(async move |this, cx| {
            let thumbnail = load.await;
            // Replacing or ending the drag drops this task, so a result that
            // arrives belongs to the files still shown.
            this.update(cx, |this, cx| {
                if let Some(files) = &mut this.drop_hover.files {
                    files.preview = match thumbnail {
                        Ok(thumbnail) => Preview::Ready {
                            thumbnail,
                            since: Instant::now(),
                        },
                        Err(_) => Preview::Absent,
                    };
                    cx.notify();
                }
            })
            .ok();
        });
        DraggedFiles {
            paths,
            format: Some(extension),
            preview: Preview::Loading { _task: task },
        }
    }

    /// The frame and card over the target, positioned inside the terminal
    /// element whose content starts `gap` pixels from its left edge.
    pub(super) fn render_drop_hover(
        &mut self,
        gap: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let now = Instant::now();
        if self.settle_drop_hover(cx.has_active_drag(), now) {
            window.request_animation_frame();
        }
        let shown = self.drop_hover.shown(now)?;
        let accent = self.theme.primary();
        let bounds = shown.bounds;
        let card = shown
            .files
            .filter(|_| {
                bounds.size.width >= px(MIN_CARD_WIDTH) && bounds.size.height >= px(MIN_CARD_HEIGHT)
            })
            .map(|files| {
                let picture = bounds.size.height >= px(MIN_PREVIEW_TARGET_HEIGHT);
                self.render_drop_card(files, bounds.size.width, picture, now)
                    .relative()
                    .top(px((1. - shown.opacity) * RISE))
            });
        Some(
            div()
                .debug_selector(|| "drop-hover".into())
                .absolute()
                .left(px(gap) + bounds.origin.x)
                .top(bounds.origin.y)
                .w(bounds.size.width)
                .h(bounds.size.height)
                .opacity(shown.opacity)
                .rounded(px(corners::PANEL))
                .border_2()
                .border_color(rgb(accent))
                .bg(Hsla::from(rgb(accent)).opacity(0.1))
                .flex()
                .items_center()
                .justify_center()
                .overflow_hidden()
                .children(card)
                .into_any_element(),
        )
    }

    fn render_drop_card(
        &self,
        files: &DraggedFiles,
        target_width: Pixels,
        picture: bool,
        now: Instant,
    ) -> Div {
        let theme = &self.theme;
        let accent = self.theme.primary();
        let count = files.paths.len();
        let name = files
            .paths
            .first()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let thumbnail = match &files.preview {
            Preview::Ready { thumbnail, since } => Some((thumbnail, *since)),
            _ => None,
        };
        let details = files
            .format
            .map(|format| image_details(format, thumbnail.map(|(thumbnail, _)| thumbnail)));
        let action = DropAction::choose(self.accepts_remote_images(), &files.paths);
        let host = &self.endpoints[self.selected_endpoint].label;
        // A slot waits for the picture while it loads, so the card does not
        // grow under the pointer; an image too large to decode keeps its size
        // and says so in its details instead.
        let slot = picture
            .then(|| match (&files.preview, thumbnail) {
                (_, Some((thumbnail, since))) => Some(thumbnail.image.clone().map(|image| {
                    let shown = motion::progress(since, now, ENTER).unwrap_or(1.);
                    img(image)
                        .size_full()
                        .object_fit(ObjectFit::Contain)
                        .opacity(shown)
                        .into_any_element()
                })),
                (Preview::Loading { .. }, None) => Some(None),
                _ => None,
            })
            .flatten();
        div()
            .debug_selector(|| "drop-card".into())
            .w((target_width - px(2. * CARD_MARGIN)).min(px(CARD_WIDTH)))
            .rounded(px(corners::PANEL))
            .border_1()
            .border_color(rgb(theme.active))
            .bg(rgb(theme.surface))
            .shadow_lg()
            .overflow_hidden()
            .text_color(rgb(theme.foreground))
            .text_font(&self.config.ui)
            .text_size(px(self.config.ui.size))
            .flex()
            .flex_col()
            .when_some(slot, |card, image| {
                card.child(
                    div()
                        .debug_selector(|| "drop-preview".into())
                        .h(px(PREVIEW_HEIGHT))
                        .p(px(8.))
                        .bg(rgb(theme.background))
                        .flex()
                        .items_center()
                        .justify_center()
                        .children(image),
                )
            })
            .child(
                div()
                    .px(px(10.))
                    .pt(px(8.))
                    .pb(px(6.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .child(div().min_w_0().truncate().child(name))
                            .when(count > 1, |row| {
                                row.child(
                                    div()
                                        .flex_none()
                                        .text_color(rgb(theme.muted))
                                        .child(format!("+{}", count - 1)),
                                )
                            }),
                    )
                    .when_some(details, |column, details| {
                        column.child(div().truncate().text_color(rgb(theme.muted)).child(details))
                    }),
            )
            .child(
                div()
                    .debug_selector(|| "drop-action".into())
                    .px(px(10.))
                    .py(px(7.))
                    .border_t_1()
                    .border_color(rgb(theme.active))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_color(rgb(accent))
                    .child(
                        div()
                            .size(px(6.))
                            .flex_none()
                            .rounded_full()
                            .bg(rgb(accent)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(action_text(action, count, host)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    // Not `super::*`: its `gpui::*` would shadow `#[test]` with `gpui::test`.
    use super::{
        DraggedFiles, DropAction, DropHover, ENTER, InputTarget, LEAVE, Preview, Thumbnail,
        action_text, image_details,
    };
    use gpui::{Bounds, Pixels, point, px, size};
    use std::{
        path::PathBuf,
        time::{Duration, Instant},
    };

    fn rect(x: f32, width: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(0.)), size(px(width), px(300.)))
    }

    fn pane(id: &str) -> InputTarget {
        InputTarget::Pane(id.into())
    }

    #[test]
    fn a_fresh_frame_fades_in_in_place_and_then_stops_moving() {
        let start = Instant::now();
        let mut hover = DropHover::default();
        assert!(hover.aim(Some((pane("a"), rect(0., 400.))), start));
        let shown = hover.shown(start).unwrap();
        assert_eq!(shown.bounds, rect(0., 400.));
        assert_eq!(shown.opacity, 0.);
        assert!(hover.tick(start));
        let midway = hover.shown(start + ENTER / 2).unwrap().opacity;
        assert!(midway > 0.5 && midway < 1., "{midway}");
        let settled = start + ENTER;
        assert_eq!(hover.shown(settled).unwrap().opacity, 1.);
        assert!(!hover.tick(settled));
    }

    #[test]
    fn moves_within_a_target_change_nothing() {
        let start = Instant::now();
        let mut hover = DropHover::default();
        hover.aim(Some((pane("a"), rect(0., 400.))), start);
        assert!(!hover.aim(Some((pane("a"), rect(0., 400.))), start + ENTER));
        assert!(!hover.tick(start + ENTER));
    }

    #[test]
    fn switching_targets_glides_from_where_the_frame_is_drawn() {
        let start = Instant::now();
        let mut hover = DropHover::default();
        hover.aim(Some((pane("a"), rect(0., 400.))), start);
        let switch = start + ENTER * 2;
        assert!(hover.aim(Some((pane("b"), rect(400., 400.))), switch));
        let shown = hover.shown(switch).unwrap();
        assert_eq!(shown.bounds, rect(0., 400.));
        assert_eq!(shown.opacity, 1.);
        let between = hover.shown(switch + ENTER / 2).unwrap().bounds.origin.x;
        assert!(between > px(200.) && between < px(400.), "{between:?}");
        assert_eq!(
            hover.shown(switch + ENTER).unwrap().bounds,
            rect(400., 400.)
        );
    }

    #[test]
    fn leaving_fades_out_then_forgets_and_reentering_starts_from_the_fade() {
        let start = Instant::now();
        let mut hover = DropHover::default();
        hover.aim(Some((pane("a"), rect(0., 400.))), start);
        let left = start + ENTER;
        assert!(hover.aim(None, left));
        assert!(!hover.aim(None, left));
        assert!(hover.over.is_none());
        let fading = hover.shown(left + LEAVE / 2).unwrap().opacity;
        assert!(fading > 0. && fading < 1., "{fading}");
        let back = left + LEAVE / 2;
        hover.aim(Some((pane("b"), rect(400., 400.))), back);
        assert!(hover.fading.is_none());
        assert_eq!(hover.shown(back).unwrap().opacity, fading);
        assert_eq!(hover.shown(back).unwrap().bounds, rect(0., 400.));
        hover.aim(None, back + ENTER);
        let gone = back + ENTER + LEAVE;
        assert!(!hover.tick(gone));
        assert!(hover.shown(gone).is_none());
    }

    #[test]
    fn ending_keeps_the_card_on_the_fading_frame_only() {
        let start = Instant::now();
        let mut hover = DropHover {
            files: Some(DraggedFiles {
                paths: vec![PathBuf::from("/tmp/a.txt")],
                format: None,
                preview: Preview::Absent,
            }),
            ..Default::default()
        };
        hover.aim(Some((pane("a"), rect(0., 400.))), start);
        assert!(hover.end(start + ENTER));
        assert!(hover.files.is_none());
        assert!(hover.shown(start + ENTER).unwrap().files.is_some());
        assert!(!hover.end(start + ENTER));
        let after = start + ENTER + LEAVE + Duration::from_millis(1);
        assert!(!hover.tick(after));
        assert!(hover.shown(after).is_none());
    }

    #[test]
    fn the_card_promises_what_the_drop_does() {
        use DropAction::*;
        for (action, count, expected) in [
            (PastePaths, 1, "Drop to paste its path"),
            (PastePaths, 3, "Drop to paste 3 paths"),
            (SendImage, 1, "Drop to send to devbox"),
            (CopyFiles, 1, "Drop to copy to devbox"),
            (CopyFiles, 2, "Drop to copy 2 files to devbox"),
        ] {
            assert_eq!(action_text(action, count, "devbox"), expected);
        }
    }

    #[test]
    fn image_details_grow_as_the_preview_arrives() {
        assert_eq!(image_details("png", None), "PNG");
        let thumbnail = Thumbnail {
            image: None,
            dimensions: Some((1920, 1080)),
            bytes: 2_516_582,
        };
        assert_eq!(
            image_details("jpg", Some(&thumbnail)),
            "JPEG \u{b7} 1920 \u{d7} 1080 \u{b7} 2.4 MiB"
        );
        let unknown = Thumbnail {
            dimensions: None,
            ..thumbnail
        };
        assert_eq!(image_details("webp", Some(&unknown)), "WEBP \u{b7} 2.4 MiB");
    }
}
