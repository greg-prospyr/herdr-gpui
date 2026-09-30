//! Which workspaces the tree shows and how each one is named. A repository and
//! its linked worktrees form one group, folded or unfolded locally: collapsing
//! changes this client's view only, never the daemon's state.

use super::{
    RowBadge,
    row::{PrBadge, first_text},
};
use crate::{HerdrWindow, config::Theme};
use gpui::{prelude::*, *};
use herdr_client::protocol::ClientShellWorkspace;
use std::collections::{HashMap, HashSet};

pub(super) fn workspace_entries(workspaces: &[ClientShellWorkspace]) -> Vec<(usize, bool)> {
    let mut groups = HashMap::<&str, (Option<usize>, Vec<usize>)>::new();
    for (index, workspace) in workspaces.iter().enumerate() {
        if let Some(worktree) = &workspace.worktree {
            let (parent, members) = groups.entry(&worktree.key).or_default();
            if !worktree.is_linked_worktree && parent.is_none() {
                *parent = Some(index);
            }
            members.push(index);
        }
    }
    let mut emitted = HashSet::new();
    let mut entries = Vec::with_capacity(workspaces.len());
    for (index, workspace) in workspaces.iter().enumerate() {
        let group = workspace.worktree.as_ref().and_then(|tree| {
            let (parent, members) = groups.get(tree.key.as_str())?;
            Some((tree.key.as_str(), (*parent)?, members))
        });
        if let Some((key, parent, members)) = group {
            if emitted.insert(key) {
                entries.push((parent, false));
                entries.extend(members.iter().filter(|&&i| i != parent).map(|&i| (i, true)));
            }
        } else {
            entries.push((index, false));
        }
    }
    entries
}

pub(super) fn visible_workspace_entries(
    workspaces: &[ClientShellWorkspace],
    collapsed: &HashSet<String>,
) -> Vec<(usize, bool, Option<String>)> {
    let entries = workspace_entries(workspaces);
    entries
        .iter()
        .enumerate()
        .filter_map(|(position, &(index, child))| {
            let key = workspaces[index].worktree.as_ref().map(|tree| &tree.key);
            if child && key.is_some_and(|key| collapsed.contains(key)) {
                return None;
            }
            let group = (!child && entries.get(position + 1).is_some_and(|entry| entry.1))
                .then(|| key.cloned())
                .flatten();
            Some((index, child, group))
        })
        .collect()
}

/// Cached pull request, dirty and teleported marks for a worktree row, if the prefetch and
/// the Git probe already have them. Rendering only reads: a missing entry
/// simply shows nothing, never a stale or guessed state.
/// `cache` is `None` for a row on a device other than the selected one: the
/// cache holds only that device's lookups, and the same path and branch can
/// exist on another host.
pub(super) fn workspace_badge(
    workspace: &ClientShellWorkspace,
    cache: Option<&crate::pull_request::Cache>,
    git: &crate::git::Git,
    marks: (&crate::teleport::Marks, &str),
    theme: &Theme,
) -> Option<RowBadge> {
    let key = workspace.worktree.as_ref()?.key.as_str();
    let branch = workspace.branch.as_deref()?;
    let (marks, endpoint) = marks;
    RowBadge::new(
        cache
            .and_then(|cache| cache.peek(key, branch))
            .map(|pr| PrBadge::new(pr, theme)),
        git.dirty(key, branch).unwrap_or(false),
        marks.find(endpoint, key, branch).is_some(),
    )
}

pub(crate) fn workspace_label(workspace: &ClientShellWorkspace, indented: bool) -> &str {
    let branch = (indented && !workspace.custom_label)
        .then_some(workspace.branch.as_deref())
        .flatten()
        .map(|branch| branch.strip_prefix("worktree/").unwrap_or(branch));
    first_text([branch, Some(&workspace.label)], "workspace")
}

/// The Spaces header's order, `manual` or `attention`, which a click flips.
/// Client-local and saved beside the sidebar width, like the agents sort.
pub(super) fn spaces_sort(window: &HerdrWindow, cx: &mut Context<HerdrWindow>) -> Stateful<Div> {
    let theme = &window.theme;
    div()
        .id("spaces-sort")
        .debug_selector(|| "spaces-sort".into())
        .flex_none()
        .min_w_0()
        .truncate()
        .text_color(rgb(theme.muted))
        .cursor_pointer()
        .hover(|sort| sort.text_color(rgb(theme.foreground)))
        .on_click(cx.listener(|this, _, _, cx| {
            cx.stop_propagation();
            this.space_sort = this.space_sort.toggled();
            this.space_sort_modified = true;
            this.save_chrome();
            cx.notify();
        }))
        .child(super::label_text(&window.space_sort.to_string()))
}

