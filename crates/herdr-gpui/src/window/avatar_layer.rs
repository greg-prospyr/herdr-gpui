//! Space avatars, painted by the window over the cached sidebar.
//!
//! Notifying any view inside the sidebar would rebuild every row, since GPUI
//! invalidates a notified view's ancestors. So the sidebar only reserves a
//! slot for each avatar, recording where it laid out and how it is clipped,
//! and the window paints all of them in one layer above it. An animation
//! frame then redraws the window through its own signal, the way terminal
//! output does, and the sidebar keeps its cached layout.
//!
//! Slots are shared rather than returned: when the sidebar's cache holds,
//! its slots from the last layout are still where its rows are.

use super::{HerdrWindow, SurfaceSignal};
use crate::{
    pixel_avatar::{self, AvatarStyle, Images, Motion, Wake},
    spaces::{Recency, SpaceKey, now_millis},
};
use gpui::{prelude::*, *};
use herdr_client::protocol::AgentStatus;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    time::Instant,
};

#[derive(Clone)]
pub(crate) struct AvatarSlot {
    pub key: SpaceKey,
    pub style: AvatarStyle,
    pub bounds: Bounds<Pixels>,
    /// The scroll area's clip when the slot laid out.
    pub clip: Bounds<Pixels>,
}

pub(crate) type Slots = Rc<RefCell<Vec<AvatarSlot>>>;

pub(crate) struct AvatarLayer {
    slots: Slots,
    /// Shared with the paint closure, which runs after render returns.
    motions: Rc<RefCell<HashMap<SpaceKey, Motion>>>,
    /// Each endpoint's snapshot last observed, so an unchanged projection
    /// costs nothing on the many renders terminal output causes.
    observed: HashMap<String, (String, u64)>,
    /// Each endpoint's focused space last observed; a change is a touch.
    focused: HashMap<String, String>,
    /// Rendered moons, shared with the paint closure and the picker.
    images: Rc<RefCell<Images>>,
    /// Loops run on this clock, so every working avatar steps at once.
    clock: Instant,
    signal: Entity<SurfaceSignal>,
    /// The pending timer's deadline, which a sooner wake replaces.
    timer: Option<(Instant, Task<()>)>,
}

impl AvatarLayer {
    pub(crate) fn new(cx: &mut App) -> Self {
        Self {
            slots: Rc::default(),
            motions: Rc::default(),
            observed: HashMap::new(),
            focused: HashMap::new(),
            images: Rc::default(),
            clock: Instant::now(),
            signal: cx.new(|_| SurfaceSignal),
            timer: None,
        }
    }

    /// The rendered moons, for anything else that paints one.
    pub(crate) fn images(&self) -> Rc<RefCell<Images>> {
        self.images.clone()
    }

    pub(crate) fn slots(&self) -> Slots {
        self.slots.clone()
    }

    /// Takes the daemon's latest status for every space in `spaces`, which
    /// are all of one endpoint's, forgetting that endpoint's departed spaces.
    fn observe<'a>(
        &mut self,
        endpoint: &str,
        spaces: impl IntoIterator<Item = (&'a str, AgentStatus)>,
        now: Instant,
    ) {
        let mut seen = HashSet::new();
        let mut motions = self.motions.borrow_mut();
        for (id, status) in spaces {
            let key = SpaceKey::new(endpoint, id);
            motions
                .entry(key.clone())
                .and_modify(|motion| motion.observe(status, now))
                .or_insert_with(|| Motion::new(status));
            seen.insert(key);
        }
        motions.retain(|key, _| key.endpoint != endpoint || seen.contains(key));
    }

    /// Whether `snapshot` is newer than the one last observed for `endpoint`.
    fn changed(&mut self, endpoint: &str, boot: &str, revision: u64) -> bool {
        if self
            .observed
            .get(endpoint)
            .is_some_and(|(seen, at)| seen == boot && *at == revision)
        {
            return false;
        }
        self.observed
            .insert(endpoint.to_owned(), (boot.to_owned(), revision));
        true
    }

    /// Whether `endpoint`'s focused space differs from the last one seen.
    fn focus_moved(&mut self, endpoint: &str, focused: &str) -> bool {
        if self
            .focused
            .get(endpoint)
            .is_some_and(|seen| seen == focused)
        {
            return false;
        }
        self.focused.insert(endpoint.to_owned(), focused.to_owned());
        true
    }

