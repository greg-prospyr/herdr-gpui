//! Spaces pinned above the attention list, in the order the user arranged
//! them. Remembered by this client only, like the rest of attention order;
//! loaded before GPUI starts, saved on a worker, and bounded.

use super::SpaceKey;
use crate::state_file;
use gpui::{App, Global};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const MAX_PINS: usize = 64;
const MAX_FILE_BYTES: u64 = 64 * 1024;

#[derive(Serialize, Deserialize)]
struct Saved {
    pins: Vec<SpaceKey>,
}

#[derive(Default)]
pub(crate) struct Pins {
    /// Top of the list first.
    pins: Vec<SpaceKey>,
    writer: Option<state_file::Writer<Saved>>,
    quitting: bool,
}

impl Global for Pins {}

fn parse(bytes: &[u8]) -> crate::Result<Vec<SpaceKey>> {
    let saved: Saved = serde_json::from_slice(bytes)?;
    if saved.pins.len() > MAX_PINS || !saved.pins.iter().all(SpaceKey::valid) {
        return Err(crate::Error::InvalidSpacePins);
    }
    Ok(saved.pins)
}

impl Pins {
    fn path() -> Option<PathBuf> {
        crate::preferences::state_dir().map(|dir| dir.join("space-pins.json"))
    }

    /// Called before starting GPUI. A missing or damaged file pins nothing.
    pub(crate) fn load() -> Self {
        let path = Self::path();
        let pins = path
            .as_deref()
            .map(|path| {
                state_file::read(path, MAX_FILE_BYTES)
                    .and_then(|bytes| bytes.as_deref().map_or(Ok(Vec::new()), parse))
            })
            .transpose()
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "Cannot restore pinned spaces");
                None
            })
            .unwrap_or_default();
        let writer = path.and_then(|path| match state_file::Writer::start("space-pins", path) {
            Ok(writer) => Some(writer),
            Err(error) => {
                tracing::warn!(%error, "Cannot start space-pin worker");
                None
            }
        });
        Self {
            pins,
            writer,
            quitting: false,
        }
    }

    pub(crate) fn install(self, cx: &mut App) {
        cx.set_global(self);
        cx.on_app_quit(|cx| {
            let store = cx.global_mut::<Self>();
            store.quitting = true;
            let writer = store.writer.take();
            cx.background_executor().spawn(async move {
                if let Some(writer) = writer {
                    writer.finish();
                }
            })
        })
        .detach();
    }

    /// Where `key` sits among the pins, top first, if it is pinned.
    pub(crate) fn position(cx: &App, key: &SpaceKey) -> Option<usize> {
        cx.try_global::<Self>()?
            .pins
            .iter()
            .position(|pin| pin == key)
    }

    /// Every pin, top first.
    pub(crate) fn all(cx: &App) -> Vec<SpaceKey> {
        cx.try_global::<Self>()
            .map(|store| store.pins.clone())
            .unwrap_or_default()
    }

    /// Pins `key` at the bottom of the pins, or unpins it, and saves.
    pub(crate) fn toggle(cx: &mut App, key: SpaceKey) {
        Self::update(cx, |pins| {
            if let Some(index) = pins.iter().position(|pin| *pin == key) {
                pins.remove(index);
            } else if key.valid() && pins.len() < MAX_PINS {
                pins.push(key);
            }
        });
    }

    /// Moves `dragged` to `target`'s place: after it when dragged down, before
    /// it when dragged up, which is where the pointer let go.
    pub(crate) fn move_to(cx: &mut App, dragged: &SpaceKey, target: &SpaceKey) {
        Self::update(cx, |pins| {
            let (Some(from), Some(to)) = (
                pins.iter().position(|pin| pin == dragged),
                pins.iter().position(|pin| pin == target),
            ) else {
                return;
            };
            let pin = pins.remove(from);
            pins.insert(to, pin);
        });
    }

    fn update(cx: &mut App, change: impl FnOnce(&mut Vec<SpaceKey>)) {
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
        }
        let store = cx.global_mut::<Self>();
        change(&mut store.pins);
        if store.quitting {
            return;
        }
        if let Some(writer) = &store.writer {
            writer.save(Saved {
                pins: store.pins.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use core::prelude::v1::test;

    fn key(id: &str) -> SpaceKey {
        SpaceKey::new("local", id)
    }

    fn order(cx: &App) -> Vec<String> {
        cx.global::<Pins>()
            .pins
            .iter()
            .map(|pin| pin.id.clone())
            .collect()
    }

    #[gpui::test]
    fn pins_toggle_and_move_where_the_pointer_let_go(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            for id in ["a", "b", "c", "d"] {
                Pins::toggle(cx, key(id));
            }
            assert_eq!(order(cx), ["a", "b", "c", "d"]);
            assert_eq!(Pins::position(cx, &key("c")), Some(2));
            // Dragged down onto "c", "a" lands after it.
            Pins::move_to(cx, &key("a"), &key("c"));
            assert_eq!(order(cx), ["b", "c", "a", "d"]);
            // Dragged up onto "b", "d" lands before it.
            Pins::move_to(cx, &key("d"), &key("b"));
            assert_eq!(order(cx), ["d", "b", "c", "a"]);
            Pins::move_to(cx, &key("d"), &key("missing"));
            assert_eq!(order(cx), ["d", "b", "c", "a"]);
            Pins::toggle(cx, key("b"));
            assert_eq!(order(cx), ["d", "c", "a"]);
            assert_eq!(Pins::position(cx, &key("b")), None);
            Pins::toggle(cx, key(""));
            assert_eq!(order(cx).len(), 3);
        });
    }

    #[test]
    fn saved_pins_round_trip_and_damaged_files_are_rejected() {
        let saved = Saved {
            pins: vec![key("a"), SpaceKey::new("ssh:devbox", "w1")],
        };
        let bytes = serde_json::to_vec(&saved).unwrap();
        assert_eq!(parse(&bytes).unwrap(), saved.pins);
        for damaged in [
            r#"{"pins":[{"endpoint":"","id":"a"}]}"#,
            r#"{"pins":3}"#,
            "[]",
        ] {
            assert!(parse(damaged.as_bytes()).is_err(), "{damaged}");
        }
        let too_many = Saved {
            pins: (0..=MAX_PINS).map(|i| key(&format!("w{i}"))).collect(),
        };
        assert!(matches!(
            parse(&serde_json::to_vec(&too_many).unwrap()),
            Err(crate::Error::InvalidSpacePins)
        ));
    }
}
