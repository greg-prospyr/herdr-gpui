//! The chat panel: one pane's conversation, the agent's working state, and a
//! box to write to it, drawn as Tern's native pane. It owns its follower, so
//! closing the panel stops the reading; the agent and its terminal never
//! notice.

use super::{
    Conversation, Placed,
    follow::{self, Event, Follower},
    markdown::Block,
    tern::{self, color},
    transcript::{Dialect, Entry, ToolState, Update},
};
use crate::{
    config::{FontConfig, Theme},
    search_input::SearchInput,
    teleport::Host,
};
use gpui::{prelude::*, *};
use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::mpsc::TryRecvError,
    time::{Duration, Instant},
};

/// Events handled per poll; more wait for the next one, which comes soon.
const EVENTS_PER_POLL: usize = 64;
const BUSY_POLL: Duration = Duration::from_millis(16);
const ACTIVE_POLL: Duration = Duration::from_millis(50);
const IDLE_POLL: Duration = Duration::from_millis(150);
/// Items arriving within this long of a session starting are its backlog,
/// shown at once rather than animated in one by one.
const BACKLOG: Duration = Duration::from_millis(1500);

/// What the window knows about the pane, from Herdr's snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PaneContext {
    /// The agent Herdr detected, such as `claude`.
    pub agent: Option<String>,
    /// Whether Herdr reports the agent at work.
    pub working: bool,
    pub cwd: Option<String>,
    /// Whether the pane has the terminal's focus.
    pub focused: bool,
}

pub(crate) enum ChatEvent {
    /// Text the user sent, to type into the pane.
    Send(String),
    /// The user asked for the terminal again.
    ShowTerminal,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Status {
    Starting,
    Following(Dialect),
    NoSession { agent: Option<String> },
    Failed(String),
}

pub(crate) struct ChatPanel {
    follower: Option<Follower>,
    pub(super) status: Status,
    pub(super) conversation: Conversation,
    pub(super) list: ListState,
    /// Parsed Markdown by item id, with the item version it was parsed from.
    pub(super) parsed: HashMap<u64, (u64, Rc<Vec<Block>>)>,
    /// Tool cards the user opened.
    pub(super) expanded: HashSet<u64>,
    pub(super) input: Entity<SearchInput>,
    pub(super) context: PaneContext,
    /// When Herdr first reported the agent working, for a turn whose log
    /// does not say when it began.
    working_since: Option<Instant>,
    pub(super) ui: FontConfig,
    pub(super) mono: FontConfig,
    /// The item whose text was just copied, for a moment of feedback.
    pub(super) copied: Option<(u64, Instant)>,
    pub(super) caches: super::render::Caches,
    /// When the session being shown started to be followed.
    following_since: Option<Instant>,
    /// When live items arrived, so they animate in.
    pub(super) born: HashMap<u64, Instant>,
    /// When running tools finished, so their Done pops.
    pub(super) settled: HashMap<u64, Instant>,
    /// The phase of looping motion: dots, shimmer, pulse.
    pub(super) clock: Instant,
    _poll: Task<()>,
}

impl EventEmitter<ChatEvent> for ChatPanel {}

impl Focusable for ChatPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus.clone()
    }
}

/// The message box's font and colors, from Tern's composer.
fn composer_style(theme: &Theme, ui: &FontConfig) -> (FontConfig, Theme) {
    let font = FontConfig {
        family: tern::SANS.into(),
        size: tern::pane_em(ui.size),
        fallbacks: Some(vec![ui.family.clone()]),
    };
    let mut theme = theme.clone();
    theme.background = tern::TILE >> 8;
    theme.foreground = tern::INK >> 8;
    theme.muted = tern::DIM >> 8;
    theme.cursor = tern::TEAL >> 8;
    (font, theme)
}

