//! Annotating a page in a window: the picker's reports, the notes panel
//! beside the page, and sending the notes to the agent that opened it. The
//! notes are written in the app, never in the page, and nothing reaches an
//! agent until the user presses Send.
use super::{
    Feedback, Tab, TabId,
    annotate::{self, Anchor, MAX_NOTES, Note, Rect, Report},
    feedback::Batch,
};
/// The notes panel's width beside the page.
pub(super) const ANNOTATIONS_WIDTH: f32 = 300.;

use crate::{
    HerdrWindow,
    connection::ConnectionBridge,
    motion::{self, ENTER, Toggle},
    search_input::SearchInput,
    terminal::InputTarget,
    window::Flash,
};
use gpui::{prelude::*, *};
use herdr_client::protocol::{AgentStatus, ClientPaneInputEvent, ClientShellSnapshot};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

/// How long a batch waits for a busy agent before it is pasted anyway, or,
/// when the agent is asking a question, kept for `browser feedback`.
const HOLD: Duration = Duration::from_secs(120);
/// Lets the agent's input take the paste before Enter submits it.
const SUBMIT_DELAY: Duration = Duration::from_millis(150);

/// Screenshots older than this are removed when the next ones are saved.
const SCREENSHOT_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// What the next note will be about, picked but not yet written, and its
/// screenshot once WebKit delivers it.
struct Draft {
    anchor: Anchor,
    image: Option<Arc<Image>>,
    capture: Option<u64>,
}

#[derive(Default)]
struct TabNotes {
    /// Whether the picker is running in the page.
    armed: bool,
    /// Whether a drag draws a region rather than selecting text.
    regions: bool,
    pending: Option<Draft>,
    notes: Vec<Note>,
}

/// Notes on the way to an agent's pane, waiting for it to be idle.
struct Delivery {
    pane_id: String,
    boot_id: String,
    text: String,
    until: Instant,
}

pub(crate) struct Annotations {
    tabs: HashMap<TabId, TabNotes>,
    /// Each tab's notes panel, sliding open and closed beside its page.
    panels: HashMap<TabId, Toggle>,
    /// When each note a tab's panel draws was added, while it grows into the
    /// list; `None` for a note that is whole.
    drawn: HashMap<TabId, Vec<Option<Instant>>>,
    pub(super) input: Entity<SearchInput>,
    deliveries: Vec<Delivery>,
    /// Numbers screenshots, to match each to its draft or note.
    captures: u64,
}

impl Annotations {
    pub(crate) fn new(cx: &mut App) -> Self {
        let input = cx.new(|cx| {
            let mut input = SearchInput::new(cx);
            input.set_placeholder("Describe the change\u{2026}", cx);
            input
        });
        Self {
            tabs: HashMap::new(),
            panels: HashMap::new(),
            drawn: HashMap::new(),
            input,
            deliveries: Vec::new(),
            captures: 0,
        }
    }

    pub(crate) fn armed(&self, id: TabId) -> bool {
        self.tabs.get(&id).is_some_and(|tab| tab.armed)
    }

    /// Whether the notes panel shows beside the page.
    pub(crate) fn open(&self, id: TabId) -> bool {
        self.tabs
            .get(&id)
            .is_some_and(|tab| tab.armed || tab.pending.is_some() || !tab.notes.is_empty())
    }

