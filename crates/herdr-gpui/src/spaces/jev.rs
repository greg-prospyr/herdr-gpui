//! Asking Jev, TypeSafe's System One model, through Vercel AI Gateway, how
//! much each space needs the user right now, as a score that attention order
//! blends with recency inside each status tier.
//!
//! What leaves this machine is deliberate and small: each space's name,
//! machine, branch, daemon status, minutes in that status and since it was
//! last focused, and its agents' task titles. No terminal contents or prompts.
//! The key comes from `AI_GATEWAY_API_KEY`; without it nothing is sent.

use super::SpaceKey;
use serde_json::{Value, json};
use std::time::Duration;

const ENDPOINT: &str = "https://ai-gateway.vercel.sh/typesafe/v1/systemone";
const MODEL: &str = "typesafe-ai/jev";
/// Spaces judged per request; beyond this the rest keep plain recency.
pub(crate) const MAX_SPACES: usize = 60;
const MAX_TASKS: usize = 3;
const MAX_TEXT_CHARS: usize = 120;
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);
/// The Score levels, lowest first; a score is normalized by the top level.
const LEVELS: [&str; 4] = [
    "Nothing to do: the space is idle with no open task, or its work was finished and seen long ago.",
    "Can wait: work is in progress and does not need the user yet.",
    "Soon: work finished or paused recently, and the user should look at it soon.",
    "Now: the space is blocked waiting on the user, or finished work is waiting on a decision.",
];

/// What Jev is told about one space.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Candidate {
    pub key: SpaceKey,
    pub name: String,
    pub machine: String,
    pub branch: Option<String>,
    /// The daemon's status word, such as `working`.
    pub status: &'static str,
    pub minutes_in_status: Option<u64>,
    pub minutes_since_focused: Option<u64>,
    pub focused_now: bool,
    /// What its agents report they are doing, from their terminal titles.
    pub tasks: Vec<String>,
}