impl ChatPanel {
    /// Starts following `pane_id` on `host`; `host` is `None` for endpoints
    /// the chat cannot reach, which then say so.
    pub(crate) fn new(
        host: Option<Host>,
        pane_id: String,
        context: PaneContext,
        theme: &Theme,
        ui: FontConfig,
        mono: FontConfig,
        cx: &mut Context<Self>,
    ) -> Self {
        let (follower, status) = match host.map(|host| follow::start(host, pane_id)) {
            Some(Ok(follower)) => (Some(follower), Status::Starting),
            Some(Err(error)) => (None, Status::Failed(error.to_string())),
            None => (None, Status::Failed(crate::Error::ChatHost.to_string())),
        };
        let input = cx.new(SearchInput::new);
        let placeholder = format!(
            "Ask {} anything",
            context.agent.as_deref().unwrap_or("the agent")
        );
        let (font, style) = composer_style(theme, &ui);
        input.update(cx, |input, cx| {
            input.set_bare(true, cx);
            input.set_placeholder(&placeholder, cx);
            input.set_appearance(font, style, cx);
        });
        let poll = cx.spawn(async move |this, cx| {
            let mut wait = ACTIVE_POLL;
            loop {
                cx.background_executor().timer(wait).await;
                match this.update(cx, |this, cx| this.poll(cx)) {
                    Ok(next) => wait = next,
                    Err(_) => break,
                }
            }
        });
        let list = ListState::new(0, ListAlignment::Bottom, px(600.));
        list.set_follow_mode(FollowMode::Tail);
        Self {
            follower,
            status,
            conversation: Conversation::default(),
            list,
            parsed: HashMap::new(),
            expanded: HashSet::new(),
            input,
            working_since: context.working.then(Instant::now),
            context,
            ui,
            mono,
            copied: None,
            caches: super::render::Caches::default(),
            following_since: None,
            born: HashMap::new(),
            settled: HashMap::new(),
            clock: Instant::now(),
            _poll: poll,
        }
    }

    /// Keeps the panel in step with the window: Herdr's view of the pane
    /// and the user's fonts.
    pub(crate) fn set_context(
        &mut self,
        context: PaneContext,
        theme: &Theme,
        ui: &FontConfig,
        mono: &FontConfig,
        cx: &mut Context<Self>,
    ) {
        if self.context == context && self.ui == *ui && self.mono == *mono {
            return;
        }
        if context.working && !self.context.working {
            self.working_since = Some(Instant::now());
        } else if !context.working {
            self.working_since = None;
        }
        if self.ui != *ui || self.mono != *mono {
            self.caches = super::render::Caches::default();
            let count = self.list.item_count();
            self.list.reset(count);
            let (font, style) = composer_style(theme, ui);
            self.input.update(cx, |input, cx| {
                input.set_appearance(font, style, cx);
            });
        }
        self.context = context;
        self.ui = ui.clone();
        self.mono = mono.clone();
        cx.notify();
    }

