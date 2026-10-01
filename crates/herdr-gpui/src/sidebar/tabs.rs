//! What each space has cooking. Its tabs show as pips beside its name, one
//! per tab, taller the more a tab needs you; the busiest tab's task stands in
//! for the branch while an agent works there; and the pips open a line per
//! tab under the row. Everything comes from the daemon's snapshot: tab
//! statuses are Herdr's own, never guessed from terminal output.

use super::{agents::agent_name, cell::ClickHandler, cell::RowContext, label_text, line_height};
use gpui::{prelude::*, *};
use herdr_client::protocol::{AgentStatus, ClientShellAgent, ClientShellSnapshot};

/// Pips drawn before the rest fold into the last one's count.
const MAX_PIPS: usize = 8;
const PIP_WIDTH: f32 = 4.;
const PIP_GAP: f32 = 3.;
/// Room the disclosure chevron after the pips takes.
const CHEVRON: f32 = 12.;

/// One tab of a space, as the row and its opened list show it.
#[derive(Debug, PartialEq)]
pub(super) struct TabActivity<'a> {
    pub(super) tab_id: &'a str,
    pub(super) label: &'a str,
    pub(super) status: AgentStatus,
    /// The agent running in it, by the name the sidebar gives agents.
    pub(super) agent: Option<&'a str>,
    /// What that agent says it is doing.
    pub(super) topic: Option<&'a str>,
}

/// How much a status asks of the user, loudest highest.
fn loudness(status: AgentStatus) -> u8 {
    match status {
        AgentStatus::Blocked => 3,
        AgentStatus::Working => 2,
        AgentStatus::Done => 1,
        AgentStatus::Idle | AgentStatus::Unknown => 0,
    }
}

/// An agent's current task: the topic Herdr's usage reporter extracts, else
/// its terminal title when that says more than its own name.
fn topic(agent: &ClientShellAgent) -> Option<&str> {
    let reported = agent
        .tokens
        .iter()
        .find(|(name, _)| name == "quota_topic")
        .map(|(_, value)| value.as_str());
    reported
        .or(agent.terminal_title_stripped.as_deref())
        .map(str::trim)
        .filter(|topic| !topic.is_empty() && *topic != agent_name(agent))
}

/// The space's tabs in their order, each with its loudest agent.
pub(super) fn tabs_of<'a>(
    snapshot: &'a ClientShellSnapshot,
    workspace_id: &str,
) -> Vec<TabActivity<'a>> {
    let mut tabs: Vec<_> = snapshot
        .tabs
        .iter()
        .filter(|tab| tab.workspace_id == workspace_id)
        .collect();
    tabs.sort_by_key(|tab| tab.number);
    tabs.into_iter()
        .map(|tab| {
            let agent = snapshot
                .agents
                .iter()
                .filter(|agent| agent.tab_id == tab.tab_id)
                .max_by_key(|agent| loudness(agent.agent_status));
            TabActivity {
                tab_id: &tab.tab_id,
                label: &tab.label,
                status: tab.agent_status,
                agent: agent.map(agent_name),
                topic: agent.and_then(topic),
            }
        })
        .collect()
}