    pub(crate) fn ids(&self) -> impl Iterator<Item = TabId> + '_ {
        self.tabs.keys().copied()
    }

    #[cfg(test)]
    pub(super) fn queued(&self, id: TabId) -> usize {
        self.tabs.get(&id).map_or(0, |tab| tab.notes.len())
    }

    #[cfg(test)]
    pub(super) fn delivering(&self) -> usize {
        self.deliveries.len()
    }

    pub(crate) fn forget(&mut self, id: TabId) {
        self.tabs.remove(&id);
        self.panels.remove(&id);
        self.drawn.remove(&id);
    }

    #[cfg(test)]
    fn tab_notes_mut(&mut self, id: TabId) -> &mut TabNotes {
        self.tabs.entry(id).or_default()
    }

    /// How much of `id`'s notes panel shows at `now`, as it slides open or
    /// closed. A panel already open when first drawn is simply there.
    pub(crate) fn panel_shown(&mut self, id: TabId, now: Instant) -> f32 {
        let open = self.open(id);
        match self.panels.get_mut(&id) {
            Some(panel) => panel.set(open, now, false),
            None => {
                let mut panel = Toggle::default();
                panel.set(open, now, true);
                self.panels.insert(id, panel);
            }
        }
        self.panels.get(&id).map_or(0., |panel| panel.shown(now))
    }

    /// Records how many notes `id`'s panel draws at `now`; notes added
    /// since the last draw start growing in. Notes there when the panel first
    /// drew are whole.
    fn observe_notes(&mut self, id: TabId, count: usize, now: Instant) {
        let added = self.drawn.entry(id).or_insert_with(|| vec![None; count]);
        added.truncate(count);
        added.resize(count, Some(now));
    }

    /// How far note `index` of `id` has grown in at `now`, or `None` once
    /// it is whole.
    fn note_growth(&self, id: TabId, index: usize, now: Instant) -> Option<f32> {
        let since = (*self.drawn.get(&id)?.get(index)?)?;
        motion::progress(since, now, ENTER)
    }

    /// Whether a panel or a note is still moving, so the window draws
    /// another frame.
    pub(crate) fn moving(&self, now: Instant) -> bool {
        self.panels.values().any(|panel| panel.moving(now))
            || self
                .drawn
                .values()
                .flatten()
                .flatten()
                .any(|since| motion::progress(*since, now, ENTER).is_some())
    }
}

/// The agent in `pane_id` and what it is doing, if Herdr sees one there.
fn agent<'a>(
    snapshot: &'a ClientShellSnapshot,
    pane_id: &str,
) -> Option<&'a herdr_client::protocol::ClientShellAgent> {
    snapshot
        .agents
        .iter()
        .find(|agent| agent.pane_id == pane_id)
}

/// Writes the notes' screenshots where the agent can read them, returning
/// each note's file. They live in the app's private state folder, and ones
/// older than a week are removed first. Blocking; run off the UI thread.
fn save_screenshots(images: &[Option<Arc<Image>>]) -> crate::Result<Vec<Option<PathBuf>>> {
    let dir = crate::preferences::state_dir()
        .ok_or(crate::Error::MissingStateRoot)?
        .join("annotations");
    save_screenshots_in(&dir, images, SystemTime::now())
}

fn save_screenshots_in(
    dir: &std::path::Path,
    images: &[Option<Arc<Image>>],
    now: SystemTime,
) -> crate::Result<Vec<Option<PathBuf>>> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > SCREENSHOT_AGE);
            if old {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let stamp = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or_default();
    images
        .iter()
        .enumerate()
        .map(|(index, image)| {
            let Some(image) = image else {
                return Ok(None);
            };
            let path = dir.join(format!("note-{stamp}-{}.png", index + 1));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            use std::io::Write as _;
            options.open(&path)?.write_all(&image.bytes)?;
            Ok(Some(path))
        })
        .collect()
}

fn enter() -> ClientPaneInputEvent {
    crate::input::enter_key()
}

impl HerdrWindow {
    fn tab_notes(&mut self, id: TabId) -> &mut TabNotes {
        self.browser.annotations.tabs.entry(id).or_default()
    }

