//! The avatars people chose for spaces, remembered by this client alone: the
//! daemon has no field for them, so another client or machine shows each
//! space's default. Loaded before GPUI starts and saved on a worker, like the browser
//! tabs, and bounded so a long-lived client never grows the file forever.

use super::style::AvatarStyle;
use crate::{spaces::SpaceKey, state_file};
use gpui::{App, Global};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf};

const MAX_ENTRIES: usize = 512;
const MAX_FILE_BYTES: u64 = 256 * 1024;
#[derive(Serialize, Deserialize)]
struct Entry {
    #[serde(flatten)]
    key: SpaceKey,
    style: AvatarStyle,
}

/// Oldest choice first, so a trimmed file keeps the most recent.
#[derive(Serialize, Deserialize)]
struct Saved {
    avatars: Vec<Entry>,
}

#[derive(Default)]
pub(crate) struct AvatarStore {
    /// Each choice with the order it was made in, for trimming the oldest.
    chosen: HashMap<SpaceKey, (AvatarStyle, u64)>,
    next: u64,
    writer: Option<state_file::Writer<Saved>>,
    quitting: bool,
}

impl Global for AvatarStore {}

fn parse(bytes: &[u8]) -> crate::Result<Vec<Entry>> {
    let saved: Saved = serde_json::from_slice(bytes)?;
    if saved.avatars.len() > MAX_ENTRIES || !saved.avatars.iter().all(|e| e.key.valid()) {
        return Err(crate::Error::InvalidSpaceAvatars);
    }
    Ok(saved.avatars)
}

impl AvatarStore {
    fn path() -> Option<PathBuf> {
        crate::preferences::state_dir().map(|dir| dir.join("space-avatars.json"))
    }

