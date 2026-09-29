//! Choosing a space's avatar from its menu, which only attention order offers.

#![allow(clippy::unwrap_used)]

use super::{Page, WorkspaceMenuAction};
use crate::{
    pixel_avatar::{AvatarStore, AvatarStyle, Hue, Shape},
    preferences::SpaceSort,
    spaces::SpaceKey,
};
use gpui::{Modifiers, TestAppContext, VisualTestContext, px, size};

#[gpui::test]
fn a_space_menu_changes_its_avatar_and_back(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        crate::bind_keys(cx);
        let mut view = crate::sidebar::layout_tests::fixture_window(window, cx);
        view.live.status = crate::state::ConnectionStatus::Connected;
        view
    });
    cx.simulate_resize(size(px(800.), px(600.)));
    let offers = |cx: &mut VisualTestContext, sort: SpaceSort| {
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.dismiss_menu(window, cx);
                view.space_sort = sort;
                view.open_workspace_menu("w1", Default::default(), window, cx);
                view.workspace_menu_actions()
                    .contains(&WorkspaceMenuAction::Avatar)
            })
        })
    };
    assert!(
        !offers(cx, SpaceSort::Manual),
        "manual order shows no avatars"
    );
    assert!(offers(cx, SpaceSort::Attention));
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.activate_workspace_menu(WorkspaceMenuAction::Avatar, window, cx);
            assert_eq!(view.menu.page, Some(Page::Avatar));
            // The row the picker is for stays marked, as for its menu.
            assert_eq!(view.workspace_menu_target(), Some("w1"));
        });
        window.draw(cx).clear(cx);
    });
    let key = SpaceKey::new(crate::endpoint::LOCAL, "w1");
    let style = |cx: &mut VisualTestContext| cx.update(|_, cx| AvatarStore::style(cx, &key));
    let click = |cx: &mut VisualTestContext, selector: &'static str| {
        let bounds = cx.debug_bounds(selector).unwrap();
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
    };
    click(cx, "avatar-shape-4");
    assert_eq!(style(cx).shape, Shape::Cloud);
    click(cx, "avatar-hue-5");
    assert_eq!(
        style(cx),
        AvatarStyle {
            shape: Shape::Cloud,
            hue: Hue::Lagoon
        }
    );
    // Another space keeps its own avatar.
    let other = SpaceKey::new(crate::endpoint::LOCAL, "w0");
    assert_eq!(
        cx.update(|_, cx| AvatarStore::style(cx, &other)),
        AvatarStyle::default_for("w0")
    );
    click(cx, "avatar-default");
    assert_eq!(style(cx), AvatarStyle::default_for("w1"));
    cx.simulate_keystrokes("escape");
    assert!(view.read_with(cx, |view, _| view.menu.page.is_none()));
}
