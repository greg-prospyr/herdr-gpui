//! Following one pane's agent session from a worker thread: ask Herdr which
//! session the pane's agent reports, then stream that session's log from the
//! host it lives on (locally, or over SSH like Teleport) and parse it there.
//! The UI only receives what changed.
//!
//! Every channel is bounded, so a UI that falls behind slows the reader down
//! instead of queueing without limit. Dropping the [`Follower`] cancels the
//! work; nothing is ever joined on the UI thread.

use super::transcript::{Dialect, Transcript, Update};
use crate::teleport::{AgentSession, Host, SessionKind};
use herdr_client::{ScriptLimits, run_script, shell_quote};
use serde::Deserialize;
use std::{
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender as Sender, TrySendError, sync_channel},
    },
    thread,
    time::Duration,
};

/// How much of an existing log is read when following starts: enough for
/// the recent conversation without replaying a day-long session.
const BACKLOG_BYTES: u64 = 3 * 1024 * 1024;
/// How often the pane is asked which session it runs.
const LOCAL_POLL: Duration = Duration::from_secs(2);
/// A remote ask opens an SSH connection, so it is asked less often.
const REMOTE_POLL: Duration = Duration::from_secs(10);
const SLICE: Duration = Duration::from_millis(100);
/// A query that prints more than this is not a pane description.
const QUERY_OUTPUT: u64 = 1024 * 1024;
const QUERY_IDLE: Duration = Duration::from_secs(30);
/// Exit status the log script uses when the session file is missing.
const MISSING: i32 = 3;

pub(crate) enum Event {
    /// A session starts (or restarts): forget what was shown.
    Session(Dialect),
    Update(Update),
    /// The pane runs no agent with a log this client can read; `agent` names
    /// the one it runs, if any.
    NoSession {
        agent: Option<String>,
    },
    Failed(crate::Error),
}

/// Follows a pane until dropped.
pub(crate) struct Follower {
    events: Receiver<Event>,
    cancelled: Arc<AtomicBool>,
}

impl Follower {
    pub(crate) fn events(&self) -> &Receiver<Event> {
        &self.events
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

pub(crate) fn start(host: Host, pane_id: String) -> crate::Result<Follower> {
    let (sender, events) = sync_channel(64);
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    thread::Builder::new()
        .name("herdr-chat".into())
        .spawn(move || watch(&host, &pane_id, &sender, &flag))
        .map_err(crate::Error::ChatWorker)?;
    Ok(Follower { events, cancelled })
}

#[derive(Debug, Deserialize)]
struct Envelope {
    result: PaneResult,
}

#[derive(Debug, Deserialize)]
struct PaneResult {
    pane: Pane,
}

#[derive(Debug, Deserialize)]
struct Pane {
    agent: Option<String>,
    agent_session: Option<AgentSession>,
}

/// A session whose log this client knows how to read.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Session {
    dialect: Dialect,
    id: String,
}

impl Session {
    fn of(session: &AgentSession) -> Option<Self> {
        let dialect = Dialect::of_agent(&session.agent)?;
        let id = session.value.as_str();
        // The id becomes part of a file-name glob, so only plain id text.
        let plain = session.kind == SessionKind::Id
            && !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        plain.then(|| Self {
            dialect,
            id: id.to_owned(),
        })
    }

