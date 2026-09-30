//! Ranking spaces across every machine. A repository's parent row and its
//! indented worktrees move as one unit, so a group is never split: the unit
//! ranks by its most urgent member and its most recently touched one.

use herdr_client::protocol::AgentStatus;
use std::ops::Range;

/// A visible row as the sidebar lists it: workspace index, whether it is an
/// indented worktree, and the group key a parent heads.
pub(crate) type Entry = (usize, bool, Option<String>);

/// One unit to rank, from any machine.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Unit {
    /// Unique across machines; a hold names units by it.
    pub head: String,
    /// Lower needs you sooner; see [`urgency`].
    pub urgency: u8,
    /// When it was last touched, in milliseconds since the Unix epoch; zero
    /// if never, and `u64::MAX` for a space focused now.
    pub recency: u64,
    /// Jev's judgment of how much it needs the user, 0 to 1, when there is one.
    pub attention: Option<f32>,
}

/// Recency fades by half every hour, so a judgment can outweigh a touch
/// that is getting old.
const HALF_LIFE_MS: f64 = 60. * 60. * 1000.;
/// A space Jev has not judged sits in the middle, neither pushed nor held back.
const UNJUDGED: f64 = 0.5;

/// How a unit places within its tier: Jev's judgment weighted over recency.
/// Without any judgment this orders exactly as recency does.
fn standing(unit: &Unit, now: u64) -> f64 {
    let recency = match unit.recency {
        u64::MAX => 1.,
        0 => 0.,
        at => 0.5_f64.powf(now.saturating_sub(at) as f64 / HALF_LIFE_MS),
    };
    0.7 * unit.attention.map_or(UNJUDGED, f64::from) + 0.3 * recency
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

/// Ranks `units`, returning their indices: those needing you first, then by
/// standing (Jev's judgment blended with recency at `now`), ties in the order
/// given. With `held`, the units keep that order of heads, and units it does
/// not name follow in rank order, so the list stays still under the pointer.
pub(crate) fn rank(units: &[Unit], held: Option<&[String]>, now: u64) -> Vec<usize> {
    let mut order: Vec<usize> = (0..units.len()).collect();
    order.sort_by(|&a, &b| {
        let (ua, ub) = (&units[a], &units[b]);
        ua.urgency
            .cmp(&ub.urgency)
            .then_with(|| standing(ub, now).total_cmp(&standing(ua, now)))
            // Touches that have faded to nothing still order by which is newer.
            .then_with(|| ub.recency.cmp(&ua.recency))
            .then(a.cmp(&b))
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

    const NOW: u64 = 10 * 60 * 60 * 1000;

    fn unit(head: &str, status: AgentStatus, recency: u64) -> Unit {
        Unit {
            head: head.into(),
            urgency: urgency(status),
            recency,
            attention: None,
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
            heads(&units, &rank(&units, None, NOW)),
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
        assert_eq!(
            heads(&units, &rank(&units, Some(&held), NOW)),
            ["a", "b", "new"]
        );
        assert_eq!(heads(&units, &rank(&units, None, NOW)), ["b", "new", "a"]);
    }

    #[test]
    fn ties_keep_the_order_given() {
        let units = [
            unit("x", AgentStatus::Idle, 0),
            unit("y", AgentStatus::Idle, 0),
            unit("z", AgentStatus::Unknown, 0),
        ];
        assert_eq!(rank(&units, None, NOW), [0, 1, 2]);
    }

    #[test]
    fn jev_reorders_within_a_tier_but_never_across_tiers() {
        use AgentStatus::*;
        let hour = 60 * 60 * 1000;
        let mut units = [
            unit("recent-trivial", Idle, NOW - 5 * 60 * 1000),
            unit("stale-important", Idle, NOW - 5 * hour),
            unit("blocked", Blocked, NOW - 9 * hour),
        ];
        assert_eq!(
            heads(&units, &rank(&units, None, NOW)),
            ["blocked", "recent-trivial", "stale-important"]
        );
        units[0].attention = Some(0.05);
        units[1].attention = Some(0.95);
        units[2].attention = Some(0.0);
        assert_eq!(
            heads(&units, &rank(&units, None, NOW)),
            ["blocked", "stale-important", "recent-trivial"],
            "a judgment outweighs a touch, not the daemon's blocked tier"
        );
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
