//! Agent panes shown as native chat. A chat covers its pane's cells while
//! the pane keeps running underneath; the window places it from the same
//! surface geometry the terminal paints with, and sends what the user
//! writes to the pane as typed text followed by Return.

use super::HerdrWindow;
use crate::{
    chat::{ChatEvent, ChatPanel, Dialect, PaneContext},
    connection::ConnectionBridge,
    terminal::InputTarget,
};
use gpui::{prelude::*, *};
use herdr_client::protocol::{AgentStatus, ClientPaneInputEvent, PaneSurfaceFrame};
use std::collections::HashMap;

/// Chats open at once; each follows a session log with its own reader.
const MAX_CHATS: usize = 8;
/// Panes narrower than this keep their terminal: a chat would not fit.
const MIN_COLUMNS: u16 = 30;

/// A pane on one endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ChatKey {
    endpoint: String,
    pane: String,
}

#[derive(Default)]
pub(crate) struct Chats {
    open: HashMap<ChatKey, (Entity<ChatPanel>, Subscription)>,
}

impl HerdrWindow {
    fn chat_key(&self, pane_id: &str) -> Option<ChatKey> {
        let endpoint = self.endpoints.get(self.selected_endpoint)?;
        Some(ChatKey {
            endpoint: endpoint.id.clone(),
            pane: pane_id.to_owned(),
        })
    }

    /// What Herdr's snapshot says about `pane_id`.
    fn pane_context(&self, pane_id: &str) -> PaneContext {
        let Some(snapshot) = &self.live.snapshot else {
            return PaneContext::default();
        };
        let agent = snapshot
            .agents
            .iter()
            .find(|agent| agent.pane_id == pane_id);
        let pane = snapshot.panes.iter().find(|pane| pane.pane_id == pane_id);
        PaneContext {
            agent: agent.and_then(|agent| agent.agent.clone()),
            working: agent.is_some_and(|agent| agent.agent_status == AgentStatus::Working),
            cwd: pane.and_then(|pane| pane.foreground_cwd.clone().or_else(|| pane.cwd.clone())),
            focused: pane.is_some_and(|pane| pane.focused),
        }
    }

    /// Whether `pane_id` runs an agent whose session the chat can read.
    pub(crate) fn chat_capable(&self, pane_id: &str) -> bool {
        self.pane_context(pane_id)
            .agent
            .as_deref()
            .and_then(Dialect::of_agent)
            .is_some()
    }

    /// Shows `pane_id` as a chat, or its terminal again.
    pub(crate) fn toggle_chat(
        &mut self,
        pane_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.chat_key(pane_id) else {
            return;
        };
        if self.chats.open.remove(&key).is_some() {
            window.focus(&self.focus, cx);
            cx.notify();
            return;
        }
        if self.chats.open.len() >= MAX_CHATS {
            self.local_error = Some(format!(
                "At most {MAX_CHATS} panes can show as chat at once."
            ));
            cx.notify();
            return;
        }
        let host = self
            .endpoints
            .get(self.selected_endpoint)
            .and_then(|endpoint| crate::teleport::host_for(&endpoint.connection.target).ok());
        let context = self.pane_context(pane_id);
        let theme = self.theme.clone();
        let ui = self.config.ui.clone();
        let mono = self.config.terminal.clone();
        let pane = pane_id.to_owned();
        let panel = cx.new(|cx| ChatPanel::new(host, pane, context, &theme, ui, mono, cx));
        let target = key.clone();
        let subscription = cx.subscribe_in(
            &panel,
            window,
            move |this, _, event: &ChatEvent, window, cx| match event {
                ChatEvent::Send(text) => this.send_chat(&target.pane, text, cx),
                ChatEvent::ShowTerminal => {
                    if this.chats.open.remove(&target).is_some() {
                        window.focus(&this.focus, cx);
                        cx.notify();
                    }
                }
            },
        );
        window.focus(&panel.focus_handle(cx), cx);
        self.chats.open.insert(key, (panel, subscription));
        cx.notify();
    }

