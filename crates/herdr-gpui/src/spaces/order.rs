//! Ranking spaces across every machine. A repository's parent row and its
//! indented worktrees move as one unit, so a group is never split: the unit
//! ranks by its most urgent member and its most recently touched one.

use herdr_client::protocol::AgentStatus;
use std::{cmp::Reverse, ops::Range};

/// A visible row as the sidebar lists it: workspace index, whether it is an
/// indented worktree, and the group key a parent heads.
pub(crate) type Entry = (usize, bool, Option<String>);

/// One unit to rank, from any machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Unit {
    /// Unique across machines; a hold names units by it.
    pub head: String,
    /// Lower needs you sooner; see [`urgency`].
    pub urgency: u8,
    /// When it was last touched, in any consistent unit; higher is newer.
    pub recency: u64,
}

/// Blocked spaces wait on your answer, done ones on your review.
pub(crate) fn urgency(status: AgentStatus) -> u8 {
    match status {
        AgentStatus::Blocked => 0,
        AgentStatus::Done => 1,
        AgentStatus::Working | AgentStatus::Idle | AgentStatus::Unknown => 2,
    }
}

/// Splits one machine's rows into units: each unindented row with the
/// indented rows under it.
pub(crate) fn units(entries: &[Entry]) -> Vec<Range<usize>> {
    let mut units: Vec<Range<usize>> = Vec::new();
    for (position, (_, indented, _)) in entries.iter().enumerate() {
        match units.last_mut() {
            Some(unit) if *indented => unit.end = position + 1,
            _ => units.push(position..position + 1),
        }
    }
    units
}

/// Ranks `units`, returning their indices: those needing you first, then the
/// most recently touched, ties in the order given. With `held`, the units
/// keep that order of heads, and units it does not name follow in rank
/// order, so the list stays still under the pointer.
pub(crate) fn rank(units: &[Unit], held: Option<&[String]>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..units.len()).collect();
    order.sort_by_key(|&index| {
        let unit = &units[index];
        (unit.urgency, Reverse(unit.recency), index)
    });
    if let Some(held) = held {
        // Stable: units the hold does not name keep their rank order.
        order.sort_by_key(|&index| {
            held.iter()
                .position(|head| *head == units[index].head)
                .unwrap_or(usize::MAX)
        });
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(head: &str, status: AgentStatus, recency: u64) -> Unit {
        Unit {
            head: head.into(),
            urgency: urgency(status),
            recency,
        }
    }

    fn heads(units: &[Unit], order: &[usize]) -> Vec<String> {
        order
            .iter()
            .map(|&index| units[index].head.clone())
            .collect()
    }

    #[test]
    fn needs_you_first_then_most_recently_touched_across_machines() {
        use AgentStatus::*;
        let units = [
            unit("local/old", Idle, 10),
            unit("omarchy/done", Done, 5),
            unit("local/recent", Working, 50),
            unit("boxd/blocked", Blocked, 0),
            unit("local/here", Idle, u64::MAX),
            unit("omarchy/never", Idle, 0),
            unit("boxd/done-newer", Done, 9),
        ];
        assert_eq!(
            heads(&units, &rank(&units, None)),
            [
                "boxd/blocked",
                "boxd/done-newer",
                "omarchy/done",
                "local/here",
                "local/recent",
                "local/old",
                "omarchy/never"
            ]
        );
    }

    #[test]
    fn a_hold_keeps_the_order_and_places_new_units_after_it() {
        use AgentStatus::*;
        let units = [
            unit("a", Idle, 0),
            unit("b", Blocked, 0),
            unit("new", Blocked, 0),
        ];
        let held = ["a".to_owned(), "b".to_owned()];
        assert_eq!(heads(&units, &rank(&units, Some(&held))), ["a", "b", "new"]);
        assert_eq!(heads(&units, &rank(&units, None)), ["b", "new", "a"]);
    }

    #[test]
    fn ties_keep_the_order_given() {
        let units = [
            unit("x", AgentStatus::Idle, 0),
            unit("y", AgentStatus::Idle, 0),
            unit("z", AgentStatus::Unknown, 0),
        ];
        assert_eq!(rank(&units, None), [0, 1, 2]);
    }

    #[test]
    fn a_group_is_one_unit() {
        let entries: Vec<Entry> = vec![
            (0, false, None),
            (1, false, Some("repo".into())),
            (2, true, None),
            (3, true, None),
            (4, false, None),
        ];
        assert_eq!(units(&entries), [0..1, 1..4, 4..5]);
    }
}