    fn poll(&mut self, cx: &mut Context<Self>) -> Duration {
        let mut handled = 0;
        let mut changed = false;
        while handled < EVENTS_PER_POLL {
            let Some(follower) = &self.follower else {
                break;
            };
            match follower.events().try_recv() {
                Ok(event) => {
                    self.handle(event);
                    handled += 1;
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.follower = None;
                    if !matches!(self.status, Status::Failed(_)) {
                        self.status = Status::Failed("The chat reader stopped.".into());
                    }
                    changed = true;
                    break;
                }
            }
        }
        if self
            .copied
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_millis(1400))
        {
            self.copied = None;
            changed = true;
        }
        // Finished animations need no record.
        let settle = Duration::from_secs(3);
        self.born.retain(|_, at| at.elapsed() < settle);
        self.settled.retain(|_, at| at.elapsed() < settle);
        // The working clock redraws every frame from render while it runs.
        if changed || self.elapsed().is_some() {
            cx.notify();
        }
        if handled == EVENTS_PER_POLL {
            BUSY_POLL
        } else if handled > 0 {
            ACTIVE_POLL
        } else {
            IDLE_POLL
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Session(dialect) => {
                self.reset();
                self.status = Status::Following(dialect);
                self.following_since = Some(Instant::now());
            }
            Event::NoSession { agent } => {
                self.reset();
                self.status = Status::NoSession { agent };
            }
            Event::Failed(error) => self.status = Status::Failed(error.to_string()),
            Event::Update(update) => {
                if let Status::Failed(_) | Status::Starting = self.status {
                    // A retried log that now reads is followed again.
                    self.status = Status::Following(self.dialect());
                    self.following_since.get_or_insert_with(Instant::now);
                }
                self.apply(update);
            }
        }
    }

    fn apply(&mut self, update: Update) {
        let now = Instant::now();
        let live = self
            .following_since
            .is_some_and(|since| since.elapsed() > BACKLOG);
        // A tool that was running and now is not has just finished.
        for item in &update.items {
            let Entry::Tool(tool) = &item.entry else {
                continue;
            };
            let was_running = self.conversation.items.iter().any(|kept| {
                kept.id == item.id
                    && matches!(&kept.entry, Entry::Tool(kept) if kept.state == ToolState::Running)
            });
            if live && was_running && tool.state != ToolState::Running {
                self.settled.insert(item.id, now);
            }
        }
        let ids: Vec<u64> = update.items.iter().map(|item| item.id).collect();
        let (placed, removed) = self.conversation.apply(update);
        for (place, id) in placed.into_iter().zip(ids) {
            match place {
                Placed::Changed(index) => self.list.splice(index..index + 1, 1),
                Placed::Appended => {
                    let end = self.list.item_count();
                    self.list.splice(end..end, 1);
                    if live {
                        self.born.insert(id, now);
                    }
                }
                Placed::Dropped => {}
            }
        }
        if removed > 0 {
            self.list.splice(0..removed, 0);
            let first = self.conversation.items.front().map_or(0, |item| item.id);
            self.parsed.retain(|id, _| *id >= first);
            self.expanded.retain(|id| *id >= first);
        }
    }

    fn reset(&mut self) {
        self.conversation.clear();
        self.parsed.clear();
        self.expanded.clear();
        self.born.clear();
        self.settled.clear();
        self.list.reset(0);
        self.list.set_follow_mode(FollowMode::Tail);
    }

    pub(super) fn dialect(&self) -> Dialect {
        match (&self.status, self.context.agent.as_deref()) {
            (Status::Following(dialect), _) => *dialect,
            (_, Some(agent)) => Dialect::of_agent(agent).unwrap_or(Dialect::Claude),
            _ => Dialect::Claude,
        }
    }

    /// How long the turn in progress has run, while the agent works.
    pub(super) fn elapsed(&self) -> Option<Duration> {
        if !self.context.working {
            return None;
        }
        let from_log = self.conversation.turn_started.and_then(|started| {
            let now = crate::spaces::now_millis();
            // A clock far behind the log's is no clock to trust.
            (now >= started).then(|| Duration::from_millis(now - started))
        });
        from_log.or_else(|| self.working_since.map(|since| since.elapsed()))
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text().trim().to_owned();
        if text.is_empty() {
            return;
        }
        cx.emit(ChatEvent::Send(text));
        self.input.update(cx, |input, cx| input.clear(cx));
        self.list.set_follow_mode(FollowMode::Tail);
    }

    pub(super) fn agent_name(&self) -> &str {
        match &self.status {
            Status::Following(dialect) => dialect.agent(),
            _ => self.context.agent.as_deref().unwrap_or("agent"),
        }
    }

    /// The agent's own colour: Claude's orange, Codex's periwinkle.
    pub(super) fn agent_color(&self) -> u32 {
        match self.dialect() {
            Dialect::Claude => tern::CLAUDE,
            Dialect::Codex => tern::CODEX,
        }
    }

    /// `claude  NATIVE  drawn by Herdr`. The tag switches back to the
    /// terminal.
    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let size = self.em() * 0.95;
        let tag = size * 0.78;
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(size * 0.55))
            .h(px(size * 2.75))
            .px(px(size * 1.1))
            .border_b_1()
            .border_color(color(tern::LINE))
            .text_size(px(size))
            .text_color(color(tern::INK_2))
            .whitespace_nowrap()
            .overflow_hidden()
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(color(self.agent_color()))
                    .child(self.agent_name().to_owned()),
            )
            .child(
                div()
                    .id("chat-show-terminal")
                    .debug_selector(|| "chat-show-terminal".into())
                    .flex_none()
                    .px(px(tag * 0.5))
                    .py(px(tag * 0.2))
                    .rounded(px(tag * 0.4))
                    .bg(color(tern::TAG_FILL))
                    .border_1()
                    .border_color(color(tern::TAG_EDGE))
                    .text_color(color(tern::TEAL))
                    .font(self.mono())
                    .text_size(px(tag))
                    .cursor_pointer()
                    .hover(|s| s.bg(color(0x34e4c933)))
                    .child("NATIVE")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(ChatEvent::ShowTerminal))),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(color(tern::DIM))
                    .child("drawn by Herdr"),
            )
    }

    /// `••• Working 10.4s`: three teal dots bouncing in turn and a light
    /// sweeping through the word, as Tern draws its working line.
    fn working_row(&self, window: &mut Window) -> Option<impl IntoElement> {
        let elapsed = self.elapsed()?;
        window.request_animation_frame();
        let size = self.em() * 0.95;
        let now = self.clock.elapsed().as_secs_f32();
        let dots = (0..3).map(|dot| {
            let cycle = tern::DOTS.as_secs_f32();
            let phase = ((now - dot as f32 * 0.16) / cycle).rem_euclid(1.);
            // Keyframes: 0% and 100% rest faint; 40% lifted and bright.
            let lift = if phase < 0.4 {
                tern::ease(phase / 0.4)
            } else {
                1. - tern::ease((phase - 0.4) / 0.6)
            };
            div()
                .size(px(size * 0.36))
                .rounded_full()
                .bg(color(tern::TEAL))
                .opacity(0.25 + 0.75 * lift)
                .relative()
                .top(px(-lift * size * 0.18))
        });
        // The shimmer: a band 2.5 words wide sweeps from right to left every
        // 2.2 s; its bright centre crosses the word once per sweep.
        let word = "Working";
        let sweep = (now / tern::SHIMMER.as_secs_f32()).fract();
        let centre = 3.5 - sweep * 3.75;
        let count = word.chars().count() as f32;
        let runs = word
            .char_indices()
            .enumerate()
            .map(|(index, (_, ch))| {
                let at = (index as f32 + 0.5) / count;
                let glow = (1. - (at - centre).abs() / 0.3).max(0.);
                TextRun {
                    len: ch.len_utf8(),
                    font: self.sans(),
                    color: color(tern::blend(tern::INK_2, 0xf2fffcff, glow)).into(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }
            })
            .collect();
        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(size * 0.6))
                .pt(px(size * 0.5))
                .px(px(size * 1.8))
                .text_size(px(size))
                .text_color(color(tern::INK_2))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(size * 0.22))
                        .children(dots),
                )
                .child(StyledText::new(word).with_runs(runs))
                .child(
                    div()
                        .font(self.mono())
                        .text_size(px(size * 0.9))
                        .text_color(color(tern::DIM))
                        .child(format!("{:.1}s", elapsed.as_secs_f32())),
                ),
        )
    }

    fn composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let size = self.em();
        let mut meta = vec![self.agent_name().to_owned()];
        if let Some(model) = &self.conversation.model {
            let model = model.strip_prefix("claude-").unwrap_or(model);
            meta.push(model.split('-').next().unwrap_or(model).to_owned());
        }
        if let Some(cwd) = &self.context.cwd {
            meta.push(super::render::short_path(cwd));
        }
        div()
            .id("chat-composer")
            .debug_selector(|| "chat-composer".into())
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .gap(px(size))
            .mt(px(size * 0.7))
            .mx(px(size * 1.6))
            .mb(px(size * 1.1))
            .px(px(size))
            .py(px(size * 0.62))
            .rounded(px(size * 0.9))
            .border_1()
            .border_color(color(tern::COMPOSER_EDGE))
            .text_size(px(size))
            .text_color(color(tern::DIM))
            .whitespace_nowrap()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let keystroke = &event.keystroke;
                if keystroke.key == "enter"
                    && !keystroke.modifiers.shift
                    && !this.input.read(cx).is_composing()
                {
                    this.send(cx);
                    cx.stop_propagation();
                    window.prevent_default();
                } else if keystroke.key == "escape" {
                    cx.emit(ChatEvent::ShowTerminal);
                    cx.stop_propagation();
                }
            }))
            .child(div().flex_1().min_w_0().child(self.input.clone()))
            .child(
                div()
                    .flex_shrink_1()
                    .min_w_0()
                    .truncate()
                    .font(self.mono())
                    .text_size(px(size * 0.85))
                    .child(meta.join(" · ")),
            )
    }

    fn empty_state(&self) -> impl IntoElement {
        let (headline, detail) = match &self.status {
            Status::Starting => ("Finding the session…".to_owned(), String::new()),
            Status::Following(_) => (
                "No messages yet".to_owned(),
                format!("Write to {} below.", self.agent_name()),
            ),
            Status::NoSession { agent: Some(agent) } if Dialect::of_agent(agent).is_none() => (
                format!("{agent} has no chat view yet"),
                "Chat reads Claude Code and Codex sessions.".to_owned(),
            ),
            Status::NoSession { .. } => (
                "No agent session reported".to_owned(),
                "Start Claude Code or Codex in this pane, or wait for it to report its session."
                    .to_owned(),
            ),
            Status::Failed(message) => ("Could not read the session".to_owned(), message.clone()),
        };
        let size = self.feed_em();
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(size * 0.4))
            .px(px(size * 1.7))
            .child(
                div()
                    .text_size(px(size * 1.05))
                    .text_color(color(tern::INK))
                    .child(headline),
            )
            .child(
                div()
                    .text_size(px(size * 0.9))
                    .text_color(color(tern::DIM))
                    .text_center()
                    .child(detail),
            )
    }
}