    /// When a visible avatar next changes.
    fn wake(&self, now: Instant) -> Wake {
        let motions = self.motions.borrow();
        self.slots
            .borrow()
            .iter()
            .filter_map(|slot| motions.get(&slot.key))
            .fold(Wake::Never, |wake, motion| {
                wake.min(motion.wake(now, self.clock))
            })
    }
}

#[cfg(test)]
impl AvatarLayer {
    /// The spaces whose avatars the last sidebar layout reserved, in order.
    pub(crate) fn slot_ids(&self) -> Vec<String> {
        self.slots
            .borrow()
            .iter()
            .map(|slot| slot.key.id.clone())
            .collect()
    }

    /// The keys of those slots, machine included.
    pub(crate) fn slot_keys(&self) -> Vec<SpaceKey> {
        self.slots
            .borrow()
            .iter()
            .map(|slot| slot.key.clone())
            .collect()
    }

    /// What a wake does: redraw the window without notifying it.
    pub(crate) fn redraw(&self, cx: &mut App) {
        self.signal.update(cx, |_, cx| cx.notify());
    }

    pub(crate) fn next_wake(&self, now: Instant) -> Wake {
        self.wake(now)
    }
}

impl HerdrWindow {
    /// Reads each endpoint's newest snapshot once: the spaces' statuses for
    /// their avatars, and a newly focused space as touched now. Runs whatever
    /// the order, so switching to attention already knows what you touched.
    fn observe_spaces(&mut self, now: Instant, cx: &mut App) {
        let layer = &mut self.space_avatars;
        for (index, endpoint) in self.endpoints.iter().enumerate() {
            let live = if index == self.selected_endpoint {
                &self.live
            } else {
                &endpoint.live
            };
            let Some(snapshot) = &live.snapshot else {
                continue;
            };
            if !layer.changed(&endpoint.id, &snapshot.boot_id, snapshot.revision) {
                continue;
            }
            layer.observe(
                &endpoint.id,
                snapshot
                    .workspaces
                    .iter()
                    .map(|space| (space.workspace_id.as_str(), space.agent_status)),
                now,
            );
            if let Some(focused) = snapshot.workspaces.iter().find(|space| space.focused)
                && layer.focus_moved(&endpoint.id, &focused.workspace_id)
            {
                Recency::touch(
                    cx,
                    SpaceKey::new(&endpoint.id, &focused.workspace_id),
                    now_millis(),
                );
            }
        }
    }