    /// Finds the session's log and follows it from `BACKLOG_BYTES` before
    /// its end. Both agents only ever append to the file.
    fn script(&self) -> String {
        let id = &self.id;
        let locate = match self.dialect {
            Dialect::Claude => format!(
                r#"f=
for c in "${{CLAUDE_CONFIG_DIR:-$HOME/.claude}}"/projects/*/{id}.jsonl; do if [ -f "$c" ]; then f=$c; break; fi; done
"#
            ),
            Dialect::Codex => format!(
                r#"f=$(find "${{CODEX_HOME:-$HOME/.codex}}/sessions" -type f -name 'rollout-*-{id}.jsonl' 2>/dev/null | head -n 1)
"#
            ),
        };
        format!(
            "{locate}[ -n \"$f\" ] || exit {MISSING}\nexec tail -c {BACKLOG_BYTES} -F -- \"$f\"\n"
        )
    }
}

/// Sends `event`, or reports that nobody listens any more.
fn send(sender: &Sender<Event>, event: Event, cancelled: &AtomicBool) -> bool {
    let mut event = event;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return false;
        }
        match sender.try_send(event) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(back)) => {
                event = back;
                thread::sleep(SLICE);
            }
        }
    }
}

fn query(host: &Host, pane_id: &str, cancelled: &AtomicBool) -> crate::Result<Pane> {
    let (target, script) = host.script(&format!("herdr_cli pane get {}\n", shell_quote(pane_id)));
    let mut output = Vec::new();
    run_script(
        target,
        &script,
        io::empty(),
        &mut output,
        ScriptLimits {
            output: QUERY_OUTPUT,
            idle: QUERY_IDLE,
        },
        cancelled,
    )
    .map_err(crate::Error::ChatScript)?;
    serde_json::from_slice::<Envelope>(&output)
        .map(|envelope| envelope.result.pane)
        .map_err(crate::Error::ChatDecode)
}

struct Tail {
    cancelled: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    thread: thread::JoinHandle<()>,
}

impl Tail {
    fn stop(self) {
        self.cancelled.store(true, Ordering::Release);
        // The script's process group is killed within a poll interval.
        let _ = self.thread.join();
    }
}

fn watch(host: &Host, pane_id: &str, sender: &Sender<Event>, cancelled: &AtomicBool) {
    let interval = if host.is_remote() {
        REMOTE_POLL
    } else {
        LOCAL_POLL
    };
    let mut current: Option<Option<Session>> = None;
    let mut tail: Option<Tail> = None;
    let mut failing = false;
    while !cancelled.load(Ordering::Acquire) {
        // A log that ended (a missing file, a dropped SSH connection) is
        // looked for again on the next round.
        if tail
            .as_ref()
            .is_some_and(|tail| tail.finished.load(Ordering::Acquire))
        {
            if let Some(tail) = tail.take() {
                tail.stop();
            }
            current = None;
        }
        match query(host, pane_id, cancelled) {
            Ok(pane) => {
                failing = false;
                let session = pane.agent_session.as_ref().and_then(Session::of);
                if current.as_ref() != Some(&session) {
                    if let Some(tail) = tail.take() {
                        tail.stop();
                    }
                    let event = match &session {
                        Some(session) => {
                            tail = start_tail(host, session.clone(), sender.clone());
                            Event::Session(session.dialect)
                        }
                        None => Event::NoSession { agent: pane.agent },
                    };
                    current = Some(session);
                    if !send(sender, event, cancelled) {
                        break;
                    }
                }
            }
            Err(crate::Error::ChatScript(herdr_client::Error::ScriptCancelled)) => break,
            Err(error) => {
                if !failing && !send(sender, Event::Failed(error), cancelled) {
                    break;
                }
                failing = true;
            }
        }
        let mut waited = Duration::ZERO;
        while waited < interval && !cancelled.load(Ordering::Acquire) {
            thread::sleep(SLICE);
            waited += SLICE;
        }
    }
    if let Some(tail) = tail {
        tail.stop();
    }
}

fn start_tail(host: &Host, session: Session, sender: Sender<Event>) -> Option<Tail> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let host = host.clone();
    let (stop, done) = (cancelled.clone(), finished.clone());
    let spawned = thread::Builder::new()
        .name("herdr-chat-log".into())
        .spawn(move || {
            let mut sink = Sink {
                transcript: Transcript::new(session.dialect),
                partial: Vec::new(),
                sender: &sender,
                cancelled: &stop,
            };
            let (target, script) = host.script(&session.script());
            // Following never goes quiet on purpose, and its output is only
            // ever held one line at a time.
            let result = run_script(
                target,
                &script,
                io::empty(),
                &mut sink,
                ScriptLimits {
                    output: u64::MAX,
                    idle: Duration::MAX,
                },
                &stop,
            );
            match result {
                Err(herdr_client::Error::ScriptCancelled) => {}
                Err(herdr_client::Error::ScriptExit { status, .. })
                    if status.code() == Some(MISSING) =>
                {
                    let missing = crate::Error::ChatSessionMissing {
                        agent: session.dialect.agent(),
                    };
                    send(&sender, Event::Failed(missing), &stop);
                }
                Err(error) => {
                    send(
                        &sender,
                        Event::Failed(crate::Error::ChatScript(error)),
                        &stop,
                    );
                }
                Ok(_) => {}
            }
            done.store(true, Ordering::Release);
        });
    match spawned {
        Ok(thread) => Some(Tail {
            cancelled,
            finished,
            thread,
        }),
        Err(error) => {
            tracing::warn!(%error, "could not start the chat log reader");
            None
        }
    }
}

/// Splits streamed log bytes into lines, parses them, and sends what changed
/// after each chunk.
struct Sink<'a> {
    transcript: Transcript,
    partial: Vec<u8>,
    sender: &'a Sender<Event>,
    cancelled: &'a AtomicBool,
}

impl Write for Sink<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut rest = bytes;
        while let Some(end) = rest.iter().position(|&b| b == b'\n') {
            let (line, after) = rest.split_at(end);
            if self.partial.is_empty() {
                self.feed(line);
            } else {
                let mut whole = std::mem::take(&mut self.partial);
                whole.extend_from_slice(line);
                self.feed(&whole);
            }
            rest = &after[1..];
        }
        // An entry longer than the parser accepts is skipped whole rather
        // than buffered without bound.
        if self.partial.len() + rest.len() > super::transcript::MAX_LINE {
            self.partial = b"\0".to_vec();
        } else if self.partial != b"\0" {
            self.partial.extend_from_slice(rest);
        }
        if let Some(update) = self.transcript.drain()
            && !send(self.sender, Event::Update(update), self.cancelled)
        {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Sink<'_> {
    fn feed(&mut self, line: &[u8]) {
        if let Ok(line) = std::str::from_utf8(line) {
            self.transcript.feed(line);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn session(agent: &str, value: &str) -> AgentSession {
        AgentSession {
            agent: agent.into(),
            kind: SessionKind::Id,
            source: format!("herdr:{agent}"),
            value: value.into(),
        }
    }

    #[test]
    fn only_readable_sessions_with_plain_ids_are_followed() {
        assert_eq!(
            Session::of(&session("claude", "7a28-ab_c")),
            Some(Session {
                dialect: Dialect::Claude,
                id: "7a28-ab_c".into()
            })
        );
        assert_eq!(Session::of(&session("grok", "abc")), None);
        for id in ["", "../x", "a b", "a*", "$(x)"] {
            assert_eq!(Session::of(&session("codex", id)), None, "{id}");
        }
        let mut path = session("claude", "/tmp/x.jsonl");
        path.kind = SessionKind::Path;
        assert_eq!(Session::of(&path), None);
        let script = Session::of(&session("codex", "01a0")).unwrap().script();
        assert!(script.contains("'rollout-*-01a0.jsonl'"));
        assert!(script.contains(&format!("tail -c {BACKLOG_BYTES} -F")));
    }

    #[test]
    fn the_sink_parses_lines_split_across_chunks() {
        let (sender, events) = sync_channel(8);
        let cancelled = AtomicBool::new(false);
        let mut sink = Sink {
            transcript: Transcript::new(Dialect::Claude),
            partial: Vec::new(),
            sender: &sender,
            cancelled: &cancelled,
        };
        let line = br#"{"type":"user","message":{"role":"user","content":"hello"}}"#;
        sink.write_all(b"cut-off partial first line\n").unwrap();
        sink.write_all(&line[..20]).unwrap();
        assert!(events.try_recv().is_err(), "nothing whole yet");
        sink.write_all(&line[20..]).unwrap();
        sink.write_all(b"\n").unwrap();
        let Ok(Event::Update(update)) = events.try_recv() else {
            panic!("expected an update")
        };
        assert_eq!(update.items.len(), 1);
        // An oversized entry is skipped, and the next line still parses.
        sink.write_all(&vec![b'x'; super::super::transcript::MAX_LINE + 1])
            .unwrap();
        sink.write_all(b"\n").unwrap();
        sink.write_all(line).unwrap();
        sink.write_all(b"\n").unwrap();
        let Ok(Event::Update(update)) = events.try_recv() else {
            panic!("expected an update")
        };
        assert_eq!(update.items[0].id, 1);
        drop(events);
        assert!(sink.write_all(b"\n").is_ok(), "no change, nothing to send");
    }
}
