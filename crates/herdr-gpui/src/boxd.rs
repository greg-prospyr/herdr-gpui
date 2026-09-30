//! Saved devices that are boxd.sh machines. boxd hibernates a machine after
//! hours without network traffic and wakes it on the next inbound connection,
//! so a client that keeps every device connected keeps every box awake, and
//! one that retries a hibernated box wakes it again.
//!
//! This asks the local `boxd` CLI, never the boxes, which machines are
//! running, on a worker and only while some enabled device is a box. Without
//! the CLI, boxes are simply treated as ordinary SSH devices.

use herdr_client::{ScriptHost, ScriptLimits, run_script};
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{
        atomic::AtomicBool,
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

/// How often the CLI is asked; a box's state changes over minutes.
const POLL: Duration = Duration::from_secs(60);
/// A listing larger than this is not one.
const MAX_OUTPUT: u64 = 1024 * 1024;
const IDLE: Duration = Duration::from_secs(20);

/// The boxd machine an SSH target names: `[user@]name.boxd` or
/// `[user@]name.boxd.sh`, the forms the boxd SSH config and its CLI accept.
pub(crate) fn box_name(target: &str) -> Option<&str> {
    let host = target.rsplit_once('@').map_or(target, |(_, host)| host);
    let name = host
        .strip_suffix(".boxd.sh")
        .or_else(|| host.strip_suffix(".boxd"))?;
    (!name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then_some(name)
}

/// What boxd says a machine is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BoxState {
    Running,
    /// Suspended, hibernated, or stopped: connecting would wake it.
    Asleep,
    /// Starting, forking, or anything newer than this client knows.
    Other,
}

impl BoxState {
    fn parse(status: &str) -> Self {
        match status {
            "running" => Self::Running,
            "suspended" | "hibernated" | "stopped" | "paused" => Self::Asleep,
            _ => Self::Other,
        }
    }
}

#[derive(Deserialize)]
struct Listed {
    name: String,
    status: String,
}

/// `boxd ls --json`: machine names and their states.
pub(crate) fn parse_list(json: &[u8]) -> crate::Result<HashMap<String, BoxState>> {
    let listed: Vec<Listed> = serde_json::from_slice(json).map_err(crate::Error::BoxdDecode)?;
    Ok(listed
        .into_iter()
        .map(|machine| (machine.name, BoxState::parse(&machine.status)))
        .collect())
}

/// Blocking: runs the CLI with a bounded output and idle time.
fn list() -> crate::Result<HashMap<String, BoxState>> {
    // A GUI app's PATH rarely includes the user's own bin directory, where
    // the boxd installer puts the CLI.
    let script = r#"PATH="$HOME/.local/bin:$HOME/.boxd/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
export PATH
command -v boxd >/dev/null 2>&1 || exit 127
exec boxd ls --json
"#;
    let mut output = Vec::new();
    run_script(
        ScriptHost::Local,
        script,
        std::io::empty(),
        &mut output,
        ScriptLimits {
            output: MAX_OUTPUT,
            idle: IDLE,
        },
        &AtomicBool::new(false),
    )
    .map_err(crate::Error::BoxdList)?;
    parse_list(&output)
}

/// Machine states by name.
type States = HashMap<String, BoxState>;
/// A running listing worker: where to ask, and where its answers arrive.
type Worker = (SyncSender<()>, Receiver<crate::Result<States>>);

/// Box states, refreshed in the background while boxes are saved devices.
#[derive(Default)]
pub(crate) struct Watcher {
    states: States,
    worker: Option<Worker>,
    busy: bool,
    next: Option<Instant>,
    /// The CLI is missing or failing; logged once until it answers again.
    failing: bool,
}

impl Watcher {
    /// What boxd last said about `name`, if it said anything.
    pub(crate) fn state(&self, name: &str) -> Option<BoxState> {
        self.states.get(name).copied()
    }

    #[cfg(test)]
    pub(crate) fn set_state(&mut self, name: &str, state: BoxState) {
        self.states.insert(name.to_owned(), state);
    }

    /// Takes a finished listing and starts the next when one is due. `wanted`
    /// is whether any enabled device is a box; without one nothing runs.
    /// Returns whether the states changed.
    pub(crate) fn poll(&mut self, wanted: bool, now: Instant) -> bool {
        let mut changed = false;
        if let Some((_, results)) = &self.worker {
            match results.try_recv() {
                Ok(result) => {
                    self.busy = false;
                    match result {
                        Ok(states) => {
                            changed = states != self.states;
                            self.states = states;
                            self.failing = false;
                        }
                        Err(error) => {
                            if !self.failing {
                                tracing::info!(%error, "boxd machine states are unavailable");
                            }
                            self.failing = true;
                            changed = !self.states.is_empty();
                            self.states.clear();
                        }
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.worker = None;
                    self.busy = false;
                }
            }
        }
        if !wanted || self.busy || self.next.is_some_and(|next| next > now) {
            return changed;
        }
        if self.worker.is_none() {
            self.worker = spawn();
        }
        if let Some((requests, _)) = &self.worker
            && requests.try_send(()).is_ok()
        {
            self.busy = true;
            self.next = Some(now + POLL);
        }
        changed
    }
}

fn spawn() -> Option<Worker> {
    let (requests, incoming) = mpsc::sync_channel::<()>(1);
    let (outgoing, results) = mpsc::sync_channel(1);
    let spawned = thread::Builder::new()
        .name("herdr-boxd".into())
        .spawn(move || {
            for () in incoming {
                if outgoing.send(list()).is_err() {
                    break;
                }
            }
        });
    match spawned {
        Ok(_) => Some((requests, results)),
        Err(error) => {
            tracing::warn!(%error, "could not start the boxd worker");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn box_targets_are_recognised_in_every_ssh_form() {
        assert_eq!(box_name("prospyr-pr-9271.boxd"), Some("prospyr-pr-9271"));
        assert_eq!(
            box_name("boxd@prospyr-pr-9271.boxd.sh"),
            Some("prospyr-pr-9271")
        );
        for other in [
            "greg@omarchy.tail862fd0.ts.net",
            ".boxd",
            "a b.boxd",
            "boxd.sh",
            "x.boxd.sh.evil",
        ] {
            assert_eq!(box_name(other), None, "{other}");
        }
    }

    #[test]
    fn listings_map_names_to_states() {
        let json = br#"[{"name":"a","status":"running","url":"a.boxd.sh"},{"name":"b","status":"hibernated"},{"name":"c","status":"suspended"},{"name":"d","status":"forking"}]"#;
        let states = parse_list(json).unwrap();
        assert_eq!(states["a"], BoxState::Running);
        assert_eq!(states["b"], BoxState::Asleep);
        assert_eq!(states["c"], BoxState::Asleep);
        assert_eq!(states["d"], BoxState::Other);
        assert!(matches!(
            parse_list(b"{}"),
            Err(crate::Error::BoxdDecode(_))
        ));
    }

    #[test]
    fn the_watcher_stays_idle_without_boxes() {
        let mut watcher = Watcher::default();
        assert!(!watcher.poll(false, Instant::now()));
        assert!(watcher.worker.is_none(), "no box, no worker, no CLI");
        assert_eq!(watcher.state("a"), None);
    }
}
