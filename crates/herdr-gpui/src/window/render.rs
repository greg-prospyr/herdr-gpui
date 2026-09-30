//! Painting the window from prepared state. Render reads bounded caches and
//! the latest projection only: it never queries the daemon, touches disk, or
//! starts a process.

use super::HerdrWindow;
use crate::{
    APP_VERSION, CheckForUpdates, Minimize, PlaySound, RunCommand, ShowHerdrNotDetected,
    ShowUpdatePreview,
    actions::ShowToastPreview,
    browser::{Pick, Shown, Slot},
    config::ClipboardToastPosition,
    fonts::StyledFont,
    state::ConnectionStatus,
    terminal::*,
    worktree_banner,
};
use gpui::{prelude::*, *};
use herdr_client::ConnectOptions;
use std::time::Duration;

impl Render for HerdrWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.restore_menu_focus(window, cx);
        let font = self.config.terminal.font();
        // Parked groups paint with the same face as the window's terminal.
        let parked_font = font.clone();
        let cell_height = self.config.terminal.line_height();
        self.painter.borrow_mut().set_appearance(
            self.config.terminal.size,
            cell_height,
            self.theme.clone(),
        );
        self.cell_width = self.painter.borrow_mut().cell_width(&font, window, cx);
        // Registers the window for surface-only redraws; see `redraw_terminal`.
        self.surface_signal.read(cx);
        let avatar_layer = self.render_avatar_layer(window, cx);
        let sidebar = self.sidebar_visible.then(|| {
            crate::sidebar::cached_view(
                &self.sidebar_view,
                self.sidebar_width,
                f32::from(window.viewport_size().width),
            )
        });
        // Paints the frame on screen, which during a focus change is the one
        // presented before it: the terminal area never blanks between two
        // projections. What the client knows to be current stays in `live`.
        let surface = self.presentation.frame(&self.live);
        // A group picking the tab another shows paints this same frame.
        let window_frame = surface.clone();
        let entity = cx.entity();
        let paint_entity = entity.clone();
        let focus = self.focus.clone();
        let menu_open = self.menu.page.is_some();
        let cell_width = self.cell_width;
        let painter = self.painter.clone();
        // The highlight is grid coordinates, so it paints with the frame that
        // owns the cells rather than being recomputed from the pointer here.
        let selection = self.selection.clone();
        // The IME composition paints inline at the input cursor; a menu's
        // text field shows its own.
        // It anchors to the live surface, as the IME's candidate window does,
        // so a retained frame never separates the text from the window.
        let marked = (!menu_open && !self.marked.is_empty())
            .then(|| (self.marked.clone(), self.live.surface.clone()));
        self.hovered_terminal_link =
            self.terminal_link_hovered(window.mouse_position(), window.modifiers());
        self.split_cursor = self.split_cursor_at(window.mouse_position());
        // Pad the terminal itself: the canvas bounds that painting, hit testing,
        // and IME placement all read then already exclude the gap.
        let sidebar_gap = if self.sidebar_visible {
            self.config.layout.sidebar_gap
        } else {
            0.
        };
        // Only the first group meets the sidebar, so only it takes the gap.
        self.ensure_layout();
        self.forget_gone_strips();
        let slots = self.group_slots();
        let shown: Vec<Shown> = slots
            .iter()
            .map(|slot| self.group_shown(slot.id, cx))
            .collect();
        let slot_gap = |slot: Slot| if slot.index == 0 { sidebar_gap } else { 0. };
        // The window's own terminal, with its input, goes to the group holding
        // its connection; other terminal groups paint parked connections.
        let primary = self.primary_group();
        let terminal_slot = slots
            .iter()
            .zip(&shown)
            .find(|(slot, shown)| {
                **shown == Shown::Terminal && primary.is_none_or(|group| group == slot.id)
            })
            .map(|(slot, _)| *slot);
        let terminal_gap = terminal_slot.map_or(sidebar_gap, slot_gap);
        let drop_hover = self.render_drop_hover(terminal_gap, window, cx);
        self.forget_gone_chats();
        let chat_layer = self.render_chat_layer(surface.as_deref(), terminal_gap, cx);
        let terminal = div()
            .id("terminal")
            .debug_selector(|| "terminal".into())
            .pl(px(terminal_gap))
            .when(self.hovered_terminal_link, |terminal| {
                terminal.cursor_pointer()
            })
            .when_some(self.split_cursor, |terminal, cursor| {
                terminal.cursor(cursor)
            })
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                let split_cursor = this.split_cursor_at(event.position);
                if split_cursor != this.split_cursor {
                    this.split_cursor = split_cursor;
                    cx.notify();
                }
                // A border is not the application's to hover.
                if split_cursor.is_none() {
                    this.terminal_mouse_hover(event, cx);
                }
                let hovered = this.terminal_link_hovered(event.position, event.modifiers);
                if hovered != this.hovered_terminal_link {
                    this.hovered_terminal_link = hovered;
                    cx.notify();
                }
            }))
            // Holding the link modifier over a link in a mouse-reporting
            // application changes what a click does, so the pointer follows.
            .on_modifiers_changed(
                cx.listener(|this, event: &ModifiersChangedEvent, window, cx| {
                    let hovered =
                        this.terminal_link_hovered(window.mouse_position(), event.modifiers);
                    if hovered != this.hovered_terminal_link {
                        this.hovered_terminal_link = hovered;
                        cx.notify();
                    }
                }),
            )
            .on_click(cx.listener(Self::open_terminal_link))
            .relative()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .bg(rgb(self.theme.background))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            // A selection is copied when it is released, so the terminal has
            // nothing for Cut, Copy, or Select All to act on.
            .on_action(cx.listener(|this, _: &crate::actions::Paste, _, cx| this.paste(cx)))
            .on_scroll_wheel(cx.listener(Self::scroll_wheel))
            .on_drag_move(cx.listener(Self::file_drag_move))
            .on_drop(cx.listener(Self::drop_terminal_files))
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.terminal_mouse_down(event, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    if this.terminal_mouse_down(event, window, cx) {
                        return;
                    }
                    cx.stop_propagation();
                    this.open_pane_menu_at(event.position, window, cx);
                    this.menu.opening_right_click = this.menu.page == Some(crate::menu::Page::Pane);
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    if this.scrollbar_mouse_down(event, cx)
                        || this.split_mouse_down(event, cx)
                        || this.terminal_mouse_down(event, window, cx)
                    {
                        return;
                    }
                    // A mouse-reporting application owns plain clicks on its
                    // links; the link modifier still opens them here.
                    this.pressed_terminal_link = (!this
                        .reports_mouse_at(event.position, event.modifiers))
                    .then(|| this.terminal_link_at(event.position))
                    .flatten()
                    .map(|url| (url, event.position));
                    if this.menu.page.is_some() {
                        return;
                    }
                    // A press on a link may still turn into a drag across it,
                    // so the selection starts either way; the click that opens
                    // the link is the one that never left its half-cell.
                    this.begin_selection(event.position, event.click_count, cx);
                    if this.pressed_terminal_link.is_some() {
                        cx.stop_propagation();
                        return;
                    }
                    window.focus(&this.focus, cx);
                    if this.input_ready()
                        && let Some(surface) = &this.live.surface
                    {
                        let pane = pane_at(
                            surface,
                            this.bounds,
                            event.position,
                            this.cell_width,
                            this.config.terminal.line_height(),
                        )
                        .map(str::to_owned);
                        if let Some(id) = pane {
                            this.focus_clicked_pane(&id, cx);
                        }
                    }
                }),
            )
            .child(
                canvas(
                    move |bounds, _, cx| {
                        entity.update(cx, |this, _| {
                            this.bounds = bounds;
                            this.options = ConnectOptions {
                                surface_size: viewport(
                                    bounds.size.width.to_f64() as f32,
                                    bounds.size.height.to_f64() as f32,
                                    cell_width,
                                    cell_height,
                                ),
                                cell_width_px: cell_width.round().max(1.) as u32,
                                cell_height_px: cell_height.round().max(1.) as u32,
                            };
                            this.resize();
                        });
                    },
                    move |bounds, _, window, cx| {
                        // Capture movement outside the terminal too, before any
                        // element can stop propagation of a drag-away event.
                        let entity = paint_entity.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                            if phase == DispatchPhase::Capture {
                                entity.update(cx, |this, cx| {
                                    if this.scrollbar_mouse_move(event, cx)
                                        || this.split_mouse_move(event, cx)
                                        || this.terminal_mouse_move(event, cx)
                                    {
                                        cx.stop_propagation();
                                        return;
                                    }
                                    if this.pressed_terminal_link.as_ref().is_some_and(
                                        |(_, position)| {
                                            (event.position.x - position.x).abs() > px(4.)
                                                || (event.position.y - position.y).abs() > px(4.)
                                        },
                                    ) {
                                        this.pressed_terminal_link = None;
                                    }
                                    // A drag that leaves the terminal keeps
                                    // selecting, and hover work elsewhere stays
                                    // out of the gesture.
                                    if this.extend_selection(event.position, cx) {
                                        cx.stop_propagation();
                                    }
                                });
                            }
                        });
                        let released = paint_entity.clone();
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                            if phase == DispatchPhase::Capture {
                                released.update(cx, |this, cx| {
                                    // Global: the overlay occludes the opener, and release
                                    // may also precede the overlay's first frame.
                                    if matches!(
                                        event.button,
                                        MouseButton::Left | MouseButton::Right
                                    ) {
                                        this.menu.opening_right_click = false;
                                    }
                                    if this.scrollbar_mouse_up(event, cx)
                                        || this.split_mouse_up(event, cx)
                                        || this.terminal_mouse_up(event, cx)
                                        || (event.button == MouseButton::Left
                                            && !cx.has_active_drag()
                                            && this.release_left(event, cx))
                                    {
                                        cx.stop_propagation();
                                    }
                                });
                            }
                        });
                        window.handle_input(
                            &focus,
                            crate::input::TerminalInputHandler::new(
                                bounds,
                                paint_entity.clone(),
                                menu_open,
                            ),
                            cx,
                        );
                        if let Some(surface) = &surface {
                            // The highlight belongs to the frame that owns the
                            // cells, so only one of the two paints it.
                            let highlight = |owned: bool| {
                                selection
                                    .as_ref()
                                    .filter(|_| owned)
                                    .map(|selection| {
                                        selection.rows(surface, cell_width, cell_height).collect()
                                    })
                                    .unwrap_or_default()
                            };
                            let panes: Vec<_> = highlight(
                                selection
                                    .as_ref()
                                    .is_some_and(|selection| selection.in_panes()),
                            );
                            painter.borrow_mut().paint_frame(
                                &surface.frame,
                                bounds.origin,
                                cell_width,
                                &font,
                                &panes,
                                &surface.panes,
                                window,
                                cx,
                            );
                            if let Some(popup) = &surface.popup {
                                let offset = popup_origin(
                                    &surface.frame,
                                    &popup.frame,
                                    cell_width,
                                    cell_height,
                                );
                                let rows: Vec<_> =
                                    highlight(selection.as_ref().is_some_and(|selection| {
                                        selection.in_popup(&popup.terminal_id)
                                    }));
                                painter.borrow_mut().paint_frame(
                                    &popup.frame,
                                    bounds.origin + offset,
                                    cell_width,
                                    &font,
                                    &rows,
                                    &[],
                                    window,
                                    cx,
                                );
                            }
                        }
                        if let Some((marked, live)) = &marked {
                            let live = live.as_deref();
                            painter.borrow().paint_composition(
                                marked,
                                input_cursor_bounds(live, bounds.origin, cell_width, cell_height)
                                    .origin,
                                input_area(live, bounds, cell_width, cell_height),
                                &font,
                                window,
                            );
                        }
                    },
                )
                .size_full(),
            )
            // Direct feedback for the user's own gesture, not a daemon notice:
            // it sits over the cells it copied and needs no dismissing.
            .when_some(self.flash.as_ref(), |terminal, (flash, _)| {
                use ClipboardToastPosition::*;
                let position = self.config.clipboard_toast.position;
                terminal.child(
                    div()
                        .absolute()
                        .map(|row| match position {
                            TopLeft | TopCenter | TopRight => row.top(px(12.)),
                            BottomLeft | BottomCenter | BottomRight => row.bottom(px(12.)),
                        })
                        .map(|row| match position {
                            TopLeft | BottomLeft => row.justify_start(),
                            TopCenter | BottomCenter => row.justify_center(),
                            TopRight | BottomRight => row.justify_end(),
                        })
                        // The pane's own padding is not part of the terminal:
                        // the flash spans the cells, so centering centers on
                        // them and a corner is the corner of the grid.
                        .left(px(terminal_gap))
                        .right_0()
                        .px(px(12.))
                        .flex()
                        .overflow_hidden()
                        .child(
                            div()
                                .debug_selector(|| "flash".into())
                                .min_w_0()
                                .flex()
                                .items_center()
                                .gap(px(10.))
                                .px(px(16.))
                                .py(px(9.))
                                .rounded(px(crate::config::corners::PANEL))
                                .border_1()
                                .border_color(rgb(flash.accent(&self.theme)))
                                .bg(rgb(self.theme.surface))
                                .shadow_lg()
                                .text_color(rgb(self.theme.foreground))
                                .text_size(px(self.config.ui.size + 1.))
                                .font_weight(FontWeight::MEDIUM)
                                // A check for something done, so the flash reads
                                // before its words do; a dot for anything else.
                                .child(match flash.tone {
                                    crate::window::flash::Tone::Success => div()
                                        .flex_none()
                                        .text_color(rgb(flash.accent(&self.theme)))
                                        .font_weight(FontWeight::BOLD)
                                        .child("\u{2713}"),
                                    crate::window::flash::Tone::Warning => div()
                                        .size(px(7.))
                                        .flex_none()
                                        .rounded_full()
                                        .bg(rgb(flash.accent(&self.theme))),
                                })
                                .child(div().truncate().child(flash.text.clone())),
                        ),
                )
            })
            .children(drop_hover);
        // Chats are the terminal's siblings, not its children: keys typed in
        // a chat never pass through the terminal's key handler.
        let terminal = div()
            .relative()
            .flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(terminal)
            .children(chat_layer);
        // A focus handle belongs to one element: the terminal when drawn,
        // otherwise the group in use.
        let keyboard = terminal_slot
            .is_none()
            .then(|| self.active_group())
            .flatten();
        let mut terminal = Some(terminal);
        let mut groups = Vec::with_capacity(slots.len());
        for (slot, shown) in slots.into_iter().zip(shown) {
            let gap = slot_gap(slot);
            let owns_keyboard = keyboard == Some(slot.id);
            let tab = match &shown {
                Shown::Page(id) => cx
                    .try_global::<crate::browser::Store>()
                    .and_then(|store| store.get(*id))
                    .cloned(),
                _ => None,
            };
            let body = match (&shown, tab) {
                (Shown::Terminal, _) if terminal_slot == Some(slot) => terminal
                    .take()
                    .map(IntoElement::into_any_element)
                    .unwrap_or_else(|| div().into_any_element()),
                (Shown::Terminal, _) if self.shows_parked_terminal(slot.id, cx) => {
                    self.render_parked_terminal(slot, gap, parked_font.clone(), cell_height, cx)
                }
                (Shown::Page(_), Some(tab)) => {
                    self.render_browser(slot, &tab, gap, owns_keyboard, cx)
                }
                (Shown::Elsewhere(Pick::Herdr(tab)), _) => {
                    match self.live_frame_of(tab, window_frame.clone()) {
                        Some(frame) => self.render_terminal_mirror(
                            slot,
                            gap,
                            Some(frame),
                            parked_font.clone(),
                            cell_height,
                            cx,
                        ),
                        None => self.render_stand_in(slot, &shown, gap, owns_keyboard, cx),
                    }
                }
                _ => self.render_stand_in(slot, &shown, gap, owns_keyboard, cx),
            };
            groups.push(self.render_group(slot, body, window, cx));
        }
        let content = self.render_groups(groups, cx);
        // Not `||`: asking forgets group motion that has finished.
        if self.groups_moving() | self.tabs_growing() | self.annotations_moving() {
            window.request_animation_frame();
        }
        // A menu just opened, or a covered page's picture is on its way.
        if self.present_browser(cx) {
            window.request_animation_frame();
        }
        let status = (!matches!(self.live.status, ConnectionStatus::Connected)
            || self.local_error.is_some()
            || self.live.error.is_some())
        .then(|| self.live.status_text(self.local_error.as_deref()));
        div()
            .on_action(cx.listener(|this, action: &crate::actions::SetLayout, _, cx| {
                this.set_layout(action.mode, cx);
            }))
            .on_action(cx.listener(|this, action: &RunCommand, window, cx| {
                this.command(action.command, window, cx);
            }))
            .on_action(cx.listener(|_, _: &Minimize, window, _| {
                window.minimize_window();
            }))
            .on_action(cx.listener(|this, _: &ShowHerdrNotDetected, window, cx| {
                this.show_install_modal(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CheckForUpdates, window, cx| {
                this.open_app_update(false, window, cx);
                this.updater.check();
            }))
            .on_action(cx.listener(|this, _: &ShowUpdatePreview, window, cx| {
                this.open_app_update(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &crate::actions::ShowUpdateDownloadPreview, window, cx| {
                this.open_update_progress_preview(
                    crate::updater::State::Downloading { received: 50_000_000, total: 100_000_000 },
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &crate::actions::ShowUpdateHomebrewPreview, window, cx| {
                this.open_update_progress_preview(
                    crate::updater::State::Upgrading { detail: "Refreshing Homebrew metadata with brew update...".into() },
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, action: &ShowToastPreview, _, cx| {
                this.show_toast_preview(action.kind, cx);
            }))
            .on_action(cx.listener(|this, _: &PlaySound, _, _| {
                this.sound.preview();
            }))
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(self.theme.background))
            .text_color(rgb(self.theme.foreground))
            .text_font(&self.config.ui)
            .text_size(px(self.config.ui.size))
            .child(self.render_titlebar(cx))
            .children(worktree_banner::render(
                env!("HERDR_BUILD_WORKTREE") == "1",
                env!("HERDR_BUILD_BRANCH"),
                env!("HERDR_BUILD_PR"),
            ))
            .child(
                div()
                    .debug_selector(|| "window-body".into())
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .children(sidebar)
                    // After the sidebar, so avatars paint over their slots,
                    // and before everything that may cover the sidebar.
                    .children(avatar_layer)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                             .flex_1()
                             .min_w_0()
                             .min_h_0()
                            .child(content)
                            .child(
                div()
                    .id("connection-status")
                    .debug_selector(|| "connection-status".into())
                    .flex()
                    .flex_none()
                    .h(px((self.config.ui.size * 1.5 + 4.).max(22.)))
                    .overflow_hidden()
                    .items_center()
                    .gap(px(6.))
                    .px_3()
                    .bg(rgb(self.theme.surface))
                    .text_color(rgb(self.theme.foreground))
                    .children(self.render_usage(cx))
                    .when_some(
                        self.prefix_armed
                            .then(|| self.config.keybindings.prefix_label())
                            .flatten(),
                        |bar, prefix| bar.child(
                            div()
                                .debug_selector(|| "prefix-armed".into())
                                .flex_none()
                                .px(px(6.))
                                .rounded(px(crate::config::corners::SMALL))
                                .bg(rgb(self.theme.active))
                                .child(prefix),
                        ),
                    )
                    .when(!self.live.status.is_connected(), |bar| bar.child(
                        if matches!(self.live.status, ConnectionStatus::StartingDaemon) {
                            div()
                                .size(px(8.))
                                .flex_none()
                                .rounded_full()
                                .bg(rgb(self.theme.palette[3]))
                                .with_animation(
                                    "daemon-starting-loader",
                                    Animation::new(Duration::from_secs(1)).repeat(),
                                    |dot, delta| {
                                        dot.opacity(
                                            0.3 + 0.7 * (delta * std::f32::consts::PI).sin(),
                                        )
                                    },
                                )
                                .into_any_element()
                        } else {
                            div()
                                .size(px(6.))
                                .flex_none()
                                .rounded_full()
                                .bg(rgb(self.theme.palette[1]))
                                .into_any_element()
                        },
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .when_some(status, |row, status| row.child(
                                div().debug_selector(|| "connection-message".into()).child(status)
                            )),
                    )
                    .child(
                        div()
                                    .id("status-theme")
                                    .debug_selector(|| "status-theme".into())
                                    .flex_shrink_1()
                                    .min_w(px(33.))
                            .flex()
                            .items_center()
                            .gap(px(5.))
                            .px_2()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(self.theme.active)))
                            .child(
                                svg()
                                    .path("icons/theme.svg")
                                    .size(px(12.))
                                    .flex_none()
                                    .text_color(rgb(self.theme.foreground)),
                            )
                                    .child(div().min_w_0().truncate().child("Theme"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_theme_picker(window, cx);
                            })),
                    )
                    .child(
                        div()
                                    .id("status-keybinds")
                                    .debug_selector(|| "status-keybinds".into())
                                    .flex_shrink_1()
                                    .min_w(px(33.))
                            .flex()
                            .items_center()
                            .gap(px(5.))
                            .px_2()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(self.theme.active)))
                            .child(
                                svg()
                                    .path("icons/keyboard.svg")
                                    .size(px(12.))
                                    .flex_none()
                                    .text_color(rgb(self.theme.foreground)),
                            )
                                    .child(div().min_w_0().truncate().child("Shortcuts"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_keybinds(window, cx);
                            })),
                    )
                    .child(
                        div()
                                    .id("report-issue")
                                    .debug_selector(|| "report-issue".into())
                                    .flex_shrink_1()
                                    .min_w(px(33.))
                            .flex()
                            .items_center()
                            .gap(px(5.))
                            .px_2()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(self.theme.active)))
                            .child(
                                div()
                                    .size(px(12.))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .border_1()
                                    .border_color(rgb(self.theme.foreground))
                                    .child(
                                        div()
                                            .size(px(3.))
                                            .rounded_full()
                                            .bg(rgb(self.theme.foreground)),
                                    ),
                            )
                                    .child(div().min_w_0().truncate().child("Report issue"))
                            .on_click(|_, _, cx| {
                                cx.open_url(&format!(
                                    "https://github.com/penso/herdr-gpui/issues/new?template=bug_report.yml&version={}",
                                    APP_VERSION.replace('+', "%2B"),
                                ));
                            }),
                    )
                    .child(
                        div()
                            .id("status-version")
                            .debug_selector(|| "status-version".into())
                            .flex_none()
                            .whitespace_nowrap()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(self.theme.active)))
                            // A waiting update is the one status here worth
                            // interrupting for, so it takes the accent color
                            // the rest of the chrome reserves for chosen rows.
                            .text_color(rgb(if self.updater.update_available() {
                                self.theme.primary()
                            } else {
                                self.theme.muted
                            }))
                            .child(if self.updater.update_available() {
                                "Update available"
                            } else {
                                APP_VERSION
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_app_update(false, window, cx);
                            })),
                    ),
                            ),
                    ),
            )
            .children(self.render_toasts(window, cx))
            .children(self.render_file_transfer(window, cx))
            .when(self.menu.page.is_some(), |root| {
                root.child(self.render_menu(window, cx))
            })
    }
}
