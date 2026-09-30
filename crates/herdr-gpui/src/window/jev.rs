//! When the window asks Jev to judge its spaces, and what it does with the
//! answer. Only in attention order, only with an AI Gateway key, only when a
//! space's status or task changed, at most once per interval and one request
//! at a time. The request runs on the background executor; a failure keeps
//! the last judgment and is logged, never shown as a terminal error.

use super::HerdrWindow;
use crate::spaces::{Candidate, Recency, SpaceKey, now_millis};
use gpui::{Context, Task};
use herdr_client::protocol::AgentStatus;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    time::{Duration, Instant},
};

/// The shortest wait between two requests, however often spaces change.
const MIN_INTERVAL: Duration = Duration::from_secs(30);

pub(crate) struct Jev {
    api_key: String,
    /// What the last request described, so an unchanged list is not re-sent.
    signature: Option<u64>,
    last: Option<Instant>,
    pending: Option<Task<()>>,
}

impl Jev {
    /// Jev is on only when `AI_GATEWAY_API_KEY` holds a key.
    pub(crate) fn from_environment() -> Option<Self> {
        let api_key = std::env::var("AI_GATEWAY_API_KEY").ok()?;
        let api_key = api_key.trim();
        (!api_key.is_empty()).then(|| Self {
            api_key: api_key.to_owned(),
            signature: None,
            last: None,
            pending: None,
        })
    }

    /// Whether a request for `signature` may go out at `now`.
    fn due(&self, signature: u64, now: Instant) -> bool {
        self.pending.is_none()
            && self.signature != Some(signature)
            && self
                .last
                .is_none_or(|last| now.saturating_duration_since(last) >= MIN_INTERVAL)
    }
}

fn status_word(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Working => "working",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Done => "done",
        AgentStatus::Idle => "idle",
        AgentStatus::Unknown => "unknown",
    }
}

/// Identifies what a list of candidates says that could change a judgment;
/// minute counts are left out, or every minute would look like news.
fn signature(candidates: &[Candidate]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for candidate in candidates {
        candidate.key.hash(&mut hasher);
        candidate.status.hash(&mut hasher);
        candidate.focused_now.hash(&mut hasher);
        candidate.tasks.hash(&mut hasher);
    }
    hasher.finish()
}

impl HerdrWindow {
    /// Every visible space on every machine, as Jev is told about it.
    fn jev_candidates(&self, now: Instant, cx: &gpui::App) -> Vec<Candidate> {
        let millis = now_millis();
        let mut candidates = Vec::new();
        for (index, endpoint) in self.endpoints.iter().enumerate() {
            if !self.device_visible(&endpoint.id) {
                continue;
            }
            let selected = index == self.selected_endpoint;
            let live = if selected { &self.live } else { &endpoint.live };
            let Some(snapshot) = &live.snapshot else {
                continue;
            };
            for space in &snapshot.workspaces {
                let key = SpaceKey::new(&endpoint.id, &space.workspace_id);
                let tasks = snapshot
                    .agents
                    .iter()
                    .filter(|agent| agent.workspace_id == space.workspace_id)
                    .filter_map(|agent| agent.terminal_title_stripped.as_deref())
                    .map(str::trim)
                    .filter(|title| !title.is_empty())
                    .map(str::to_owned)
                    .collect();
                candidates.push(Candidate {
                    minutes_in_status: self
                        .space_avatars
                        .since_change(&key, now)
                        .map(|since| since.as_secs() / 60),
                    minutes_since_focused: Recency::touched(cx, &key)
                        .map(|at| millis.saturating_sub(at) / 60_000),
                    focused_now: selected && space.focused,
                    name: space.label.clone(),
                    machine: endpoint.label.clone(),
                    branch: space.branch.clone(),
                    status: status_word(space.agent_status),
                    tasks,
                    key,
                });
            }
        }
        candidates.truncate(crate::spaces::JEV_MAX_SPACES);
        candidates
    }

    /// Asks Jev to judge the spaces if their story changed since it last did.
    pub(super) fn consider_jev(&mut self, now: Instant, cx: &mut Context<Self>) {
        if self.jev.is_none() {
            return;
        }
        let candidates = self.jev_candidates(now, cx);
        let signature = signature(&candidates);
        let Some(jev) = &mut self.jev else {
            return;
        };
        if !jev.due(signature, now) {
            return;
        }
        jev.signature = Some(signature);
        jev.last = Some(now);
        let api_key = jev.api_key.clone();
        let executor = cx.background_executor().clone();
        jev.pending = Some(cx.spawn(async move |this, cx| {
            let result = executor
                .spawn(async move { crate::spaces::judge(&api_key, &candidates) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(jev) = &mut this.jev {
                    jev.pending = None;
                }
                match result {
                    Ok(scores) => {
                        this.jev_scores = scores.into_iter().collect();
                        cx.notify();
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Jev could not judge the spaces");
                    }
                }
            });
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(status: &'static str, minutes: u64) -> Candidate {
        Candidate {
            key: SpaceKey::new("local", "w1"),
            name: "herdr".into(),
            machine: "Local".into(),
            branch: None,
            status,
            minutes_in_status: Some(minutes),
            minutes_since_focused: Some(minutes),
            focused_now: false,
            tasks: vec!["Fix the drop preview".into()],
        }
    }

    #[test]
    fn only_a_changed_story_is_news_and_requests_are_spaced() {
        assert_eq!(
            signature(&[candidate("working", 1)]),
            signature(&[candidate("working", 9)]),
            "time passing alone does not ask again"
        );
        assert_ne!(
            signature(&[candidate("working", 1)]),
            signature(&[candidate("done", 1)])
        );
        let start = Instant::now();
        let mut jev = Jev {
            api_key: "key".into(),
            signature: None,
            last: None,
            pending: None,
        };
        assert!(jev.due(1, start));
        jev.signature = Some(1);
        jev.last = Some(start);
        assert!(!jev.due(1, start + MIN_INTERVAL * 2), "nothing changed");
        assert!(!jev.due(2, start + MIN_INTERVAL / 2), "too soon");
        assert!(jev.due(2, start + MIN_INTERVAL));
    }
}
