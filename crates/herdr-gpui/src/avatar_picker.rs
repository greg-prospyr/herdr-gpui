//! Choosing a space's avatar from its menu: a shape, a hue, or back to the
//! default. A choice applies at once and is remembered by this app
//! only (see `pixel_avatar::AvatarStore`).

use crate::{
    HerdrWindow,
    pixel_avatar::{self, AvatarStore, AvatarStyle, Hue, Images, Shape},
    spaces::SpaceKey,
};
use gpui::{prelude::*, *};
use std::{cell::RefCell, rc::Rc};

const TILE: f32 = 36.;
const TILE_SPRITE: f32 = 32.;
const SWATCH: f32 = 20.;
const PREVIEW: f32 = 64.;

/// A buddy painted still, `side` pixels square.
fn still(images: &Rc<RefCell<Images>>, style: AvatarStyle, side: f32) -> impl IntoElement {
    let images = images.clone();
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            pixel_avatar::paint_still(window, &mut images.borrow_mut(), bounds, style)
        },
    )
    .size(px(side))
    .flex_none()
}

impl HerdrWindow {
    /// The space the open menu is for, on the endpoint the menu opened on.
    fn avatar_target(&self) -> Option<SpaceKey> {
        let id = self.workspace_menu_target()?;
        Some(SpaceKey::new(
            &self.endpoints[self.selected_endpoint].id,
            id,
        ))
    }

    fn choose_avatar(&mut self, style: Option<AvatarStyle>, cx: &mut Context<Self>) {
        let Some(key) = self.avatar_target() else {
            return;
        };
        AvatarStore::choose(cx, key, style);
        cx.notify();
    }

    pub(crate) fn render_avatar_picker(&self, cx: &mut Context<Self>) -> Div {
        let Some(key) = self.avatar_target() else {
            return div();
        };
        let key = &key;
        let theme = &self.theme;
        let accent = theme.primary();
        let current = AvatarStore::style(cx, key);
        let custom = cx
            .try_global::<AvatarStore>()
            .and_then(|store| store.chosen(key))
            .is_some();
        let images = self.space_avatars.images();
        let label = |text: &'static str| div().text_color(rgb(theme.muted)).child(text);
        let tile = |id: ElementId, selected: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(crate::config::corners::CONTROL))
                .border_1()
                .border_color(rgb(if selected { accent } else { theme.surface }))
                .when(selected, |tile| tile.bg(rgb(theme.active)))
                .hover(|tile| tile.bg(rgb(theme.active)))
                .cursor_pointer()
        };
        let shapes = Shape::ALL.into_iter().enumerate().map(|(index, shape)| {
            let style = AvatarStyle { shape, ..current };
            tile(("avatar-shape", index).into(), shape == current.shape)
                .debug_selector(move || format!("avatar-shape-{index}"))
                .size(px(TILE))
                .child(still(&images, style, TILE_SPRITE))
                .on_click(cx.listener(move |this, _, _, cx| this.choose_avatar(Some(style), cx)))
        });
        let hues = Hue::ALL.into_iter().enumerate().map(|(index, hue)| {
            let style = AvatarStyle { hue, ..current };
            tile(("avatar-hue", index).into(), hue == current.hue)
                .debug_selector(move || format!("avatar-hue-{index}"))
                .size(px(SWATCH + 6.))
                .rounded_full()
                .child(div().size(px(SWATCH)).rounded_full().bg(rgb(hue.base())))
                .on_click(cx.listener(move |this, _, _, cx| this.choose_avatar(Some(style), cx)))
        });
        div()
            .debug_selector(|| "avatar-picker".into())
            .p(px(10.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(still(&images, current, PREVIEW))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .child(div().font_weight(FontWeight::SEMIBOLD).child("Avatar"))
                            .child(div().text_color(rgb(theme.muted)).child(format!(
                                "{} \u{b7} {}",
                                current.shape.label(),
                                current.hue.label()
                            )))
                            .child(
                                div()
                                    .text_color(rgb(theme.muted))
                                    .child("Remembered by this app only."),
                            ),
                    ),
            )
            .child(label("Shape"))
            .child(div().flex().flex_wrap().gap(px(4.)).children(shapes))
            .child(label("Color"))
            .child(div().flex().flex_wrap().gap(px(4.)).children(hues))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        div()
                            .id("avatar-default")
                            .debug_selector(|| "avatar-default".into())
                            .p(px(6.))
                            .rounded(px(crate::config::corners::CONTROL))
                            .when(custom, |button| {
                                button
                                    .cursor_pointer()
                                    .hover(|button| button.bg(rgb(theme.active)))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.choose_avatar(None, cx)),
                                    )
                            })
                            .when(!custom, |button| button.text_color(rgb(theme.muted)))
                            .child("Default"),
                    )
                    .child(
                        div()
                            .id("avatar-done")
                            .debug_selector(|| "avatar-done".into())
                            .p(px(6.))
                            .rounded(px(crate::config::corners::CONTROL))
                            .bg(rgb(theme.active))
                            .cursor_pointer()
                            .child("Done")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.dismiss_menu(window, cx)),
                            ),
                    ),
            )
    }
}
