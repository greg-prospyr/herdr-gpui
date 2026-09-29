//! The glide that carries a reordered row from where it was drawn to its new
//! place, as a function of time like the rest of the window's motion.

use crate::motion;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

/// How long a row takes to glide to its new place.
const SLIDE: Duration = Duration::from_millis(360);

/// Rows gliding to new places when the list reorders: each remembers its
/// last resting top, and a row whose top changes starts from where it was
/// drawn and eases into its new place.
#[derive(Default)]
pub(crate) struct Slides {
    tops: HashMap<String, f32>,
    moving: HashMap<String, (f32, Instant)>,
}

impl Slides {
    /// Takes every card's new resting top. A row seen for the first time
    /// simply appears there.
    pub(crate) fn arrange(&mut self, tops: impl IntoIterator<Item = (String, f32)>, now: Instant) {
        let mut next = HashMap::new();
        for (key, top) in tops {
            if let Some(&old) = self.tops.get(&key)
                && (old - top).abs() > 0.5
            {
                let (drawn, _) = self.offset(&key, now);
                self.moving.insert(key.clone(), (old + drawn - top, now));
            }
            next.insert(key, top);
        }
        self.moving.retain(|key, (_, since)| {
            next.contains_key(key) && motion::progress(*since, now, SLIDE).is_some()
        });
        self.tops = next;
    }

    /// How far `key` is drawn from its resting top, and whether it is still
    /// moving.
    pub(crate) fn offset(&self, key: &str, now: Instant) -> (f32, bool) {
        self.moving
            .get(key)
            .and_then(|(start, since)| {
                motion::progress(*since, now, SLIDE).map(|k| (start * (1. - k), true))
            })
            .unwrap_or((0., false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reordered_row_glides_from_where_it_was_drawn() {
        let start = Instant::now();
        let mut slides = Slides::default();
        slides.arrange([("a".into(), 0.), ("b".into(), 40.)], start);
        assert_eq!(slides.offset("b", start), (0., false));
        // "b" needs you and moves to the top.
        slides.arrange([("b".into(), 0.), ("a".into(), 40.)], start);
        assert_eq!(slides.offset("b", start), (40., true));
        assert_eq!(slides.offset("a", start), (-40., true));
        let midway = slides.offset("b", start + SLIDE / 2).0;
        assert!(midway > 0. && midway < 20., "{midway}");
        // Reversed mid-glide, it starts from where it is drawn, not a jump.
        let back = start + SLIDE / 2;
        slides.arrange([("a".into(), 0.), ("b".into(), 40.)], back);
        assert!((slides.offset("b", back).0 - (midway - 40.)).abs() < 0.01);
        let settled = back + SLIDE;
        slides.arrange([("a".into(), 0.), ("b".into(), 40.)], settled);
        assert_eq!(slides.offset("b", settled), (0., false));
        assert!(slides.moving.is_empty());
        // A new row appears in place.
        slides.arrange([("c".into(), 80.)], settled);
        assert_eq!(slides.offset("c", settled), (0., false));
        assert!(!slides.tops.contains_key("a"));
    }
}