    /// Called before starting GPUI. A missing or damaged file starts with
    /// every space on its default.
    pub(crate) fn load() -> Self {
        let path = Self::path();
        let entries = path
            .as_deref()
            .map(|path| {
                state_file::read(path, MAX_FILE_BYTES)
                    .and_then(|bytes| bytes.as_deref().map_or(Ok(Vec::new()), parse))
            })
            .transpose()
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "Cannot restore space avatars");
                None
            })
            .unwrap_or_default();
        let writer = path.and_then(
            |path| match state_file::Writer::start("space-avatars", path) {
                Ok(writer) => Some(writer),
                Err(error) => {
                    tracing::warn!(%error, "Cannot start space-avatar worker");
                    None
                }
            },
        );
        Self::with_entries(entries, writer)
    }

    fn with_entries(entries: Vec<Entry>, writer: Option<state_file::Writer<Saved>>) -> Self {
        let mut store = Self {
            writer,
            ..Self::default()
        };
        for entry in entries {
            store.remember(entry.key, entry.style);
        }
        store
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

    /// The avatar `key` shows: the one chosen for it, or its default.
    pub(crate) fn style(cx: &App, key: &SpaceKey) -> AvatarStyle {
        cx.try_global::<Self>()
            .and_then(|store| store.chosen(key))
            .unwrap_or_else(|| AvatarStyle::default_for(&key.id))
    }

    pub(crate) fn chosen(&self, key: &SpaceKey) -> Option<AvatarStyle> {
        self.chosen.get(key).map(|(style, _)| *style)
    }

    /// Chooses `style` for `key`, or returns it to its default with `None`,
    /// and saves. Fixtures that never installed a store get an unsaved one.
    pub(crate) fn choose(cx: &mut App, key: SpaceKey, style: Option<AvatarStyle>) {
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
        }
        let store = cx.global_mut::<Self>();
        match style {
            Some(style) if key.valid() => store.remember(key, style),
            Some(_) => return,
            None => {
                store.chosen.remove(&key);
            }
        }
        store.save();
    }

    fn remember(&mut self, key: SpaceKey, style: AvatarStyle) {
        self.chosen.insert(key, (style, self.next));
        self.next += 1;
        if self.chosen.len() > MAX_ENTRIES
            && let Some(oldest) = self
                .chosen
                .iter()
                .min_by_key(|(_, (_, order))| *order)
                .map(|(key, _)| key.clone())
        {
            self.chosen.remove(&oldest);
        }
    }

    fn saved(&self) -> Saved {
        let mut entries: Vec<_> = self.chosen.iter().collect();
        entries.sort_by_key(|(_, (_, order))| *order);
        Saved {
            avatars: entries
                .into_iter()
                .map(|(key, (style, _))| Entry {
                    key: key.clone(),
                    style: *style,
                })
                .collect(),
        }
    }

    fn save(&self) {
        if self.quitting {
            return;
        }
        if let Some(writer) = &self.writer {
            writer.save(self.saved());
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::pixel_avatar::{Hue, Shape};

    fn key(id: &str) -> SpaceKey {
        SpaceKey::new("local", id)
    }

    const CLOUD: AvatarStyle = AvatarStyle {
        shape: Shape::Cloud,
        hue: Hue::Lagoon,
    };

    #[gpui::test]
    fn choices_override_defaults_and_reset_back(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let space = key("w1");
            assert_eq!(
                AvatarStore::style(cx, &space),
                AvatarStyle::default_for("w1")
            );
            AvatarStore::choose(cx, space.clone(), Some(CLOUD));
            assert_eq!(AvatarStore::style(cx, &space), CLOUD);
            let elsewhere = SpaceKey::new("ssh:devbox", "w1");
            assert_ne!(AvatarStore::style(cx, &elsewhere), CLOUD);
            AvatarStore::choose(cx, space.clone(), None);
            assert_eq!(
                AvatarStore::style(cx, &space),
                AvatarStyle::default_for("w1")
            );
            AvatarStore::choose(cx, key(""), Some(CLOUD));
            assert!(cx.global::<AvatarStore>().chosen.is_empty());
        });
    }

    #[test]
    fn keeps_the_most_recent_choices_within_its_bound() {
        let mut store = AvatarStore::default();
        for index in 0..MAX_ENTRIES + 3 {
            store.remember(key(&format!("p{index}")), CLOUD);
        }
        store.remember(key("p5"), CLOUD);
        assert_eq!(store.chosen.len(), MAX_ENTRIES);
        for gone in ["p0", "p1", "p2"] {
            assert!(store.chosen(&key(gone)).is_none(), "{gone}");
        }
        assert!(store.chosen(&key("p3")).is_some());
        let saved = store.saved();
        assert_eq!(saved.avatars.last().unwrap().key, key("p5"));
    }

    #[test]
    fn round_trips_and_rejects_damaged_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("space-avatars.json");
        let mut store = AvatarStore::default();
        store.remember(key("a"), CLOUD);
        store.remember(
            key("b"),
            AvatarStyle {
                shape: Shape::Peak,
                hue: Hue::Berry,
            },
        );
        state_file::write(&path, &store.saved()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(
                r#"{"endpoint":"local","id":"a","style":{"shape":"cloud","hue":"lagoon"}}"#
            )
        );
        let restored = AvatarStore::with_entries(parse(text.as_bytes()).unwrap(), None);
        assert_eq!(restored.chosen(&key("a")), Some(CLOUD));
        assert_eq!(restored.chosen(&key("b")).unwrap().shape, Shape::Peak);
        for damaged in [
            r#"{"avatars":[{"endpoint":"","id":"a","style":{"shape":"cloud","hue":"lagoon"}}]}"#,
            r#"{"avatars":[{"endpoint":"local","id":"a","style":{"shape":"star","hue":"lagoon"}}]}"#,
            r#"[]"#,
        ] {
            assert!(parse(damaged.as_bytes()).is_err(), "{damaged}");
        }
        let too_many = Saved {
            avatars: (0..=MAX_ENTRIES)
                .map(|index| Entry {
                    key: key(&format!("p{index}")),
                    style: CLOUD,
                })
                .collect(),
        };
        assert!(matches!(
            parse(&serde_json::to_vec(&too_many).unwrap()),
            Err(crate::Error::InvalidSpaceAvatars)
        ));
    }
}
