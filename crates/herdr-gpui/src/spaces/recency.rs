//! When each space was last focused, remembered by this client so spaces left
//! alone stay low after a relaunch. It orders spaces only: status and the
//! activity dot still come from the daemon, which has no such field.
//! Loaded before GPUI starts and saved on a worker, and bounded.

use super::SpaceKey;
use crate::state_file;
use gpui::{App, Global};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_ENTRIES: usize = 512;
const MAX_FILE_BYTES: u64 = 256 * 1024;

#[derive(Serialize, Deserialize)]
struct Entry {
    #[serde(flatten)]
    key: SpaceKey,
    /// Milliseconds since the Unix epoch.
    touched: u64,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    spaces: Vec<Entry>,
}

#[derive(Default)]
pub(crate) struct Recency {
    touched: HashMap<SpaceKey, u64>,
    writer: Option<state_file::Writer<Saved>>,
    quitting: bool,
}

impl Global for Recency {}

fn parse(bytes: &[u8]) -> crate::Result<Vec<Entry>> {
    let saved: Saved = serde_json::from_slice(bytes)?;
    if saved.spaces.len() > MAX_ENTRIES || !saved.spaces.iter().all(|e| e.key.valid()) {
        return Err(crate::Error::InvalidSpaceRecency);
    }
    Ok(saved.spaces)
}

/// Now, as the store records it.
pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

impl Recency {
    fn path() -> Option<PathBuf> {
        crate::preferences::state_dir().map(|dir| dir.join("space-recency.json"))
    }

    /// Called before starting GPUI. A missing or damaged file starts every
    /// space untouched, in the daemon's order.
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
                tracing::warn!(%error, "Cannot restore space recency");
                None
            })
            .unwrap_or_default();
        let writer = path.and_then(
            |path| match state_file::Writer::start("space-recency", path) {
                Ok(writer) => Some(writer),
                Err(error) => {
                    tracing::warn!(%error, "Cannot start space-recency worker");
                    None
                }
            },
        );
        let mut store = Self {
            writer,
            ..Self::default()
        };
        for entry in entries {
            store.remember(entry.key, entry.touched);
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

    pub(crate) fn touched(cx: &App, key: &SpaceKey) -> Option<u64> {
        cx.try_global::<Self>()?.touched.get(key).copied()
    }

    /// Records that `key` was focused at `at` and saves. Fixtures that never
    /// installed a store get an unsaved one.
    pub(crate) fn touch(cx: &mut App, key: SpaceKey, at: u64) {
        if !key.valid() {
            return;
        }
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
        }
        let store = cx.global_mut::<Self>();
        store.remember(key, at);
        store.save();
    }

    fn remember(&mut self, key: SpaceKey, at: u64) {
        self.touched.insert(key, at);
        if self.touched.len() > MAX_ENTRIES
            && let Some(oldest) = self
                .touched
                .iter()
                .min_by_key(|(_, touched)| **touched)
                .map(|(key, _)| key.clone())
        {
            self.touched.remove(&oldest);
        }
    }

    fn saved(&self) -> Saved {
        let mut spaces: Vec<_> = self
            .touched
            .iter()
            .map(|(key, touched)| Entry {
                key: key.clone(),
                touched: *touched,
            })
            .collect();
        spaces.sort_by_key(|entry| entry.touched);
        Saved { spaces }
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
    use core::prelude::v1::test;

    fn key(id: &str) -> SpaceKey {
        SpaceKey::new("local", id)
    }

    #[gpui::test]
    fn touches_are_per_endpoint_and_ignore_invalid_keys(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            assert_eq!(Recency::touched(cx, &key("w1")), None);
            Recency::touch(cx, key("w1"), 7);
            assert_eq!(Recency::touched(cx, &key("w1")), Some(7));
            assert_eq!(
                Recency::touched(cx, &SpaceKey::new("ssh:devbox", "w1")),
                None
            );
            Recency::touch(cx, key(""), 9);
            assert_eq!(cx.global::<Recency>().touched.len(), 1);
        });
    }

    #[test]
    fn forgets_the_least_recent_beyond_its_bound() {
        let mut store = Recency::default();
        for index in 0..MAX_ENTRIES as u64 + 2 {
            store.remember(key(&format!("w{index}")), 100 + index);
        }
        assert_eq!(store.touched.len(), MAX_ENTRIES);
        assert!(!store.touched.contains_key(&key("w0")));
        assert!(!store.touched.contains_key(&key("w1")));
        assert!(store.touched.contains_key(&key("w2")));
    }

    #[test]
    fn round_trips_and_rejects_damaged_files() {
        let mut store = Recency::default();
        store.remember(key("a"), 5);
        store.remember(key("b"), 3);
        let bytes = serde_json::to_vec(&store.saved()).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.starts_with(r#"{"spaces":[{"endpoint":"local","id":"b","touched":3}"#));
        let entries = parse(&bytes).unwrap();
        assert_eq!(entries.len(), 2);
        for damaged in [
            r#"{"spaces":[{"endpoint":"","id":"a","touched":1}]}"#,
            r#"{"spaces":[{"endpoint":"local","id":"a","touched":-1}]}"#,
            "[]",
        ] {
            assert!(parse(damaged.as_bytes()).is_err(), "{damaged}");
        }
    }
}