impl HerdrWindow {
    /// Every visible machine's space rows as one list in attention order, each
    /// with the index of the endpoint it is on. A space focused on the machine
    /// the window shows counts as touched now; another machine's focus counts
    /// from when this client saw it move there.
    pub(super) fn ranked_spaces(&self, cx: &App) -> RankedSpaces {
        use crate::spaces::{Pins, Recency, SpaceKey, Unit, rank, row_key, units, urgency};
        let mut ranked = Vec::new();
        let mut rows = Vec::new();
        // Each unit's pin, if any member is pinned: its place among the pins
        // and the key that holds it.
        let mut pins = Vec::new();
        for (endpoint_index, endpoint) in self.endpoints.iter().enumerate() {
            if !self.device_visible(&endpoint.id) {
                continue;
            }
            let selected = endpoint_index == self.selected_endpoint;
            let live = if selected { &self.live } else { &endpoint.live };
            let Some(snapshot) = &live.snapshot else {
                continue;
            };
            let collapsed = if endpoint_index == 0 {
                &self.collapsed_repos
            } else {
                &endpoint.collapsed_repos
            };
            let entries = visible_workspace_entries(&snapshot.workspaces, collapsed);
            for unit in units(&entries) {
                let members = &entries[unit];
                let spaces = || members.iter().map(|entry| &snapshot.workspaces[entry.0]);
                let head = &snapshot.workspaces[members[0].0].workspace_id;
                ranked.push(Unit {
                    head: row_key(&endpoint.id, head),
                    urgency: spaces()
                        .map(|space| urgency(space.agent_status))
                        .min()
                        .unwrap_or(u8::MAX),
                    recency: spaces()
                        .map(|space| {
                            if selected && space.focused {
                                u64::MAX
                            } else {
                                Recency::touched(
                                    cx,
                                    &SpaceKey::new(&endpoint.id, &space.workspace_id),
                                )
                                .unwrap_or(0)
                            }
                        })
                        .max()
                        .unwrap_or(0),
                });
                rows.push(
                    members
                        .iter()
                        .map(|entry| (endpoint_index, entry.clone()))
                        .collect::<Vec<_>>(),
                );
                pins.push(
                    spaces()
                        .filter_map(|space| {
                            let key = SpaceKey::new(&endpoint.id, &space.workspace_id);
                            Some((Pins::position(cx, &key)?, key))
                        })
                        .min_by_key(|(position, _)| *position),
                );
            }
        }
        // Pins come first, in the order the user arranged; they are not ranked
        // and the hold does not touch them.
        let mut pinned: Vec<usize> = (0..ranked.len()).filter(|&i| pins[i].is_some()).collect();
        pinned.sort_by_key(|&i| pins[i].as_ref().map(|(position, _)| *position));
        let loose: Vec<usize> = (0..ranked.len()).filter(|&i| pins[i].is_none()).collect();
        let candidates: Vec<Unit> = loose.iter().map(|&i| ranked[i].clone()).collect();
        // A pointer over the list holds its order; leaving lets it settle.
        let held = self
            .spaces_hovered
            .then(|| self.space_heads.borrow().clone());
        let order: Vec<usize> = rank(&candidates, held.as_deref())
            .into_iter()
            .map(|index| loose[index])
            .collect();
        *self.space_heads.borrow_mut() = order
            .iter()
            .map(|&index| ranked[index].head.clone())
            .collect();
        let mut result = RankedSpaces::default();
        for index in pinned {
            let key = pins[index].take().map(|(_, key)| key);
            for row in std::mem::take(&mut rows[index]) {
                result.rows.push((row, key.clone()));
            }
        }
        result.pinned = result.rows.len();
        for index in order {
            result.rows.extend(
                std::mem::take(&mut rows[index])
                    .into_iter()
                    .map(|row| (row, None)),
            );
        }
        result
    }
}

/// Every visible machine's space rows in attention order: the pinned rows
/// first, each with the pin that holds its group, then the ranked rest.
#[derive(Default)]
pub(super) struct RankedSpaces {
    pub rows: Vec<(
        (usize, crate::spaces::Entry),
        Option<crate::spaces::SpaceKey>,
    )>,
    pub pinned: usize,
}
