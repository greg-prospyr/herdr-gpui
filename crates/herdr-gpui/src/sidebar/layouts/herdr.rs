//! Herdr's own rows, after its terminal client: a status dot, the name over
//! its branch or agent, tree guides for worktrees, and the pull request on
//! the right edge. Density and style decide spacing and which lines show.

use super::super::{
    ARROW_RESERVE,
    agents::agent_labels,
    cell::{AgentRow, RowContext, RowLayout, RowState, WorkspaceRow},
    line_height,
    row::{RowIcon, RowKind, RowTree},
};
use gpui::{prelude::*, *};

pub(in super::super) struct Herdr;

impl RowLayout for Herdr {
    fn workspace(&self, row: WorkspaceRow<'_>, state: RowState, cx: &RowContext<'_>) -> Div {
        let density = cx.look.density;
        let lines = if density.workspace_details() { 2. } else { 1. };
        let (branch, status) = (row.branch().unwrap_or(""), row.status());
        let detail = row.lead.unwrap_or(branch);
        let WorkspaceRow {
            label,
            tree,
            icon,
            fold,
            grouped,
            badge,
            removing,
            host,
            tabs,
            ..
        } = row;
        let pips = tabs.map(|tabs| {
            let width = tabs.width();
            (tabs.element(cx), width)
        });
        let arrow = fold.map(|fold| {
            fold.element(cx.theme)
                .w(px(ARROW_RESERVE - density.gap()))
                .h(px(line_height(cx.font) * lines))
                .text_size(px(16.))
        });
        super::super::row::row(
            label,
            &[(label, true)],
            detail,
            RowKind::Workspace,
            status,
            removing,
            state,
            tree,
            grouped,
            cx.width,
            icon,
            arrow,
            badge,
            None,
            host,
            pips,
            cx.look,
            (cx.font, cx.theme),
        )
    }

    fn agent(&self, agent: AgentRow<'_>, state: RowState, cx: &RowContext<'_>) -> Div {
        let (name, detail) = agent_labels(agent.name, agent.place, cx.host);
        super::super::row::row(
            &agent.key,
            &name,
            detail,
            RowKind::Agent(agent.icon),
            agent.status,
            false,
            state,
            RowTree::None,
            false,
            cx.width,
            RowIcon::None,
            None,
            None,
            agent.status_text,
            None,
            None,
            cx.look,
            (cx.font, cx.theme),
        )
    }
}