    /// Types `text` into the pane and presses Return, as the user would.
    fn send_chat(&mut self, pane_id: &str, text: &str, cx: &mut Context<Self>) {
        let result = (|| {
            let endpoint = &self.endpoints[self.selected_endpoint];
            let handle = endpoint
                .connection
                .handle
                .as_ref()
                .ok_or(crate::Error::NotConnected)?;
            let snapshot = self
                .live
                .snapshot
                .as_ref()
                .ok_or(crate::Error::NoSnapshot)?;
            let target = InputTarget::Pane(pane_id.to_owned());
            for event in [
                ClientPaneInputEvent::TextCommit(text.to_owned()),
                crate::input::enter_key(),
            ] {
                ConnectionBridge::send_input(handle, &snapshot.boot_id, &target, event)
                    .map_err(crate::Error::ChatSend)?;
            }
            Ok::<_, crate::Error>(())
        })();
        if let Err(error) = result {
            self.local_error = Some(error.to_string());
            cx.notify();
        }
    }

    /// Chats over their panes, and a way into chat on the agent panes that
    /// are not showing one. Positions come from the surface being painted.
    pub(super) fn render_chat_layer(
        &mut self,
        surface: Option<&PaneSurfaceFrame>,
        gap: f32,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(surface) = surface.filter(|surface| surface.popup.is_none()) else {
            return Vec::new();
        };
        let cell_width = self.cell_width;
        let cell_height = self.config.terminal.line_height();
        let mut layer = Vec::new();
        for pane in &surface.panes {
            // A chat is a tile over the whole pane, its frame included, a
            // hair inside it so neighbouring tiles keep a gap between them.
            let rect = pane.rect;
            if rect.width < MIN_COLUMNS || rect.height < 6 {
                continue;
            }
            let inset = 2.;
            let left = gap + f32::from(rect.x) * cell_width + inset;
            let top = f32::from(rect.y) * cell_height + inset;
            let width = f32::from(rect.width) * cell_width - inset * 2.;
            let height = f32::from(rect.height) * cell_height - inset * 2.;
            let key = self.chat_key(&pane.pane_id);
            let open = key.as_ref().and_then(|key| self.chats.open.get(key));
            if let Some((panel, _)) = open {
                let context = self.pane_context(&pane.pane_id);
                panel.update(cx, |panel, cx| {
                    panel.set_context(
                        context,
                        &self.theme,
                        &self.config.ui,
                        &self.config.terminal,
                        cx,
                    );
                });
                layer.push(
                    div()
                        .debug_selector(|| "chat-overlay".into())
                        .absolute()
                        .left(px(left))
                        .top(px(top))
                        .w(px(width))
                        .h(px(height))
                        .occlude()
                        .child(
                            AnyView::from(panel.clone())
                                .cached(StyleRefinement::default().size_full()),
                        )
                        .into_any_element(),
                );
            } else if self.chat_capable(&pane.pane_id) {
                let pane_id = pane.pane_id.clone();
                let inner = pane.inner_rect;
                layer.push(
                    div()
                        .absolute()
                        .left(px(gap + f32::from(inner.x) * cell_width))
                        .top(px(f32::from(inner.y) * cell_height + 6.))
                        .w(px(f32::from(inner.width) * cell_width - 8.))
                        .flex()
                        .justify_end()
                        .child(
                            crate::chat::native_tag(self.config.ui.size)
                                .id(ElementId::Name(format!("chat-open-{pane_id}").into()))
                                .debug_selector(|| "chat-open".into())
                                .occlude()
                                .opacity(0.8)
                                .hover(|tag| tag.opacity(1.))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.toggle_chat(&pane_id, window, cx);
                                })),
                        )
                        .into_any_element(),
                );
            }
        }
        layer
    }

    /// Chats of panes that no longer exist on their endpoint are closed.
    pub(super) fn forget_gone_chats(&mut self) {
        let Some(snapshot) = &self.live.snapshot else {
            return;
        };
        let Some(endpoint) = self.endpoints.get(self.selected_endpoint) else {
            return;
        };
        self.chats.open.retain(|key, _| {
            key.endpoint != endpoint.id
                || snapshot.panes.iter().any(|pane| pane.pane_id == key.pane)
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::{sidebar::layout_tests::fixture_window, window::MockPeer};
    use core::prelude::v1::test;
    use herdr_client::{ConnectTarget, protocol::ClientMessage};
    use std::path::PathBuf;

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
    }

    /// An agent pane offers chat; opening it covers the pane, what the user
    /// writes reaches the pane as typed text and Return, and asking for the
    /// terminal takes the chat away again.
    #[gpui::test]
    fn agent_panes_open_as_chat_and_send_what_the_user_writes(cx: &mut TestAppContext) {
        let mut peer = MockPeer::new();
        let handle = peer.client.handle.clone();
        let (view, cx) = cx.add_window_view(|window, cx| {
            let mut view = fixture_window(window, cx);
            // A custom socket: the chat says it cannot read sessions there,
            // and no reader is started against any daemon.
            view.endpoints[0].connection.target =
                ConnectTarget::Socket(PathBuf::from("/nonexistent/herdr.sock"));
            view.endpoints[0].connection.handle = Some(handle);
            let mut snapshot = (*view.live.snapshot.clone().unwrap()).clone();
            // The peer's daemon; input for any other boot is refused.
            snapshot.boot_id = "boot-v1".into();
            snapshot
                .panes
                .push(herdr_client::protocol::ClientShellPane {
                    pane_id: "p0".into(),
                    workspace_id: "w0".into(),
                    tab_id: "t0".into(),
                    label: None,
                    cwd: Some("/work/herdr".into()),
                    foreground_cwd: None,
                    focused: true,
                    right_click_passthrough: false,
                });
            view.live.surface = Some(crate::group_terminals::tests::surface(&snapshot, "p0"));
            view.live.snapshot = Some(std::sync::Arc::new(snapshot));
            view
        });
        draw(cx);
        assert!(
            cx.debug_bounds("chat-open").is_some(),
            "the fixture's pane p0 runs Claude"
        );
        assert!(cx.debug_bounds("chat-overlay").is_none());

        view.update_in(cx, |view, window, cx| view.toggle_chat("p0", window, cx));
        draw(cx);
        let overlay = cx
            .debug_bounds("chat-overlay")
            .expect("the chat covers the pane");
        assert!(overlay.size.width > px(0.) && overlay.size.height > px(0.));
        assert!(cx.debug_bounds("chat-open").is_none());
        assert!(cx.debug_bounds("chat-composer").is_some());

        let panel = view.read_with(cx, |view, _| {
            view.chats.open.values().next().unwrap().0.clone()
        });
        panel.update(cx, |_, cx| cx.emit(ChatEvent::Send("Ship it".into())));
        let mut events = Vec::new();
        while events.len() < 2 {
            match peer.receive() {
                ClientMessage::ClientShellPaneInput {
                    pane_id,
                    events: sent,
                } => {
                    assert_eq!(pane_id, "p0");
                    events.extend(sent);
                }
                // The window's own resize traffic shares the wire.
                ClientMessage::ClientShellResize { .. } => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(
            events,
            [
                ClientPaneInputEvent::TextCommit("Ship it".into()),
                crate::input::enter_key()
            ]
        );

        panel.update(cx, |_, cx| cx.emit(ChatEvent::ShowTerminal));
        draw(cx);
        assert!(cx.debug_bounds("chat-overlay").is_none());
        assert!(cx.debug_bounds("chat-open").is_some());
        assert!(view.read_with(cx, |view, _| view.chats.open.is_empty()));
    }
}
