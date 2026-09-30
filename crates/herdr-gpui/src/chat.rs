//! A native chat view of an agent pane. The agent keeps running in its
//! terminal; this reads the session log the agent writes (Claude Code or
//! Codex), found through the session id Herdr reports for the pane, and draws
//! it as a conversation: messages, Markdown, code, diffs, diagrams, and math.
//! What the user types is sent to the pane as if typed there.
//!
//! Reading and parsing happen on worker threads (`follow`); the view keeps a
//! bounded copy of the conversation and renders from it.

mod diagram;
mod follow;
mod highlight;
mod markdown;
mod math;
mod mermaid;
mod render;
mod tern;
mod transcript;
mod view;

pub(crate) use tern::{install_fonts, native_tag};
pub(crate) use transcript::Dialect;
pub(crate) use view::{ChatEvent, ChatPanel, PaneContext};

use std::collections::VecDeque;
use transcript::{Item, Update};

/// Conversation items the view keeps, matching what a transcript keeps.
const MAX_ITEMS: usize = 600;

/// The view's copy of a conversation, kept current from transcript updates.
#[derive(Debug, Default)]
struct Conversation {
    items: VecDeque<Item>,
    title: Option<String>,
    model: Option<String>,
    turn_started: Option<u64>,
}

/// Where an update's item landed.
#[derive(Debug, PartialEq, Eq)]
enum Placed {
    /// Replaced the item at this index.
    Changed(usize),
    Appended,
    /// Older than anything kept.
    Dropped,
}

impl Conversation {
    fn clear(&mut self) {
        *self = Self::default();
    }

    /// Applies `update`, reporting where each item landed and how many items
    /// scrolled off the front.
    fn apply(&mut self, update: Update) -> (Vec<Placed>, usize) {
        self.title = update.title;
        self.model = update.model;
        self.turn_started = update.turn_started;
        let mut placed = Vec::with_capacity(update.items.len());
        for item in update.items {
            let first = self.items.front().map(|item| item.id);
            let last = self.items.back().map(|item| item.id);
            match (first, last) {
                (Some(first), Some(last)) if item.id >= first && item.id <= last => {
                    // Ids in the copy are ascending, though not contiguous
                    // once the reader restarted mid-session.
                    match self.items.binary_search_by_key(&item.id, |kept| kept.id) {
                        Ok(index) => {
                            self.items[index] = item;
                            placed.push(Placed::Changed(index));
                        }
                        Err(_) => placed.push(Placed::Dropped),
                    }
                }
                (Some(first), _) if item.id < first => placed.push(Placed::Dropped),
                _ => {
                    self.items.push_back(item);
                    placed.push(Placed::Appended);
                }
            }
        }
        let mut removed = 0;
        while self.items.len() > MAX_ITEMS {
            self.items.pop_front();
            removed += 1;
        }
        (placed, removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use transcript::Entry;

    fn item(id: u64, version: u64) -> Item {
        Item {
            id,
            version,
            entry: Entry::User(format!("{id}.{version}")),
        }
    }

    #[test]
    fn updates_append_replace_and_stay_bounded() {
        let mut conversation = Conversation::default();
        let (placed, removed) = conversation.apply(Update {
            items: vec![item(3, 0), item(4, 0)],
            title: Some("t".into()),
            ..Update::default()
        });
        assert_eq!(placed, [Placed::Appended, Placed::Appended]);
        assert_eq!(removed, 0);
        assert_eq!(conversation.title.as_deref(), Some("t"));
        let (placed, _) = conversation.apply(Update {
            items: vec![item(2, 0), item(3, 1), item(5, 0)],
            ..Update::default()
        });
        assert_eq!(
            placed,
            [Placed::Dropped, Placed::Changed(0), Placed::Appended]
        );
        assert_eq!(conversation.items[0].version, 1);
        assert_eq!(
            conversation.title, None,
            "the update carries the whole state"
        );
        let (_, removed) = conversation.apply(Update {
            items: (6..6 + MAX_ITEMS as u64).map(|id| item(id, 0)).collect(),
            ..Update::default()
        });
        assert_eq!(removed, 3);
        assert_eq!(conversation.items.len(), MAX_ITEMS);
        assert_eq!(conversation.items[0].id, 6);
    }
}