    /// Starts the picker in the page again, drawing the queued notes. A page
    /// loses it whenever it navigates.
    pub(crate) fn arm_page(&mut self, id: TabId, cx: &mut Context<Self>) {
        #[cfg(any(target_os = "macos", windows))]
        {
            let tab = self.tab_notes(id);
            let script = annotate::arm_script(&tab.notes);
            let regions = tab.regions;
            self.browser.pages.script(id, &script, cx);
            if regions {
                self.browser
                    .pages
                    .script(id, &annotate::mode_script(true), cx);
            }
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        let _ = (id, cx);
    }

    fn disarm_page(&mut self, id: TabId, cx: &mut Context<Self>) {
        #[cfg(any(target_os = "macos", windows))]
        self.browser.pages.script(id, annotate::disarm_script(), cx);
        #[cfg(not(any(target_os = "macos", windows)))]
        let _ = (id, cx);
    }

    pub(crate) fn toggle_annotating(
        &mut self,
        id: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let notes = self.tab_notes(id);
        notes.armed = !notes.armed;
        if notes.armed {
            self.arm_page(id, cx);
            self.show_flash(
                Flash::success("Click an element or select text to note it"),
                cx,
            );
        } else {
            notes.pending = None;
            self.disarm_page(id, cx);
            window.focus(&self.focus, cx);
        }
        cx.notify();
    }

    /// Switches the picker between picking elements and drawing regions.
    fn toggle_regions(&mut self, id: TabId, cx: &mut Context<Self>) {
        let tab = self.tab_notes(id);
        tab.regions = !tab.regions;
        let regions = tab.regions;
        #[cfg(any(target_os = "macos", windows))]
        self.browser
            .pages
            .script(id, &annotate::mode_script(regions), cx);
        let _ = regions;
        cx.notify();
    }

    /// The picker lost its page to a navigation; put it back.
    pub(crate) fn page_loaded(&mut self, id: TabId, cx: &mut Context<Self>) {
        if self.browser.annotations.armed(id) {
            self.arm_page(id, cx);
        }
    }

    /// Applies what the picker posted. Only a tab being annotated listens,
    /// and a pick only fills the draft: the user still writes and sends.
    pub(crate) fn page_posted(
        &mut self,
        id: TabId,
        body: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.browser.annotations.armed(id) {
            return;
        }
        match Report::parse(body) {
            Some(Report::Picked { anchor, shot }) => {
                self.begin_note(id, anchor, shot, window, cx);
            }
            Some(Report::Cancelled) => {
                if self.tab_notes(id).pending.take().is_none() {
                    self.toggle_annotating(id, window, cx);
                }
                cx.notify();
            }
            None => tracing::debug!("Ignored a malformed annotation message"),
        }
    }

    pub(super) fn begin_note(
        &mut self,
        id: TabId,
        anchor: Anchor,
        shot: Option<Rect>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tab_notes(id).notes.len() >= MAX_NOTES {
            self.reveal_page(id, cx);
            self.show_flash(
                Flash::warning("Send or remove notes before adding more"),
                cx,
            );
            return;
        }
        // The picker hid its overlay for the screenshot; WebKit takes it
        // after the next screen update, then the overlay comes back.
        let capture = shot.and_then(|rect| {
            self.browser.annotations.captures += 1;
            let capture = self.browser.annotations.captures;
            #[cfg(any(target_os = "macos", windows))]
            let asked = self.browser.pages.capture(id, rect, capture, cx);
            #[cfg(not(any(target_os = "macos", windows)))]
            let asked = {
                let _ = rect;
                false
            };
            asked.then_some(capture)
        });
        if capture.is_none() {
            self.reveal_page(id, cx);
        }
        self.tab_notes(id).pending = Some(Draft {
            anchor,
            image: None,
            capture,
        });
        // The page holds the keyboard natively; the note is typed here.
        #[cfg(any(target_os = "macos", windows))]
        self.browser.pages.blur(id, cx);
        let input = self.browser.annotations.input.clone();
        input.update(cx, |input, cx| input.clear(cx));
        let focus = input.read(cx).focus.clone();
        window.focus(&focus, cx);
        cx.notify();
    }

    fn reveal_page(&mut self, id: TabId, cx: &mut Context<Self>) {
        #[cfg(any(target_os = "macos", windows))]
        self.browser.pages.script(id, annotate::reveal_script(), cx);
        #[cfg(not(any(target_os = "macos", windows)))]
        let _ = (id, cx);
    }

    /// A screenshot arrived from WebKit: bring the overlay back, turn the
    /// capture into a PNG off the UI thread, and give it to its draft or note.
    #[cfg(target_os = "macos")]
    pub(crate) fn page_captured(
        &mut self,
        id: TabId,
        capture: u64,
        tiff: Option<Vec<u8>>,
        cx: &mut Context<Self>,
    ) {
        self.reveal_page(id, cx);
        let Some(tiff) = tiff else {
            return;
        };
        let png = cx
            .background_executor()
            .spawn(async move { super::snapshot::png(&tiff) });
        cx.spawn(async move |this, cx| {
            let Some(png) = png.await else {
                return;
            };
            let image = Arc::new(Image::from_bytes(ImageFormat::Png, png));
            this.update(cx, |this, cx| {
                this.attach_screenshot(id, capture, image);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    #[cfg(target_os = "macos")]
    fn attach_screenshot(&mut self, id: TabId, capture: u64, image: Arc<Image>) {
        let Some(tab) = self.browser.annotations.tabs.get_mut(&id) else {
            return;
        };
        if let Some(draft) = tab
            .pending
            .as_mut()
            .filter(|draft| draft.capture == Some(capture))
        {
            draft.image = Some(image);
            draft.capture = None;
        } else if let Some(note) = tab
            .notes
            .iter_mut()
            .find(|note| note.capture == Some(capture))
        {
            note.image = Some(image);
            note.capture = None;
        }
    }

    pub(super) fn add_note(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.browser.annotations.input.read(cx).text().to_owned();
        let Some(draft) = self.tab_notes(id).pending.take() else {
            return;
        };
        let Some(mut note) = Note::new(draft.anchor.clone(), &text) else {
            self.tab_notes(id).pending = Some(draft);
            self.show_flash(Flash::warning("Write what should change first"), cx);
            return;
        };
        note.image = draft.image;
        note.capture = draft.capture;
        let notes = self.tab_notes(id);
        notes.notes.push(note);
        self.browser
            .annotations
            .input
            .update(cx, |input, cx| input.clear(cx));
        self.refresh_markers(id, cx);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn refresh_markers(&mut self, id: TabId, cx: &mut Context<Self>) {
        if self.browser.annotations.armed(id) {
            self.arm_page(id, cx);
        }
    }

    fn remove_note(&mut self, id: TabId, index: usize, cx: &mut Context<Self>) {
        let notes = self.tab_notes(id);
        if index < notes.notes.len() {
            notes.notes.remove(index);
        }
        self.refresh_markers(id, cx);
        cx.notify();
    }

    /// The queued notes as a prompt, after their screenshots are saved to
    /// files the agent can read. Saving runs off the UI thread; `then` gets
    /// the prompt back on it.
    fn with_notes_prompt(
        &mut self,
        tab: &Tab,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, String, &mut Context<Self>) + 'static,
    ) {
        let notes = self.tab_notes(tab.id).notes.clone();
        if notes.is_empty() {
            return;
        }
        let reload = crate::control::reload_command();
        if notes.iter().all(|note| note.image.is_none()) {
            let text = annotate::prompt(tab, &notes, &[], &reload);
            then(self, text, cx);
            return;
        }
        let images: Vec<Option<Arc<Image>>> = notes.iter().map(|note| note.image.clone()).collect();
        let saving = cx
            .background_executor()
            .spawn(async move { save_screenshots(&images) });
        let tab = tab.clone();
        cx.spawn(async move |this, cx| {
            let paths = saving.await;
            this.update(cx, |this, cx| {
                let paths = paths.unwrap_or_else(|error| {
                    tracing::warn!(%error, "Could not save note screenshots");
                    this.show_flash(Flash::warning("Screenshots could not be saved"), cx);
                    Vec::new()
                });
                let text = annotate::prompt(&tab, &notes, &paths, &reload);
                then(this, text, cx);
            })
            .ok();
        })
        .detach();
    }

    fn clear_notes(&mut self, id: TabId, cx: &mut Context<Self>) {
        self.tab_notes(id).notes.clear();
        self.refresh_markers(id, cx);
        cx.notify();
    }

    fn copy_notes(&mut self, tab: &Tab, cx: &mut Context<Self>) {
        self.with_notes_prompt(tab, cx, |this, text, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            this.show_flash(Flash::success("Notes copied"), cx);
        });
    }

    /// Where Send delivers: the pane of the agent that opened the page,
    /// while this window shows that pane's daemon.
    fn origin_pane<'a>(&'a self, tab: &'a Tab) -> Option<(&'a str, &'a ClientShellSnapshot)> {
        let pane = tab.origin.as_deref()?;
        let snapshot = self.live.snapshot.as_deref()?;
        let here = super::view::scope(&self.endpoints[self.selected_endpoint]) == tab.scope;
        (here
            && snapshot
                .panes
                .iter()
                .any(|candidate| candidate.pane_id == pane))
        .then_some((pane, snapshot))
    }

    /// Sends the queued notes to the agent that opened the page: to it
    /// directly when it waits in `browser feedback`, otherwise into its pane
    /// once it is idle, and kept for `browser feedback` when its pane is gone.
    pub(super) fn send_notes(&mut self, tab: &Tab, cx: &mut Context<Self>) {
        // The queue is cleared at once, so a second Send cannot repeat it
        // while screenshots are still being saved.
        let owned = tab.clone();
        self.with_notes_prompt(tab, cx, move |this, text, cx| {
            this.deliver_notes(&owned, text, cx);
        });
        self.clear_notes(tab.id, cx);
    }

    fn deliver_notes(&mut self, tab: &Tab, text: String, cx: &mut Context<Self>) {
        let Some(pane_id) = tab.origin.clone() else {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.show_flash(
                Flash::success("No agent opened this page, so the notes were copied"),
                cx,
            );
            return;
        };
        let waiting = cx
            .try_global::<Feedback>()
            .is_some_and(|feedback| feedback.is_waiting(&pane_id));
        let flash = if waiting {
            cx.default_global::<Feedback>()
                .keep(Batch { pane_id, text });
            Flash::success("Notes sent to the waiting agent")
        } else if let Some((pane, snapshot)) = self
            .origin_pane(tab)
            .filter(|(pane, snapshot)| agent(snapshot, pane).is_some())
        {
            let busy = agent(snapshot, pane).is_some_and(|agent| {
                matches!(
                    agent.agent_status,
                    AgentStatus::Working | AgentStatus::Blocked
                )
            });
            let delivery = Delivery {
                pane_id: pane.to_owned(),
                boot_id: snapshot.boot_id.clone(),
                text,
                until: Instant::now() + HOLD,
            };
            self.browser.annotations.deliveries.push(delivery);
            if busy {
                Flash::success("Notes will go to the agent once it is idle")
            } else {
                Flash::success("Notes sent to the agent")
            }
        } else if self.origin_pane(tab).is_some() {
            // A shell, not an agent: Enter there would run the notes.
            cx.default_global::<Feedback>()
                .keep(Batch { pane_id, text });
            Flash::warning("No agent runs in that pane; notes kept for `browser feedback`")
        } else {
            cx.default_global::<Feedback>()
                .keep(Batch { pane_id, text });
            Flash::warning("The agent's pane is not here; notes kept for `browser feedback`")
        };
        self.show_flash(flash, cx);
    }

    /// Pastes held notes into agents that became idle. Runs every tick.
    pub(crate) fn poll_deliveries(&mut self, cx: &mut Context<Self>) {
        if self.browser.annotations.deliveries.is_empty() {
            return;
        }
        let now = Instant::now();
        let deliveries = std::mem::take(&mut self.browser.annotations.deliveries);
        for delivery in deliveries {
            let waiting = cx
                .try_global::<Feedback>()
                .is_some_and(|feedback| feedback.is_waiting(&delivery.pane_id));
            let snapshot = self.live.snapshot.clone();
            let present = snapshot.as_deref().filter(|snapshot| {
                snapshot.boot_id == delivery.boot_id
                    && snapshot
                        .panes
                        .iter()
                        .any(|pane| pane.pane_id == delivery.pane_id)
            });
            let status = present
                .and_then(|snapshot| agent(snapshot, &delivery.pane_id))
                .map(|agent| agent.agent_status);
            let busy = matches!(status, Some(AgentStatus::Working | AgentStatus::Blocked));
            if present.is_some() && !waiting && busy && now < delivery.until {
                self.browser.annotations.deliveries.push(delivery);
                continue;
            }
            // Only a running agent's prompt is typed into. A pane whose agent
            // exited is a shell, where Enter would run the pasted text, page
            // quotes included; an agent asking the user something must not
            // have its answer typed by a paste. Both fetch the notes instead.
            let typable = matches!(
                status,
                Some(AgentStatus::Idle | AgentStatus::Done | AgentStatus::Working)
            );
            if waiting || present.is_none() || !typable {
                cx.default_global::<Feedback>().keep(Batch {
                    pane_id: delivery.pane_id,
                    text: delivery.text,
                });
                if !waiting {
                    self.show_flash(
                        Flash::warning("The agent is not ready; notes kept for `browser feedback`"),
                        cx,
                    );
                }
                continue;
            }
            self.paste_into_pane(delivery, cx);
        }
    }

    fn paste_into_pane(&mut self, delivery: Delivery, cx: &mut Context<Self>) {
        let target = InputTarget::Pane(delivery.pane_id.clone());
        let pasted = self.endpoints[self.selected_endpoint]
            .connection
            .handle
            .as_ref()
            .ok_or(crate::Error::NotConnected)
            .and_then(|handle| {
                ConnectionBridge::send_input(
                    handle,
                    &delivery.boot_id,
                    &target,
                    ClientPaneInputEvent::Paste(delivery.text.clone()),
                )
                .map_err(crate::Error::from)
            });
        if let Err(error) = pasted {
            tracing::warn!(%error, "Could not paste notes into the agent's pane");
            cx.default_global::<Feedback>().keep(Batch {
                pane_id: delivery.pane_id,
                text: delivery.text,
            });
            self.show_flash(
                Flash::warning("Could not reach the agent; notes kept for `browser feedback`"),
                cx,
            );
            return;
        }
        let timer = cx.background_executor().clone();
        let boot_id = delivery.boot_id;
        cx.spawn(async move |this, cx| {
            timer.timer(SUBMIT_DELAY).await;
            this.update(cx, |this, _| {
                let handle = this.endpoints[this.selected_endpoint]
                    .connection
                    .handle
                    .as_ref();
                if let Some(handle) = handle
                    && let Err(error) =
                        ConnectionBridge::send_input(handle, &boot_id, &target, enter())
                {
                    tracing::warn!(%error, "Could not submit notes in the agent's pane");
                }
            })
            .ok();
        })
        .detach();
    }

    /// The notes panel beside the page.
    pub(crate) fn render_annotations(&mut self, tab: &Tab, cx: &mut Context<Self>) -> AnyElement {
        let id = tab.id;
        let theme = self.theme.clone();
        let armed = self.browser.annotations.armed(id);
        let notes = self.tab_notes(id);
        let regions = notes.regions;
        let pending = notes.pending.as_ref().map(|draft| {
            (
                draft.anchor.summary(),
                draft.image.clone(),
                draft.capture.is_some(),
            )
        });
        let list: Vec<(usize, Note)> = notes.notes.iter().cloned().enumerate().collect();
        let now = Instant::now();
        self.browser.annotations.observe_notes(id, list.len(), now);
        let origin = tab.origin.is_some();
        let tab_for_send = tab.clone();
        let tab_for_copy = tab.clone();
        let button = |id: &'static str, label: &'static str, primary: bool| {
            let background = if primary {
                theme.primary()
            } else {
                theme.active
            };
            div()
                .id(id)
                .debug_selector(move || id.into())
                .px_2()
                .py_1()
                .rounded(px(crate::config::corners::CONTROL))
                .cursor_pointer()
                .bg(rgb(background))
                .text_color(rgb(theme.text_on(background)))
                .child(label)
        };
        let thumbnail = |image: Arc<Image>| {
            img(image)
                .max_w_full()
                .max_h(px(96.))
                .rounded(px(crate::config::corners::CONTROL))
                .border_1()
                .border_color(rgb(theme.active))
        };
        let composer = pending.map(|(summary, image, capturing)| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .border_b_1()
                .border_color(rgb(theme.active))
                .child(div().text_color(rgb(theme.muted)).truncate().child(summary))
                .children(image.map(thumbnail))
                .when(capturing, |draft| {
                    draft.child(
                        div()
                            .text_color(rgb(theme.muted))
                            .child("Taking a screenshot\u{2026}"),
                    )
                })
                .child(
                    div()
                        .id("annotation-input")
                        .debug_selector(|| "annotation-input".into())
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                            match event.keystroke.key.as_str() {
                                "enter" => this.add_note(id, window, cx),
                                "escape" => {
                                    this.tab_notes(id).pending = None;
                                    window.focus(&this.focus, cx);
                                    cx.notify();
                                }
                                _ => return,
                            }
                            cx.stop_propagation();
                        }))
                        .child(self.browser.annotations.input.clone()),
                )
                .child(div().flex().gap_1().child(
                    button("annotation-add", "Add note", true).on_click(
                        cx.listener(move |this, _, window, cx| this.add_note(id, window, cx)),
                    ),
                ))
        });
        let growth: Vec<Option<f32>> = (0..list.len())
            .map(|index| self.browser.annotations.note_growth(id, index, now))
            .collect();
        let rows = list.into_iter().map(|(index, note)| {
            div()
                .id(("annotation-note", index))
                // A new note opens into the list and fades in.
                .when_some(growth[index], |row, k| {
                    row.max_h(px(320. * k)).overflow_hidden().opacity(k)
                })
                .flex()
                .gap_2()
                .p_2()
                .border_b_1()
                .border_color(rgb(theme.active))
                .child(
                    div()
                        .flex_none()
                        .size(px(18.))
                        .rounded_full()
                        .bg(rgb(theme.palette[3]))
                        .text_color(rgb(theme.text_on(theme.palette[3])))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child((index + 1).to_string()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_color(rgb(theme.muted))
                                .truncate()
                                .child(note.anchor.summary()),
                        )
                        .child(div().child(note.comment))
                        .children(note.image.clone().map(thumbnail)),
                )
                .child(
                    div()
                        .id(("annotation-remove", index))
                        .flex_none()
                        .size(px(18.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .rounded(px(crate::config::corners::CONTROL))
                        .hover(|s| s.bg(rgb(theme.active)))
                        .child(
                            svg()
                                .path("icons/close.svg")
                                .size(px(12.))
                                .text_color(rgb(theme.muted)),
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.remove_note(id, index, cx)),
                        ),
                )
        });
        let has_notes = !self.tab_notes(id).notes.is_empty();
        div()
            .id("annotations")
            .debug_selector(|| "annotations".into())
            .flex_none()
            .w(px(ANNOTATIONS_WIDTH))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(theme.surface))
            .border_l_1()
            .border_color(rgb(theme.active))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .p_2()
                    .border_b_1()
                    .border_color(rgb(theme.active))
                    .child("Notes")
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                // Drag draws a region while this is on;
                                // Shift-drag draws one either way.
                                button("annotation-region", "Region", regions).on_click(
                                    cx.listener(move |this, _, _, cx| this.toggle_regions(id, cx)),
                                ),
                            )
                            .child(button("annotation-page", "Note on page", false).on_click(
                                cx.listener(move |this, _, window, cx| {
                                    this.begin_note(id, Anchor::Page, None, window, cx);
                                }),
                            )),
                    ),
            )
            .children(composer)
            .child(
                div()
                    .id("annotation-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows)
                    .when(!has_notes && armed, |list| {
                        list.child(
                            div()
                                .p_2()
                                .text_color(rgb(theme.muted))
                                .child("Click an element, select text, or Shift-drag a region in the page, then describe the change."),
                        )
                    }),
            )
            .when(has_notes, |panel| {
                panel.child(
                    div()
                        .flex()
                        .gap_1()
                        .p_2()
                        .border_t_1()
                        .border_color(rgb(theme.active))
                        .when(origin, |row| {
                            row.child(button("annotation-send", "Send to agent", true).on_click(
                                cx.listener(move |this, _, _, cx| this.send_notes(&tab_for_send, cx)),
                            ))
                        })
                        .child(button("annotation-copy", "Copy", !origin).on_click(cx.listener(
                            move |this, _, _, cx| this.copy_notes(&tab_for_copy, cx),
                        ))),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{Annotations, ENTER, Instant, SCREENSHOT_AGE, TabId, save_screenshots_in};
    use gpui::{Image, ImageFormat};
    use std::{
        sync::Arc,
        time::{Duration, SystemTime},
    };

    #[gpui::test]
    fn new_notes_grow_in_and_the_panel_slides(cx: &mut gpui::TestAppContext) {
        let id = TabId::test(7);
        cx.update(|cx| {
            let mut annotations = Annotations::new(cx);
            let start = Instant::now();
            // Notes a panel has when it first draws are whole.
            annotations.observe_notes(id, 2, start);
            assert_eq!(annotations.note_growth(id, 1, start), None);
            assert!(!annotations.moving(start));
            // One added since grows in, then is whole.
            annotations.observe_notes(id, 3, start);
            assert_eq!(annotations.note_growth(id, 2, start), Some(0.));
            assert_eq!(annotations.note_growth(id, 1, start), None);
            assert!(annotations.moving(start));
            assert_eq!(annotations.note_growth(id, 2, start + ENTER), None);
            // Sent notes leave; the next one grows in again.
            annotations.observe_notes(id, 0, start + ENTER);
            annotations.observe_notes(id, 1, start + ENTER);
            assert!(annotations.note_growth(id, 0, start + ENTER).is_some());

            // A closed panel first drawn closed shows nothing; opening slides.
            let page = TabId::test(8);
            assert_eq!(annotations.panel_shown(page, start), 0.);
            annotations.tab_notes_mut(page).armed = true;
            assert_eq!(annotations.panel_shown(page, start), 0.);
            assert_eq!(annotations.panel_shown(page, start + ENTER), 1.);
            annotations.forget(page);
            annotations.forget(id);
            assert!(!annotations.moving(start + ENTER));
        });
    }

    #[test]
    fn screenshots_are_private_files_and_old_ones_go() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("note-1-1.png");
        std::fs::write(&stale, b"old").unwrap();
        let week_ago = SystemTime::now() - SCREENSHOT_AGE - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(week_ago)
            .unwrap();
        let image = Arc::new(Image::from_bytes(ImageFormat::Png, b"png bytes".to_vec()));
        let paths =
            save_screenshots_in(dir.path(), &[None, Some(image)], SystemTime::now()).unwrap();
        assert!(paths[0].is_none());
        let saved = paths[1].as_ref().unwrap();
        assert_eq!(std::fs::read(saved).unwrap(), b"png bytes");
        assert!(
            saved
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with("-2.png")
        );
        assert!(!stale.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &std::path::Path| {
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777
            };
            assert_eq!(mode(saved), 0o600);
            assert_eq!(mode(dir.path()), 0o700);
        }
    }
}