fn clip(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_TEXT_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// The request for `candidates`: one Score question per space over a shared
/// state, so a single call judges them all in parallel.
pub(crate) fn request_body(candidates: &[Candidate]) -> Value {
    let candidates = &candidates[..candidates.len().min(MAX_SPACES)];
    let spaces: Vec<Value> = candidates
        .iter()
        .map(|space| {
            json!({
                "name": clip(&space.name),
                "machine": clip(&space.machine),
                "branch": space.branch.as_deref().map(clip),
                "status": space.status,
                "minutes_in_status": space.minutes_in_status,
                "minutes_since_user_last_focused": space.minutes_since_focused,
                "focused_by_user_now": space.focused_now,
                "agent_tasks": space.tasks.iter().take(MAX_TASKS).map(|task| clip(task)).collect::<Vec<_>>(),
            })
        })
        .collect();
    let questions: serde_json::Map<String, Value> = (0..candidates.len())
        .map(|index| {
            (
                format!("s{index}"),
                json!({
                    "type": "score",
                    "instructions": format!(
                        "How much does the coding workspace at `spaces[{index}]` need the user's attention right now? Blocked means an agent is waiting for the user's answer; done means an agent finished and its work awaits review."
                    ),
                    "criteria": LEVELS,
                }),
            )
        })
        .collect();
    json!({
        "model": MODEL,
        "state": { "spaces": spaces },
        "questions": questions,
    })
}

/// Each judged space's need for attention, from 0 to 1, in request order.
pub(crate) fn parse_scores(
    body: &Value,
    candidates: &[Candidate],
) -> crate::Result<Vec<(SpaceKey, f32)>> {
    let answers = body
        .get("answers")
        .and_then(Value::as_object)
        .ok_or(crate::Error::JevAnswer)?;
    let top = (LEVELS.len() - 1) as f64;
    candidates
        .iter()
        .take(MAX_SPACES)
        .enumerate()
        .map(|(index, space)| {
            let score = answers
                .get(&format!("s{index}"))
                .filter(|answer| answer.get("type").and_then(Value::as_str) == Some("score"))
                .and_then(|answer| answer.get("score"))
                .and_then(Value::as_f64)
                .filter(|score| score.is_finite())
                .ok_or(crate::Error::JevAnswer)?;
            Ok((space.key.clone(), (score / top).clamp(0., 1.) as f32))
        })
        .collect()
}

/// Blocking: call only on a background executor. One bounded request.
pub(crate) fn judge(
    api_key: &str,
    candidates: &[Candidate],
) -> crate::Result<Vec<(SpaceKey, f32)>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into();
    let body = serde_json::to_vec(&request_body(candidates))?;
    let mut response = agent
        .post(ENDPOINT)
        .header("Authorization", &format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .send(&body[..])
        .map_err(crate::Error::JevNetwork)?;
    let status = response.status().as_u16();
    if !response.status().is_success() {
        return Err(crate::Error::JevStatus(status));
    }
    let bytes = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_vec()
        .map_err(crate::Error::JevNetwork)?;
    parse_scores(&serde_json::from_slice(&bytes)?, candidates)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn candidate(id: &str, status: &'static str) -> Candidate {
        Candidate {
            key: SpaceKey::new("local", id),
            name: format!("space {id}"),
            machine: "Local".into(),
            branch: Some("main".into()),
            status,
            minutes_in_status: Some(12),
            minutes_since_focused: None,
            focused_now: false,
            tasks: vec!["Investigate PRO-6233 SMS loss".into()],
        }
    }

    #[test]
    fn one_request_asks_a_score_per_space_over_shared_state() {
        let mut long = candidate("b", "done");
        long.tasks = vec![
            "x".repeat(500),
            "two\u{1b}[31m".into(),
            "3".into(),
            "4".into(),
        ];
        let body = request_body(&[candidate("a", "blocked"), long]);
        assert_eq!(body["model"], MODEL);
        let spaces = body["state"]["spaces"].as_array().unwrap();
        assert_eq!(spaces.len(), 2);
        assert_eq!(spaces[0]["status"], "blocked");
        assert_eq!(spaces[0]["minutes_in_status"], 12);
        assert!(spaces[0]["minutes_since_user_last_focused"].is_null());
        let tasks = spaces[1]["agent_tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), MAX_TASKS, "tasks are capped");
        assert_eq!(tasks[0].as_str().unwrap().chars().count(), MAX_TEXT_CHARS);
        assert!(
            !tasks[1].as_str().unwrap().contains('\u{1b}'),
            "controls are dropped"
        );
        let question = &body["questions"]["s1"];
        assert_eq!(question["type"], "score");
        assert!(
            question["instructions"]
                .as_str()
                .unwrap()
                .contains("`spaces[1]`")
        );
        assert_eq!(question["criteria"].as_array().unwrap().len(), LEVELS.len());
        let many: Vec<_> = (0..MAX_SPACES + 5)
            .map(|i| candidate(&i.to_string(), "idle"))
            .collect();
        assert_eq!(
            request_body(&many)["questions"].as_object().unwrap().len(),
            MAX_SPACES
        );
    }

    /// Opt-in and live: sends two made-up spaces, never real ones, to check
    /// the gateway, the key, and the answer's shape against the real service.
    #[test]
    #[ignore = "requires AI_GATEWAY_API_KEY and network access to AI Gateway"]
    fn jev_live_judges_a_blocked_space_above_an_idle_one() {
        let key = std::env::var("AI_GATEWAY_API_KEY").unwrap();
        let mut idle = candidate("idle", "idle");
        idle.tasks = vec![];
        idle.minutes_in_status = Some(600);
        let blocked = candidate("blocked", "blocked");
        let scores = judge(&key, &[idle, blocked]).unwrap();
        assert!(scores[1].1 > scores[0].1, "{scores:?}");
    }

    #[test]
    fn scores_are_normalized_and_malformed_answers_rejected() {
        let candidates = [candidate("a", "done"), candidate("b", "idle")];
        let body = json!({
            "model": "jev-1.13.0",
            "answers": {
                "s0": { "type": "score", "score": 2.4, "confidence": 0.8 },
                "s1": { "type": "score", "score": 0.3, "confidence": 0.9 },
            },
        });
        let scores = parse_scores(&body, &candidates).unwrap();
        assert_eq!(scores[0].0, SpaceKey::new("local", "a"));
        assert!((scores[0].1 - 0.8).abs() < 1e-6);
        assert!((scores[1].1 - 0.1).abs() < 1e-6);
        for bad in [
            json!({}),
            json!({ "answers": { "s0": { "type": "score", "score": 1.0 } } }),
            json!({ "answers": { "s0": { "type": "noul", "noul": 0.5 }, "s1": { "type": "score", "score": 1 } } }),
        ] {
            assert!(matches!(
                parse_scores(&bad, &candidates),
                Err(crate::Error::JevAnswer)
            ));
        }
        assert!(
            judge("unused", &[]).unwrap().is_empty(),
            "nothing to judge, nothing sent"
        );
    }
}