    /// Called from render: observes the snapshots, schedules the next frame
    /// only if an avatar will move, and returns the layer to paint while the
    /// spaces show avatars.
    pub(super) fn render_avatar_layer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let now = Instant::now();
        self.observe_spaces(now, cx);
        if self.space_sort != crate::preferences::SpaceSort::Attention || !self.sidebar_visible {
            self.space_avatars.timer = None;
            return None;
        }
        self.space_avatars.signal.read(cx);
        self.schedule_avatar_wake(now, window, cx);
        let layer = &self.space_avatars;
        let slots = layer.slots.clone();
        let clock = layer.clock;
        let motions = layer.motions.clone();
        let images = layer.images.clone();
        Some(
            canvas(
                |_, _, _| (),
                move |_, _, window, _| {
                    let motions = motions.borrow();
                    let mut images = images.borrow_mut();
                    for slot in slots.borrow().iter() {
                        let Some(motion) = motions.get(&slot.key) else {
                            continue;
                        };
                        let pose = motion.pose(now, clock);
                        window.with_content_mask(
                            Some(ContentMask { bounds: slot.clip }),
                            |window| {
                                pixel_avatar::paint(
                                    window,
                                    &mut images,
                                    slot.bounds,
                                    slot.style,
                                    &pose,
                                )
                            },
                        );
                    }
                },
            )
            .absolute()
            .size_full()
            .into_any_element(),
        )
    }

    fn schedule_avatar_wake(&mut self, now: Instant, window: &mut Window, cx: &mut Context<Self>) {
        let layer = &mut self.space_avatars;
        match layer.wake(now) {
            Wake::Never => layer.timer = None,
            Wake::Frame => {
                let signal = layer.signal.clone();
                window.on_next_frame(move |_, cx| signal.update(cx, |_, cx| cx.notify()));
            }
            Wake::After(delay) => {
                let deadline = now + delay;
                // Keep a pending timer that fires no later than needed; a
                // render in between would otherwise restart it every frame.
                if layer
                    .timer
                    .as_ref()
                    .is_some_and(|(pending, _)| *pending > now && *pending <= deadline)
                {
                    return;
                }
                let signal = layer.signal.clone();
                let task = cx.spawn(async move |_, cx| {
                    cx.background_executor().timer(delay).await;
                    signal.update(cx, |_, cx| cx.notify());
                });
                layer.timer = Some((deadline, task));
            }
        }
    }

    /// A slot for `key`'s avatar, filling its parent, which records where it
    /// lays out for the window's avatar layer.
    pub(crate) fn avatar_slot(&self, key: SpaceKey, style: AvatarStyle) -> Canvas<()> {
        let slots = self.space_avatars.slots();
        canvas(
            move |bounds, window, _| {
                slots.borrow_mut().push(AvatarSlot {
                    key,
                    style,
                    bounds,
                    clip: window.content_mask().bounds,
                });
            },
            |_, _, _, _| (),
        )
        .size_full()
    }

    /// Forgets the slots of the last sidebar layout as a new one begins.
    pub(crate) fn clear_avatar_slots(&self) {
        self.space_avatars.slots.borrow_mut().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use std::time::Duration;

    fn key(endpoint: &str, id: &str) -> SpaceKey {
        SpaceKey::new(endpoint, id)
    }

    fn slot(key: SpaceKey) -> AvatarSlot {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(32.), px(32.)));
        AvatarSlot {
            style: AvatarStyle::default_for(&key.id),
            key,
            bounds,
            clip: bounds,
        }
    }

    #[gpui::test]
    fn wakes_only_for_visible_avatars_that_move(cx: &mut TestAppContext) {
        let mut layer = cx.update(AvatarLayer::new);
        let now = layer.clock;
        layer.observe(
            "local",
            [("idle", AgentStatus::Idle), ("gone", AgentStatus::Working)],
            now,
        );
        assert_eq!(layer.wake(now), Wake::Never, "no slots, nothing to draw");
        layer.slots.borrow_mut().push(slot(key("local", "idle")));
        assert_eq!(
            layer.wake(now),
            Wake::Never,
            "a resting crescent holds still"
        );
        layer.slots.borrow_mut().push(slot(key("local", "gone")));
        assert!(
            matches!(layer.wake(now), Wake::After(delay) if delay <= Duration::from_millis(240))
        );
        // A status change plays a moment, which draws every frame.
        let later = now + Duration::from_secs(2);
        layer.observe(
            "local",
            [("idle", AgentStatus::Done), ("gone", AgentStatus::Working)],
            later,
        );
        assert_eq!(layer.wake(later), Wake::Frame);
    }

    #[gpui::test]
    fn only_a_change_of_focus_counts_as_a_touch(cx: &mut TestAppContext) {
        let mut layer = cx.update(AvatarLayer::new);
        assert!(layer.focus_moved("local", "w1"));
        assert!(!layer.focus_moved("local", "w1"));
        assert!(layer.focus_moved("ssh:devbox", "w1"));
        assert!(layer.focus_moved("local", "w2"));
        assert!(layer.focus_moved("local", "w1"));
    }

    #[gpui::test]
    fn only_a_new_projection_is_observed(cx: &mut TestAppContext) {
        let mut layer = cx.update(AvatarLayer::new);
        assert!(layer.changed("local", "boot", 1));
        assert!(!layer.changed("local", "boot", 1));
        assert!(layer.changed("local", "boot", 2));
        assert!(layer.changed("local", "reboot", 2));
        assert!(layer.changed("ssh:devbox", "reboot", 2));
        assert!(!layer.changed("local", "reboot", 2));
    }

    #[gpui::test]
    fn observing_one_endpoint_forgets_only_its_departed_spaces(cx: &mut TestAppContext) {
        let mut layer = cx.update(AvatarLayer::new);
        let now = layer.clock;
        layer.observe(
            "local",
            [("a", AgentStatus::Idle), ("b", AgentStatus::Idle)],
            now,
        );
        layer.observe("ssh:devbox", [("a", AgentStatus::Working)], now);
        layer.observe("local", [("b", AgentStatus::Idle)], now);
        let motions = layer.motions.borrow();
        assert!(!motions.contains_key(&key("local", "a")));
        assert!(motions.contains_key(&key("local", "b")));
        assert!(motions.contains_key(&key("ssh:devbox", "a")));
    }
}
