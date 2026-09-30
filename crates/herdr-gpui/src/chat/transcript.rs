//! An agent's own session log, read line by line into the conversation the
//! chat view shows. Claude Code and Codex each append JSON lines to one file
//! per session; this keeps what a reader wants (what the user asked, what
//! the agent said, and what it ran or changed) and drops the rest.
//!
//! Everything here is untrusted text from the agent's files: it is displayed,
//! never executed, and every piece is bounded so a runaway session cannot
//! grow the client without limit.

use serde_json::Value;
use std::collections::VecDeque;

/// Conversation items kept; older ones scroll away for good.
const MAX_ITEMS: usize = 600;
/// Characters kept of one message.
const MAX_TEXT: usize = 64 * 1024;
/// Characters kept of one tool's output.
const MAX_OUTPUT: usize = 16 * 1024;
/// Lines kept of one edit's diff.
const MAX_DIFF_LINES: usize = 400;
/// Lines compared exactly when an edit arrives without the agent's own patch.
const MAX_DIFF_INPUT: usize = 300;
/// Lines of one JSON log entry beyond this are not parsed at all.
pub(crate) const MAX_LINE: usize = 4 * 1024 * 1024;

/// Whose log format a session file is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dialect {
    Claude,
    Codex,
}

impl Dialect {
    /// The dialect of the agent Herdr reports for a pane, by its name.
    pub(crate) fn of_agent(agent: &str) -> Option<Self> {
        match agent {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    pub(crate) fn agent(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Entry {
    User(String),
    Assistant(String),
    Tool(Tool),
    Todos(Vec<Todo>),
    /// Something that happened to the session rather than in it.
    Notice(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Tool {
    /// The agent's call id, matching a later result to its call.
    pub id: String,
    pub kind: ToolKind,
    pub state: ToolState,
    pub output: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolState {
    Running,
    Done,
    Failed,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ToolKind {
    Command {
        command: String,
        description: Option<String>,
    },
    Edit {
        path: String,
        /// Unified diff body: each line starts with ' ', '+', or '-', or is a
        /// hunk header: `@@ -old,count +new,count @@`, or a bare `@@` where
        /// the agent's log gives no line numbers.
        diff: Vec<String>,
        created: bool,
    },
    Read {
        path: String,
    },
    Search {
        pattern: String,
        scope: Option<String>,
    },
    Web {
        target: String,
    },
    Delegate {
        description: String,
    },
    Other {
        name: String,
        summary: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Todo {
    pub text: String,
    pub state: TodoState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TodoState {
    Pending,
    Active,
    Done,
}

/// One conversation item with a stable identity: `id` never changes and
/// `version` grows whenever the item does, so a view can cache by both.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Item {
    pub id: u64,
    pub version: u64,
    pub entry: Entry,
}

#[derive(Debug)]
pub(crate) struct Transcript {
    dialect: Dialect,
    items: VecDeque<Item>,
    next_id: u64,
    /// Grows with every change, for cheap "anything new?" checks.
    revision: u64,
    title: Option<String>,
    model: Option<String>,
    /// When the turn in progress began, in Unix milliseconds.
    turn_started: Option<u64>,
    /// Items added or changed since the last `drain`.
    changed: Vec<u64>,
    /// Whether the title, model, or turn changed since the last `drain`.
    meta_changed: bool,
}

/// What changed in a transcript, for a reader that keeps its own copy.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Update {
    /// New or changed items, oldest first.
    pub items: Vec<Item>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub turn_started: Option<u64>,
}

impl Transcript {
    pub(crate) fn new(dialect: Dialect) -> Self {
        Self {
            dialect,
            items: VecDeque::new(),
            next_id: 0,
            revision: 0,
            title: None,
            model: None,
            turn_started: None,
            changed: Vec::new(),
            meta_changed: false,
        }
    }

    /// Everything that changed since the last call, if anything did.
    pub(crate) fn drain(&mut self) -> Option<Update> {
        if self.changed.is_empty() && !self.meta_changed {
            return None;
        }
        self.meta_changed = false;
        let mut ids = std::mem::take(&mut self.changed);
        ids.sort_unstable();
        ids.dedup();
        let first = self.items.front().map_or(0, |item| item.id);
        let items = ids
            .into_iter()
            .filter(|id| *id >= first)
            .filter_map(|id| self.items.get(usize::try_from(id - first).ok()?).cloned())
            .collect();
        Some(Update {
            items,
            title: self.title.clone(),
            model: self.model.clone(),
            turn_started: self.turn_started,
        })
    }

    #[cfg(test)]
    pub(crate) fn items(&self) -> &VecDeque<Item> {
        &self.items
    }

    /// The session's title, when the agent named it.
    #[cfg(test)]
    pub(crate) fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn turn_started(&self) -> Option<u64> {
        self.turn_started
    }

    /// Reads one log line; lines that are not JSON, or that hold nothing to
    /// show, change nothing. Returns whether anything changed.
    pub(crate) fn feed(&mut self, line: &str) -> bool {
        let line = line.trim();
        if line.is_empty() || line.len() > MAX_LINE {
            return false;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return false;
        };
        let before = self.revision;
        match self.dialect {
            Dialect::Claude => self.claude(&value),
            Dialect::Codex => self.codex(&value),
        }
        self.revision != before
    }

    fn push(&mut self, entry: Entry) {
        // Consecutive notices of one kind say nothing new.
        if let (
            Entry::Notice(text),
            Some(Item {
                entry: Entry::Notice(last),
                ..
            }),
        ) = (&entry, self.items.back())
            && text == last
        {
            return;
        }
        self.items.push_back(Item {
            id: self.next_id,
            version: 0,
            entry,
        });
        self.changed.push(self.next_id);
        self.next_id += 1;
        while self.items.len() > MAX_ITEMS {
            self.items.pop_front();
        }
        self.revision += 1;
    }

    /// The running or finished tool call with `id`, newest first.
    fn tool_mut(&mut self, id: &str) -> Option<&mut Item> {
        self.items
            .iter_mut()
            .rev()
            .find(|item| matches!(&item.entry, Entry::Tool(tool) if tool.id == id))
    }

    fn set_title(&mut self, title: &str) {
        let title = clip(title.trim(), 200);
        if !title.is_empty() && self.title.as_deref() != Some(title.as_str()) {
            self.title = Some(title);
            self.revision += 1;
            self.meta_changed = true;
        }
    }

    fn set_model(&mut self, model: &str) {
        if !model.is_empty() && self.model.as_deref() != Some(model) {
            self.model = Some(clip(model, 80));
            self.revision += 1;
            self.meta_changed = true;
        }
    }

    fn set_turn_started(&mut self, at: Option<u64>) {
        if self.turn_started != at {
            self.turn_started = at;
            self.revision += 1;
            self.meta_changed = true;
        }
    }

    fn claude(&mut self, value: &Value) {
        if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return;
        }
        match value.get("type").and_then(Value::as_str) {
            Some("ai-title") => {
                if let Some(title) = value.get("aiTitle").and_then(Value::as_str) {
                    self.set_title(title);
                }
            }
            Some("user") => self.claude_user(value),
            Some("assistant") => self.claude_assistant(value),
            Some("system")
                if value.get("subtype").and_then(Value::as_str) == Some("compact_boundary") =>
            {
                self.push(Entry::Notice("Conversation compacted".into()));
            }
            _ => {}
        }
    }

    fn claude_user(&mut self, value: &Value) {
        if value.get("isMeta").and_then(Value::as_bool) == Some(true) {
            return;
        }
        if value.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
            self.push(Entry::Notice("Conversation compacted".into()));
            return;
        }
        let Some(content) = value.pointer("/message/content") else {
            return;
        };
        let at = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp);
        match content {
            Value::String(text) => self.claude_prompt(text, at),
            Value::Array(blocks) => {
                let mut text = String::new();
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("tool_result") => {
                            self.claude_result(block, value.get("toolUseResult"))
                        }
                        Some("text") => {
                            if let Some(part) = block.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(part);
                            }
                        }
                        Some("image") => {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str("[image]");
                        }
                        _ => {}
                    }
                }
                if !text.is_empty() {
                    self.claude_prompt(&text, at);
                }
            }
            _ => {}
        }
    }

    /// Something the user typed, or Claude Code's wrapping of a command.
    fn claude_prompt(&mut self, text: &str, at: Option<u64>) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if let Some(name) = between(text, "<command-name>", "</command-name>") {
            let args = between(text, "<command-args>", "</command-args>").unwrap_or_default();
            let command = format!("{} {}", name.trim(), args.trim());
            self.push(Entry::Notice(clip(command.trim(), 200)));
            return;
        }
        if let Some(command) = between(text, "<bash-input>", "</bash-input>") {
            self.push(Entry::User(clip(
                &format!("! {}", command.trim()),
                MAX_TEXT,
            )));
            self.set_turn_started(at);
            return;
        }
        if text.starts_with("<task-notification>") {
            let summary = between(text, "<summary>", "</summary>")
                .map(str::trim)
                .filter(|summary| !summary.is_empty())
                .unwrap_or("A background task finished");
            self.push(Entry::Notice(clip(summary, 200)));
            return;
        }
        if text.starts_with('<') {
            // Command output, caveats, and reminders the agent reads but the
            // user never typed.
            return;
        }
        if text.starts_with("[Request interrupted") {
            self.push(Entry::Notice("Interrupted".into()));
            self.set_turn_started(None);
            return;
        }
        self.push(Entry::User(clip(text, MAX_TEXT)));
        self.set_turn_started(at);
    }

    fn claude_assistant(&mut self, value: &Value) {
        if let Some(model) = value.pointer("/message/model").and_then(Value::as_str)
            && !model.starts_with('<')
        {
            self.set_model(model);
        }
        let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) else {
            return;
        };
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    if !text.is_empty() && text != "(no content)" {
                        self.push(Entry::Assistant(clip(text, MAX_TEXT)));
                    }
                }
                Some("tool_use") => {
                    let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                    let input = block.get("input").unwrap_or(&Value::Null);
                    if name == "TodoWrite" {
                        if let Some(todos) = claude_todos(input) {
                            self.push(Entry::Todos(todos));
                        }
                        continue;
                    }
                    self.push(Entry::Tool(Tool {
                        id: clip(id, 128),
                        kind: claude_tool(name, input),
                        state: ToolState::Running,
                        output: String::new(),
                    }));
                }
                _ => {}
            }
        }
    }

    fn claude_result(&mut self, block: &Value, result: Option<&Value>) {
        let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
            return;
        };
        let failed = block.get("is_error").and_then(Value::as_bool) == Some(true);
        let output = match block.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        let patch = result
            .and_then(|result| result.get("structuredPatch"))
            .and_then(patch_lines);
        let Some(item) = self.tool_mut(id) else {
            return;
        };
        let Entry::Tool(tool) = &mut item.entry else {
            return;
        };
        tool.state = if failed {
            ToolState::Failed
        } else {
            ToolState::Done
        };
        tool.output = tail(&output, MAX_OUTPUT);
        if let (ToolKind::Edit { diff, .. }, Some(patch)) = (&mut tool.kind, patch)
            && !patch.is_empty()
        {
            *diff = patch;
        }
        item.version += 1;
        let id = item.id;
        self.changed.push(id);
        self.revision += 1;
    }

    fn codex(&mut self, value: &Value) {
        let payload = value.get("payload").unwrap_or(&Value::Null);
        match value.get("type").and_then(Value::as_str) {
            Some("turn_context") => {
                if let Some(model) = payload.get("model").and_then(Value::as_str) {
                    self.set_model(model);
                }
            }
            Some("event_msg") => match payload.get("type").and_then(Value::as_str) {
                Some("item_completed") => {
                    if let Some(item) = payload.get("item") {
                        self.codex_item(item);
                    }
                }
                Some("task_started") => {
                    let at = payload
                        .get("started_at")
                        .and_then(Value::as_u64)
                        .map(|secs| secs.saturating_mul(1000));
                    self.set_turn_started(at);
                }
                Some("task_complete") => self.set_turn_started(None),
                Some("turn_aborted") => {
                    self.push(Entry::Notice("Interrupted".into()));
                    self.set_turn_started(None);
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn codex_item(&mut self, item: &Value) {
        let id = clip(item.get("id").and_then(Value::as_str).unwrap_or(""), 128);
        match item.get("type").and_then(Value::as_str) {
            Some("UserMessage") => {
                let text = codex_text(item);
                let text = text.trim();
                if !text.is_empty() && !text.starts_with('<') {
                    self.push(Entry::User(clip(text, MAX_TEXT)));
                }
            }
            Some("AgentMessage") => {
                let text = codex_text(item);
                if !text.trim().is_empty() {
                    self.push(Entry::Assistant(clip(text.trim(), MAX_TEXT)));
                }
            }
            Some("Plan") => {
                if let Some(text) = item.get("text").and_then(Value::as_str)
                    && !text.trim().is_empty()
                {
                    self.push(Entry::Assistant(clip(text.trim(), MAX_TEXT)));
                }
            }
            Some("CommandExecution") => {
                let command = match item.get("command") {
                    // `["/bin/zsh", "-lc", script]`: the script is the command.
                    Some(Value::Array(argv)) => {
                        argv.last().and_then(Value::as_str).unwrap_or("").to_owned()
                    }
                    Some(Value::String(command)) => command.clone(),
                    _ => String::new(),
                };
                let output = item
                    .get("aggregated_output")
                    .or_else(|| item.get("stdout"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let failed = item
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .is_some_and(|code| code != 0)
                    || item.get("status").and_then(Value::as_str) == Some("failed");
                self.push(Entry::Tool(Tool {
                    id,
                    kind: ToolKind::Command {
                        command: clip(command.trim(), 8 * 1024),
                        description: None,
                    },
                    state: if failed {
                        ToolState::Failed
                    } else {
                        ToolState::Done
                    },
                    output: tail(output, MAX_OUTPUT),
                }));
            }
            Some("FileChange") => {
                let Some(changes) = item.get("changes").and_then(Value::as_object) else {
                    return;
                };
                let failed = item.get("status").and_then(Value::as_str) == Some("failed");
                for (path, change) in changes.iter().take(32) {
                    let (diff, created) = match change.get("type").and_then(Value::as_str) {
                        Some("add") => (
                            added_lines(
                                change.get("content").and_then(Value::as_str).unwrap_or(""),
                            ),
                            true,
                        ),
                        Some("delete") => (vec!["-(file deleted)".to_owned()], false),
                        _ => (
                            unified_lines(
                                change
                                    .get("unified_diff")
                                    .and_then(Value::as_str)
                                    .unwrap_or(""),
                            ),
                            false,
                        ),
                    };
                    self.push(Entry::Tool(Tool {
                        id: id.clone(),
                        kind: ToolKind::Edit {
                            path: clip(path, 1024),
                            diff,
                            created,
                        },
                        state: if failed {
                            ToolState::Failed
                        } else {
                            ToolState::Done
                        },
                        output: String::new(),
                    }));
                }
            }
            Some("McpToolCall") => {
                let server = item.get("server").and_then(Value::as_str).unwrap_or("");
                let tool = item.get("tool").and_then(Value::as_str).unwrap_or("tool");
                let arguments = item.get("arguments").unwrap_or(&Value::Null);
                let summary = arguments
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| compact_json(arguments));
                let failed = item.get("status").and_then(Value::as_str) == Some("failed");
                self.push(Entry::Tool(Tool {
                    id,
                    kind: ToolKind::Other {
                        name: clip(&format!("{server}.{tool}"), 120),
                        summary: clip(&summary, 400),
                    },
                    state: if failed {
                        ToolState::Failed
                    } else {
                        ToolState::Done
                    },
                    output: String::new(),
                }));
            }
            Some("Extension") => {
                let queries: Vec<&str> = item
                    .pointer("/action/queries")
                    .and_then(Value::as_array)
                    .map(|queries| queries.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let target = if queries.is_empty() {
                    item.get("query")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned()
                } else {
                    queries.join(" · ")
                };
                self.push(Entry::Tool(Tool {
                    id,
                    kind: ToolKind::Web {
                        target: clip(&target, 400),
                    },
                    state: ToolState::Done,
                    output: String::new(),
                }));
            }
            Some("ImageView") => {
                let path = item.get("path").and_then(Value::as_str).unwrap_or("");
                self.push(Entry::Tool(Tool {
                    id,
                    kind: ToolKind::Read {
                        path: clip(path.strip_prefix("file://").unwrap_or(path), 1024),
                    },
                    state: ToolState::Done,
                    output: String::new(),
                }));
            }
            Some("ContextCompaction") => self.push(Entry::Notice("Conversation compacted".into())),
            _ => {}
        }
    }
}

fn claude_tool(name: &str, input: &Value) -> ToolKind {
    let text = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or("");
    match name {
        "Bash" => ToolKind::Command {
            command: clip(text("command").trim(), 8 * 1024),
            description: Some(clip(text("description").trim(), 400)).filter(|d| !d.is_empty()),
        },
        "Edit" => ToolKind::Edit {
            path: clip(text("file_path"), 1024),
            diff: line_diff(text("old_string"), text("new_string")),
            created: false,
        },
        "MultiEdit" => {
            let mut diff = Vec::new();
            for edit in input
                .get("edits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let part = |key: &str| edit.get(key).and_then(Value::as_str).unwrap_or("");
                if !diff.is_empty() {
                    diff.push("@@".to_owned());
                }
                diff.extend(line_diff(part("old_string"), part("new_string")));
            }
            diff.truncate(MAX_DIFF_LINES);
            ToolKind::Edit {
                path: clip(text("file_path"), 1024),
                diff,
                created: false,
            }
        }
        "Write" => ToolKind::Edit {
            path: clip(text("file_path"), 1024),
            diff: added_lines(text("content")),
            created: true,
        },
        "Read" | "NotebookRead" => ToolKind::Read {
            path: clip(text("file_path"), 1024),
        },
        "Grep" | "Glob" => ToolKind::Search {
            pattern: clip(text("pattern"), 400),
            scope: Some(clip(text("path"), 1024)).filter(|p| !p.is_empty()),
        },
        "WebFetch" => ToolKind::Web {
            target: clip(text("url"), 1024),
        },
        "WebSearch" => ToolKind::Web {
            target: clip(text("query"), 400),
        },
        "Task" | "Agent" => ToolKind::Delegate {
            description: clip(text("description"), 400),
        },
        _ => ToolKind::Other {
            name: clip(name, 120),
            summary: clip(
                &[
                    "description",
                    "command",
                    "query",
                    "url",
                    "file_path",
                    "pattern",
                    "prompt",
                ]
                .iter()
                .map(|key| text(key))
                .find(|value| !value.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| compact_json(input)),
                400,
            ),
        },
    }
}

fn claude_todos(input: &Value) -> Option<Vec<Todo>> {
    let todos = input.get("todos")?.as_array()?;
    Some(
        todos
            .iter()
            .take(50)
            .filter_map(|todo| {
                let text = todo.get("content").and_then(Value::as_str)?;
                let state = match todo.get("status").and_then(Value::as_str) {
                    Some("completed") => TodoState::Done,
                    Some("in_progress") => TodoState::Active,
                    _ => TodoState::Pending,
                };
                Some(Todo {
                    text: clip(text, 400),
                    state,
                })
            })
            .collect(),
    )
}

fn codex_text(item: &Value) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Claude Code's `structuredPatch`: hunks whose lines already carry their
/// diff prefix.
fn patch_lines(patch: &Value) -> Option<Vec<String>> {
    let hunks = patch.as_array()?;
    let mut lines = Vec::new();
    for hunk in hunks {
        let number = |key: &str| hunk.get(key).and_then(Value::as_u64).unwrap_or(0);
        lines.push(format!(
            "@@ -{},{} +{},{} @@",
            number("oldStart"),
            number("oldLines"),
            number("newStart"),
            number("newLines")
        ));
        for line in hunk.get("lines")?.as_array()? {
            if lines.len() >= MAX_DIFF_LINES {
                return Some(lines);
            }
            lines.push(clip(line.as_str()?, 2048));
        }
    }
    Some(lines)
}

fn unified_lines(diff: &str) -> Vec<String> {
    diff.lines()
        .filter(|line| !line.starts_with("---") && !line.starts_with("+++"))
        .map(|line| clip(line, 2048))
        .take(MAX_DIFF_LINES)
        .collect()
}

fn added_lines(content: &str) -> Vec<String> {
    content
        .lines()
        .take(MAX_DIFF_LINES)
        .map(|line| clip(&format!("+{line}"), 2048))
        .collect()
}

/// A line diff of an edit's old and new text: an exact longest common
/// subsequence for edits of ordinary size, and "all removed, all added"
/// beyond that, where exactness would cost more than it shows.
fn line_diff(old: &str, new: &str) -> Vec<String> {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    let mut out = Vec::new();
    if old.len() > MAX_DIFF_INPUT || new.len() > MAX_DIFF_INPUT {
        out.extend(old.iter().map(|line| format!("-{line}")));
        out.extend(new.iter().map(|line| format!("+{line}")));
    } else {
        // lengths[i][j]: common lines of old[i..] and new[j..].
        let mut lengths = vec![vec![0u16; new.len() + 1]; old.len() + 1];
        for i in (0..old.len()).rev() {
            for j in (0..new.len()).rev() {
                lengths[i][j] = if old[i] == new[j] {
                    lengths[i + 1][j + 1] + 1
                } else {
                    lengths[i + 1][j].max(lengths[i][j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < old.len() || j < new.len() {
            if i < old.len() && j < new.len() && old[i] == new[j] {
                out.push(format!(" {}", old[i]));
                i += 1;
                j += 1;
            } else if i < old.len() && (j == new.len() || lengths[i + 1][j] >= lengths[i][j + 1]) {
                // Removals first, as unified diffs show a replaced line.
                out.push(format!("-{}", old[i]));
                i += 1;
            } else {
                out.push(format!("+{}", new[j]));
                j += 1;
            }
        }
    }
    out.truncate(MAX_DIFF_LINES);
    out.into_iter().map(|line| clip(&line, 2048)).collect()
}

fn compact_json(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        _ => clip(&value.to_string(), 400),
    }
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)? + start;
    Some(&text[start..end])
}

/// At most `limit` characters of `text`, from the start, with control
/// characters other than newlines and tabs removed: they are displayed, and
/// terminal escapes must not reach the screen.
pub(crate) fn clip(text: &str, limit: usize) -> String {
    let mut out: String = text
        .chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        .take(limit)
        .collect();
    if text.chars().count() > limit {
        out.push('…');
    }
    out
}

/// The last `limit` characters of `text`, cleaned as `clip` cleans.
fn tail(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    let skip = count.saturating_sub(limit);
    let kept: String = text
        .chars()
        .skip(skip)
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        .collect();
    if skip > 0 { format!("…{kept}") } else { kept }
}

/// `2026-09-30T12:48:00.392Z` as Unix milliseconds. Only the UTC form the
/// agents write is accepted.
pub(crate) fn parse_timestamp(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<u64> { text.get(range)?.parse().ok() };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let rest = text.get(19..)?;
    let millis = match rest.strip_prefix('.') {
        Some(fraction) => {
            let digits: String = fraction
                .chars()
                .take_while(char::is_ascii_digit)
                .take(3)
                .collect();
            if digits.is_empty() || !fraction.ends_with('Z') {
                return None;
            }
            format!("{digits:0<3}").parse::<u64>().ok()?
        }
        None if rest == "Z" => 0,
        None => return None,
    };
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm), in u64 because
    // years before 1970 were rejected above.
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = y / 400;
    let year_of_era = y - era * 400;
    let day_of_year = (153 * m + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = (era * 146_097 + day_of_era).checked_sub(719_468)?;
    Some((((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use serde_json::json;

    fn feed(transcript: &mut Transcript, value: Value) -> bool {
        transcript.feed(&value.to_string())
    }

    fn entries(transcript: &Transcript) -> Vec<&Entry> {
        transcript.items().iter().map(|item| &item.entry).collect()
    }

    #[test]
    fn claude_sessions_read_as_a_conversation() {
        let mut t = Transcript::new(Dialect::Claude);
        assert!(!t.feed("not json"));
        assert!(!feed(&mut t, json!({"type": "mode", "mode": "normal"})));
        feed(
            &mut t,
            json!({"type": "ai-title", "aiTitle": "Fix the drop preview"}),
        );
        assert_eq!(t.title(), Some("Fix the drop preview"));
        feed(
            &mut t,
            json!({"type": "user", "isMeta": true, "message": {"role": "user", "content": "<local-command-caveat>x</local-command-caveat>"}}),
        );
        feed(
            &mut t,
            json!({"type": "user", "timestamp": "2026-09-30T12:48:00.392Z", "message": {"role": "user", "content": "Why is the build red?"}}),
        );
        assert_eq!(
            t.turn_started(),
            parse_timestamp("2026-09-30T12:48:00.392Z")
        );
        feed(
            &mut t,
            json!({"type": "assistant", "message": {"model": "claude-opus-5-5", "content": [{"type": "thinking", "thinking": ""}]}}),
        );
        feed(
            &mut t,
            json!({"type": "assistant", "message": {"model": "claude-opus-5-5", "content": [{"type": "text", "text": "Checking **CI**."}]}}),
        );
        feed(
            &mut t,
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "just ci", "description": "Run CI"}}]}}),
        );
        assert!(feed(
            &mut t,
            json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "error\u{1b}[31m: failed", "is_error": true}]}}),
        ));
        feed(
            &mut t,
            json!({"type": "assistant", "isSidechain": true, "message": {"content": [{"type": "text", "text": "subagent chatter"}]}}),
        );
        assert_eq!(t.model(), Some("claude-opus-5-5"));
        let got = entries(&t);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], &Entry::User("Why is the build red?".into()));
        assert_eq!(got[1], &Entry::Assistant("Checking **CI**.".into()));
        let Entry::Tool(tool) = got[2] else {
            panic!("{got:?}")
        };
        assert_eq!(tool.state, ToolState::Failed);
        assert_eq!(
            tool.output, "error[31m: failed",
            "escapes never reach the screen"
        );
        assert_eq!(
            tool.kind,
            ToolKind::Command {
                command: "just ci".into(),
                description: Some("Run CI".into())
            }
        );
        assert_eq!(t.items()[2].version, 1, "a result bumps the call's version");
    }

    #[test]
    fn claude_edits_show_their_patch_and_commands_become_notices() {
        let mut t = Transcript::new(Dialect::Claude);
        feed(
            &mut t,
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "e1", "name": "Edit", "input": {"file_path": "/a.rs", "old_string": "a\nb\nc", "new_string": "a\nB\nc"}}]}}),
        );
        let Entry::Tool(Tool {
            kind: ToolKind::Edit { diff, .. },
            ..
        }) = &t.items()[0].entry
        else {
            panic!()
        };
        assert_eq!(diff, &[" a", "-b", "+B", " c"]);
        feed(
            &mut t,
            json!({"type": "user", "toolUseResult": {"structuredPatch": [{"oldStart": 41, "oldLines": 2, "newStart": 41, "newLines": 2, "lines": [" a", "-b", "+B"]}, {"oldStart": 90, "oldLines": 0, "newStart": 90, "newLines": 1, "lines": ["+z"]}]}, "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "e1", "content": "ok"}]}}),
        );
        let Entry::Tool(Tool {
            kind: ToolKind::Edit { diff, .. },
            state,
            ..
        }) = &t.items()[0].entry
        else {
            panic!()
        };
        assert_eq!(
            diff,
            &[
                "@@ -41,2 +41,2 @@",
                " a",
                "-b",
                "+B",
                "@@ -90,0 +90,1 @@",
                "+z"
            ]
        );
        assert_eq!(*state, ToolState::Done);
        feed(
            &mut t,
            json!({"type": "user", "message": {"role": "user", "content": "<command-name>/clear</command-name>\n<command-args></command-args>"}}),
        );
        feed(
            &mut t,
            json!({"type": "user", "message": {"role": "user", "content": "[Request interrupted by user]"}}),
        );
        feed(
            &mut t,
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "d", "name": "TodoWrite", "input": {"todos": [{"content": "Parse", "status": "completed"}, {"content": "Paint", "status": "in_progress"}]}}]}}),
        );
        let got = entries(&t);
        assert_eq!(got[1], &Entry::Notice("/clear".into()));
        assert_eq!(got[2], &Entry::Notice("Interrupted".into()));
        assert_eq!(
            got[3],
            &Entry::Todos(vec![
                Todo {
                    text: "Parse".into(),
                    state: TodoState::Done
                },
                Todo {
                    text: "Paint".into(),
                    state: TodoState::Active
                },
            ])
        );
    }

    #[test]
    fn codex_rollouts_read_as_a_conversation() {
        let mut t = Transcript::new(Dialect::Codex);
        feed(
            &mut t,
            json!({"type": "turn_context", "payload": {"model": "gpt-6-astra"}}),
        );
        feed(
            &mut t,
            json!({"type": "event_msg", "payload": {"type": "task_started", "started_at": 1_790_361_458u64}}),
        );
        assert_eq!(t.turn_started(), Some(1_790_361_458_000));
        let completed = |item: Value| json!({"type": "event_msg", "payload": {"type": "item_completed", "item": item}});
        feed(
            &mut t,
            completed(
                json!({"type": "UserMessage", "content": [{"type": "text", "text": "<environment_context>x"}]}),
            ),
        );
        feed(
            &mut t,
            completed(
                json!({"type": "UserMessage", "content": [{"type": "text", "text": "Review the SMS code"}]}),
            ),
        );
        feed(
            &mut t,
            completed(json!({"type": "Reasoning", "summary_text": []})),
        );
        feed(
            &mut t,
            completed(
                json!({"type": "AgentMessage", "content": [{"type": "Text", "text": "Tracing sender selection."}], "phase": "commentary"}),
            ),
        );
        feed(
            &mut t,
            completed(
                json!({"type": "CommandExecution", "id": "c1", "command": ["/bin/zsh", "-lc", "rg -n sms"], "aggregated_output": "1:sms", "exit_code": 0, "status": "completed"}),
            ),
        );
        feed(
            &mut t,
            completed(
                json!({"type": "FileChange", "id": "f1", "status": "completed", "changes": {"/x.ts": {"type": "update", "unified_diff": "@@ -1,2 +1,2 @@\n-a\n+b\n c\n"}, "/new.md": {"type": "add", "content": "# Hi\n"}}}),
            ),
        );
        feed(
            &mut t,
            json!({"type": "event_msg", "payload": {"type": "task_complete"}}),
        );
        assert_eq!(t.turn_started(), None);
        assert_eq!(t.model(), Some("gpt-6-astra"));
        let got = entries(&t);
        assert_eq!(got[0], &Entry::User("Review the SMS code".into()));
        assert_eq!(
            got[1],
            &Entry::Assistant("Tracing sender selection.".into())
        );
        assert!(
            matches!(got[2], Entry::Tool(Tool { kind: ToolKind::Command { command, .. }, state: ToolState::Done, output, .. }) if command == "rg -n sms" && output == "1:sms")
        );
        // One item per changed file, in whatever order the log's map holds.
        let edits: Vec<(&Vec<String>, bool)> = got[3..]
            .iter()
            .filter_map(|entry| match entry {
                Entry::Tool(Tool {
                    kind: ToolKind::Edit { diff, created, .. },
                    ..
                }) => Some((diff, *created)),
                _ => None,
            })
            .collect();
        assert_eq!(edits.len(), 2);
        assert!(edits.contains(&(&vec!["+# Hi".to_owned()], true)));
        assert!(edits.contains(&(
            &vec![
                "@@ -1,2 +1,2 @@".to_owned(),
                "-a".to_owned(),
                "+b".to_owned(),
                " c".to_owned()
            ],
            false
        )));
    }

    #[test]
    fn drains_carry_only_what_changed() {
        let mut t = Transcript::new(Dialect::Claude);
        assert_eq!(t.drain(), None);
        feed(
            &mut t,
            json!({"type": "user", "message": {"role": "user", "content": "hi"}}),
        );
        feed(
            &mut t,
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}}]}}),
        );
        let update = t.drain().unwrap();
        assert_eq!(
            update.items.iter().map(|item| item.id).collect::<Vec<_>>(),
            [0, 1]
        );
        assert_eq!(t.drain(), None);
        feed(
            &mut t,
            json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "a"}]}}),
        );
        let update = t.drain().unwrap();
        assert_eq!(update.items.len(), 1);
        assert_eq!((update.items[0].id, update.items[0].version), (1, 1));
        feed(&mut t, json!({"type": "ai-title", "aiTitle": "Listing"}));
        let update = t.drain().unwrap();
        assert!(update.items.is_empty());
        assert_eq!(update.title.as_deref(), Some("Listing"));
    }

    #[test]
    fn transcripts_stay_bounded() {
        let mut t = Transcript::new(Dialect::Claude);
        for i in 0..MAX_ITEMS + 25 {
            feed(
                &mut t,
                json!({"type": "user", "message": {"role": "user", "content": format!("question {i}")}}),
            );
        }
        assert_eq!(t.items().len(), MAX_ITEMS);
        assert_eq!(
            t.items()[0].id,
            25,
            "the oldest scroll away, ids stay stable"
        );
        let long = "x".repeat(MAX_TEXT + 10);
        feed(
            &mut t,
            json!({"type": "user", "message": {"role": "user", "content": long}}),
        );
        let Some(Item {
            entry: Entry::User(text),
            ..
        }) = t.items().back()
        else {
            panic!()
        };
        assert_eq!(text.chars().count(), MAX_TEXT + 1);
        assert!(line_diff(&"a\n".repeat(400), "b").len() <= MAX_DIFF_LINES);
    }

    #[test]
    fn timestamps_parse_as_unix_milliseconds() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_timestamp("2026-09-30T12:48:00.392Z"),
            Some(1_790_772_480_392)
        );
        assert_eq!(
            parse_timestamp("2000-02-29T23:59:59.5Z"),
            Some(951_868_799_500)
        );
        for bad in [
            "",
            "2026-09-30",
            "2026-13-01T00:00:00Z",
            "2026-09-30T12:48:00+02:00",
            "1969-12-31T23:59:59Z",
        ] {
            assert_eq!(parse_timestamp(bad), None, "{bad}");
        }
    }
}