/// The busiest tab's task while an agent is at work or waiting on the user,
/// as `agent · task`; otherwise nothing, and the row keeps its branch.
pub(super) fn lead(tabs: &[TabActivity<'_>]) -> Option<String> {
    let tab = tabs
        .iter()
        .filter(|tab| matches!(tab.status, AgentStatus::Working | AgentStatus::Blocked))
        .filter(|tab| tab.topic.is_some())
        .max_by_key(|tab| loudness(tab.status))?;
    let who = tab.agent.unwrap_or(tab.label);
    Some(format!("{who} · {}", tab.topic?))
}

/// Whether a space has anything worth pips: more than one tab, or an agent.
pub(super) fn worth_showing(tabs: &[TabActivity<'_>]) -> bool {
    tabs.len() > 1 || tabs.iter().any(|tab| tab.agent.is_some())
}

/// A space's pips and the toggle that opens its tab list.
pub(super) struct TabPips {
    pub(super) id: ElementId,
    pub(super) statuses: Vec<AgentStatus>,
    pub(super) open: bool,
    pub(super) toggle: Box<ClickHandler>,
}

impl TabPips {
    pub(super) fn width(&self) -> f32 {
        let pips = self.statuses.len().min(MAX_PIPS) as f32;
        pips * PIP_WIDTH + (pips - 1.).max(0.) * PIP_GAP + CHEVRON
    }

    /// The pips, bottom-aligned on the name line, then a chevron. Clicking
    /// them opens or closes the tab list and never selects the row.
    pub(super) fn element(self, cx: &RowContext<'_>) -> Stateful<Div> {
        let Self {
            id,
            statuses,
            open,
            toggle,
        } = self;
        let (theme, font) = (cx.theme, cx.font);
        let line = line_height(font);
        let tallest = (line * 0.62).round().max(8.);
        let idle = theme.active;
        let (muted, foreground) = (theme.muted, theme.foreground);
        div()
            .id(id)
            .flex_none()
            .h(px(line))
            .flex()
            .items_center()
            .gap(px(PIP_GAP))
            .cursor_pointer()
            .text_color(rgb(muted))
            .hover(move |style| style.text_color(rgb(foreground)))
            .child(
                div()
                    .h(px(tallest))
                    .flex()
                    .items_end()
                    .gap(px(PIP_GAP))
                    .children(statuses.iter().take(MAX_PIPS).map(|status| {
                        let (_, _, color) = super::agents::status_style(*status);
                        let (height, color) = match status {
                            AgentStatus::Blocked => (1., color),
                            AgentStatus::Working => (0.72, color),
                            AgentStatus::Done => (0.45, color),
                            AgentStatus::Idle | AgentStatus::Unknown => (0.3, idle),
                        };
                        div()
                            .w(px(PIP_WIDTH))
                            .h(px((tallest * height).round().max(2.)))
                            .rounded(px(1.5))
                            .bg(rgb(color))
                    })),
            )
            .child(
                div()
                    .w(px(CHEVRON - PIP_GAP))
                    .text_size(px((font.size - 3.).max(8.)))
                    .child(label_text(if open { "\u{25be}" } else { "\u{25b8}" })),
            )
            .on_click(move |event, window, cx| {
                cx.stop_propagation();
                toggle(event, window, cx);
            })
    }
}

/// An opened space's tabs, a line each under its row: status, tab, agent,
/// task. `open` is what clicking a line does, given the tab's id.
pub(super) fn tab_list(
    key: &str,
    tabs: &[TabActivity<'_>],
    indent: f32,
    cx: &RowContext<'_>,
    open: impl Fn(&str) -> Box<ClickHandler>,
) -> Div {
    let (theme, font) = (cx.theme, cx.font);
    let line = line_height(font);
    div()
        .debug_selector(|| format!("space-tab-list-{key}"))
        .flex()
        .flex_col()
        .pl(px(indent))
        .pr(px(cx.look.content_x()))
        .pb(px(4.))
        .children(tabs.iter().map(|tab| {
            let (_, filled, color) = super::agents::status_style(tab.status);
            let dot = 7.;
            let handler = open(tab.tab_id);
            div()
                .id(SharedString::from(format!(
                    "space-tab-{key}-{}",
                    tab.tab_id
                )))
                .debug_selector(|| format!("space-tab-{}", tab.tab_id))
                .h(px(line + 4.))
                .flex()
                .items_center()
                .gap(px(8.))
                .pl(px(10.))
                .border_l_1()
                .border_color(rgb(theme.active))
                .cursor_pointer()
                .rounded_r(px(4.))
                .hover(|row| row.bg(rgb(theme.active)))
                .child(
                    div()
                        .flex_none()
                        .size(px(dot))
                        .rounded_full()
                        .border_1()
                        .border_color(rgb(color))
                        .when(filled, |dot| dot.bg(rgb(color))),
                )
                .child(
                    div()
                        .flex_none()
                        .max_w(relative(0.45))
                        .truncate()
                        .text_color(rgb(theme.foreground))
                        .child(label_text(tab.agent.unwrap_or(tab.label))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(theme.muted))
                        .child(label_text(tab.topic.unwrap_or(if tab.agent.is_some() {
                            tab.label
                        } else {
                            ""
                        }))),
                )
                .on_click(move |event, window, cx| {
                    cx.stop_propagation();
                    handler(event, window, cx);
                })
        }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use core::prelude::v1::test;
    use herdr_client::protocol::ClientShellTab;

    fn tab(id: &str, number: usize, status: AgentStatus) -> ClientShellTab {
        ClientShellTab {
            tab_id: id.into(),
            workspace_id: "w".into(),
            number,
            label: format!("tab {number}"),
            custom_label: false,
            zoomed: false,
            focused: false,
            agent_status: status,
        }
    }

    #[test]
    fn tabs_lead_with_the_task_that_needs_you_most() {
        let mut snapshot = crate::sidebar::layout_tests::snapshot(1);
        snapshot.tabs = vec![
            tab("t2", 2, AgentStatus::Working),
            tab("t1", 1, AgentStatus::Idle),
            tab("t3", 3, AgentStatus::Blocked),
            ClientShellTab {
                workspace_id: "other".into(),
                ..tab("t9", 9, AgentStatus::Blocked)
            },
        ];
        let mut agent = snapshot.agents[0].clone();
        agent.tab_id = "t2".into();
        agent.tokens = vec![("quota_topic".into(), "Running tests".into())];
        let mut blocked = agent.clone();
        blocked.tab_id = "t3".into();
        blocked.agent_status = AgentStatus::Blocked;
        blocked.tokens = vec![];
        blocked.terminal_title_stripped = Some("Approve the migration?".into());
        snapshot.agents = vec![agent, blocked];
        let tabs = tabs_of(&snapshot, "w");
        assert_eq!(
            tabs.iter().map(|tab| tab.tab_id).collect::<Vec<_>>(),
            ["t1", "t2", "t3"],
            "in tab order, this space's only"
        );
        assert_eq!(tabs[1].topic, Some("Running tests"));
        assert_eq!(tabs[0].agent, None);
        let lead = lead(&tabs).unwrap();
        assert!(lead.ends_with(" · Approve the migration?"), "{lead}");
        assert!(worth_showing(&tabs));
        let quiet = [TabActivity {
            tab_id: "t",
            label: "zsh",
            status: AgentStatus::Idle,
            agent: None,
            topic: None,
        }];
        assert_eq!(super::lead(&quiet), None, "an idle space keeps its branch");
        assert!(!worth_showing(&quiet), "a lone shell needs no pips");
    }
}