impl Render for ChatPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let em = self.em();
        let failure = match &self.status {
            Status::Failed(message) if !self.conversation.items.is_empty() => Some(message.clone()),
            _ => None,
        };
        let feed =
            if self.conversation.items.is_empty() {
                div()
                    .flex_1()
                    .min_h_0()
                    .child(self.empty_state())
                    .into_any_element()
            } else {
                // The feed fades out under the header, as Tern masks its top.
                let fade = self.feed_em() * 2.4;
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        list(
                            self.list.clone(),
                            cx.processor(|this, index, window, cx| {
                                this.render_item(index, window, cx)
                            }),
                        )
                        .size_full(),
                    )
                    .child(div().absolute().top_0().left_0().right_0().h(px(fade)).bg(
                        linear_gradient(
                            180.,
                            linear_color_stop(color(tern::TILE), 0.),
                            linear_color_stop(color(tern::TILE & 0xffffff00), 1.),
                        ),
                    ))
                    .into_any_element()
            };
        let working = self.working_row(window);
        div()
            .id("chat-panel")
            .debug_selector(|| "chat-panel".into())
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(em * 0.8))
            .border_1()
            .border_color(color(if self.context.focused {
                tern::TILE_FOCUS
            } else {
                tern::TILE_EDGE
            }))
            .bg(color(tern::TILE))
            .text_color(color(tern::INK))
            .font(self.sans())
            .text_size(px(em))
            .line_height(px(em * 1.35))
            .child(self.header(cx))
            .child(feed)
            .children(failure.map(|message| {
                div()
                    .px(px(em * 1.8))
                    .pt(px(em * 0.4))
                    .text_size(px(em * 0.86))
                    .text_color(color(tern::FAIL_INK))
                    .truncate()
                    .child(message)
            }))
            .children(working)
            .child(self.composer(cx))
    }
}
