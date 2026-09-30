//! Mermaid diagrams for the chat view: a small parser and layered layout engine.
//!
//! [`parse`] recognizes the flowchart and sequence dialects that agents commonly
//! write and returns `None` for anything else, so the caller can fall back to a
//! plain code block. [`Diagram::layout`] turns a parsed diagram into a [`Scene`]
//! of primitives in logical pixels. Text is measured by the caller, so boxes are
//! sized for the fonts that will actually paint them.
//!
//! Flowcharts use a Sugiyama-style pipeline: cycles are broken by reversing DFS
//! back edges, nodes are layered by longest path, long edges get dummy vertices,
//! layers are ordered by barycenter sweeps, and coordinates come from a weighted
//! least-squares relaxation whose per-layer solve is an exact isotonic regression.
//! Layout always runs top-down; other directions rotate or mirror the result,
//! with node sizes swapped beforehand so boxes are never rotated.
//!
//! Everything here is pure, deterministic, and bounded: inputs beyond the node,
//! edge, and message caps are rejected rather than laid out slowly, and malformed
//! input degrades to a partial diagram or `None`, never a panic.

use std::collections::HashMap;

/// Largest flowchart (nodes) or sequence diagram (participants) we lay out.
const MAX_NODES: usize = 200;
const MAX_EDGES: usize = 400;
const MAX_MESSAGES: usize = 300;
/// Notes, activations, and block markers also cost layout rows.
const MAX_SEQUENCE_EVENTS: usize = 1_000;
const MAX_CLUSTERS: usize = 100;
const MAX_NESTING: usize = 12;
/// Long edges beyond this total bend-point budget are drawn without bends.
const MAX_DUMMIES: usize = 4_000;
const MAX_LABEL_CHARS: usize = 400;
/// Keeps pathological fences from even being scanned.
const MAX_SOURCE_BYTES: usize = 256 * 1024;
/// Caller measurements are clamped so one bad value cannot blow up the scene.
const MAX_EXTENT: f32 = 4_096.0;

const MARGIN: f32 = 12.0;
const NODE_PAD_X: f32 = 12.0;
const NODE_PAD_Y: f32 = 8.0;
const MIN_NODE_WIDTH: f32 = 40.0;
const CYLINDER_CAP: f32 = 12.0;
const LAYER_GAP: f32 = 48.0;
const NODE_GAP: f32 = 28.0;
const DUMMY_GAP: f32 = 12.0;
const DUMMY_NODE_GAP: f32 = 16.0;
const LABEL_PAD_X: f32 = 6.0;
const LABEL_PAD_Y: f32 = 3.0;
const CLUSTER_PAD: f32 = 12.0;
/// Space kept between a cluster outline and nodes that are not in it.
const CLUSTER_CLEARANCE: f32 = 10.0;
const TITLE_INSET_X: f32 = 10.0;
const TITLE_INSET_Y: f32 = 6.0;
const SELF_LOOP_REACH: f32 = 26.0;
const SELF_LOOP_STEP: f32 = 12.0;
const ORDER_SWEEPS: usize = 12;
const TRANSPOSE_PASSES: usize = 4;
const X_ITERATIONS: usize = 48;
const OVERLAP_PASSES: usize = 16;
/// Pixels below which the L1 reweighting treats an edge as already straight.
const L1_SOFTNESS: f32 = 2.0;
/// Offsets below this snap to a perfectly straight edge when room allows.
const SNAP_DISTANCE: f32 = 12.0;
const CURVE_STEPS: usize = 12;

const SEQ_BOX_PAD_X: f32 = 14.0;
const SEQ_BOX_PAD_Y: f32 = 9.0;
const SEQ_MIN_BOX_WIDTH: f32 = 72.0;
const SEQ_COLUMN_GAP: f32 = 40.0;
const SEQ_TOP_GAP: f32 = 22.0;
const SEQ_LABEL_GAP: f32 = 5.0;
const SEQ_ROW_GAP: f32 = 16.0;
const SEQ_LABEL_CLEARANCE: f32 = 18.0;
const SEQ_SELF_WIDTH: f32 = 34.0;
const SEQ_SELF_HEIGHT: f32 = 18.0;
const ACTIVATION_WIDTH: f32 = 10.0;
const ACTIVATION_STEP: f32 = 4.0;
const ACTIVATION_MIN: f32 = 12.0;
const NOTE_PAD_X: f32 = 10.0;
const NOTE_PAD_Y: f32 = 7.0;
const NOTE_OFFSET: f32 = 10.0;
const NOTE_OVERHANG: f32 = 24.0;
const BLOCK_PAD: f32 = 12.0;
/// How far a block reaches past the lifelines of the participants inside it.
const SEQ_BLOCK_REACH: f32 = 16.0;

/// Diagram family, shown in the block header (`mermaid · flowchart`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Flowchart,
    Sequence,
}

/// A parsed diagram, ready to lay out at any font size.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Diagram {
    pub kind: Kind,
    body: Body,
}

/// Which font a label is painted (and must be measured) with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Font {
    Node,
    Edge,
}

/// Paint-ready primitives in logical pixels, origin at the top-left.
///
/// Shapes are listed back to front: clusters and blocks come before the nodes
/// they contain.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Scene {
    pub width: f32,
    pub height: f32,
    pub shapes: Vec<Shape>,
    pub lines: Vec<Line>,
    pub labels: Vec<Label>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Shape {
    pub kind: ShapeKind,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub tone: Tone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShapeKind {
    Rect,
    Rounded,
    Stadium,
    Diamond,
    Circle,
    Cylinder,
    Hexagon,
    Subroutine,
    Note,
    /// A thin dashed vertical line at `x`; `width` is zero.
    Lifeline,
    Activation,
    /// Subgraph or sequence block background; its title is a separate label.
    Cluster,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Tone {
    #[default]
    Normal,
    /// Nodes the author styled (`style`, `class`, `:::`) and sequence actors.
    Accent,
    /// Clusters, blocks, and notes.
    Muted,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Line {
    pub points: Vec<(f32, f32)>,
    pub style: Stroke,
    pub arrow_start: Arrow,
    pub arrow_end: Arrow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stroke {
    Solid,
    Dotted,
    Thick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arrow {
    None,
    Filled,
    /// Sequence-diagram async arrow: two strokes, no fill.
    Open,
    Cross,
    Circle,
}

/// Text centered on (`x`, `y`); `\n` separates centered lines.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Label {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub font: Font,
    /// Edge labels sit on their line and need a background pill.
    pub backdrop: bool,
}

/// Parses a mermaid fence body. `None` when the source is not a diagram kind we
/// draw, is empty, or exceeds the layout caps.
pub(crate) fn parse(source: &str) -> Option<Diagram> {
    if source.len() > MAX_SOURCE_BYTES {
        return None;
    }
    let mut lines = source.lines();
    let header = loop {
        let line = lines.next()?.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if line == "---" {
            // YAML front matter (title, config) carries nothing we draw.
            for inner in lines.by_ref() {
                if inner.trim() == "---" {
                    break;
                }
            }
            continue;
        }
        break line;
    };
    let (head, inline) = header.split_once(';').unwrap_or((header, ""));
    let (keyword, rest) = split_word(head);
    match keyword {
        "graph" | "flowchart" | "flowchart-elk" => {
            let direction = match split_word(rest).0.to_ascii_uppercase().as_str() {
                "LR" => Direction::Right,
                "RL" => Direction::Left,
                "BT" => Direction::Up,
                _ => Direction::Down,
            };
            let flow = parse_flowchart(direction, std::iter::once(inline).chain(lines))?;
            Some(Diagram {
                kind: Kind::Flowchart,
                body: Body::Flow(flow),
            })
        }
        "sequenceDiagram" => {
            let sequence = parse_sequence(std::iter::once(inline).chain(lines))?;
            Some(Diagram {
                kind: Kind::Sequence,
                body: Body::Sequence(sequence),
            })
        }
        _ => None,
    }
}

impl Diagram {
    /// Lays the diagram out with `measure` returning the (width, height) of one
    /// line of text in the given font.
    pub(crate) fn layout(&self, measure: impl Fn(&str, Font) -> (f32, f32)) -> Scene {
        match &self.body {
            Body::Flow(flow) => layout_flowchart(flow, &measure),
            Body::Sequence(sequence) => layout_sequence(sequence, &measure),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Body {
    Flow(Flowchart),
    Sequence(Sequence),
}

// ---------------------------------------------------------------------------
// Shared text helpers
// ---------------------------------------------------------------------------

/// Splits off the first whitespace-delimited word.
fn split_word(text: &str) -> (&str, &str) {
    let text = text.trim();
    match text.find(char::is_whitespace) {
        Some(index) => (&text[..index], text[index..].trim()),
        None => (text, ""),
    }
}

/// Splits a line on `;` outside quotes and brackets.
fn split_statements(line: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut quoted = false;
    let mut depth = 0usize;
    let mut start = 0;
    for (index, ch) in line.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            '[' | '(' | '{' if !quoted => depth += 1,
            ']' | ')' | '}' if !quoted => depth = depth.saturating_sub(1),
            ';' if !quoted && depth == 0 => {
                parts.push(&line[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&line[start..]);
    parts
}

/// Normalizes label text: strips quotes and markdown-string markers, turns
/// `<br>` and `\n` into line breaks, drops simple inline HTML tags and icon
/// tokens, and decodes the entity forms mermaid documents.
fn clean_label(raw: &str) -> String {
    let mut text = raw.trim();
    if let Some(inner) = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        text = inner.trim();
    }
    let markdown_inner = text.strip_prefix('`').and_then(|t| t.strip_suffix('`'));
    let markdown = markdown_inner.is_some();
    let text = markdown_inner.unwrap_or(text);

    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while let Some(&ch) = chars.get(index) {
        let rest = &chars[index..];
        if ch == '<'
            && let Some((len, is_break)) = html_tag(rest)
        {
            if is_break {
                out.push('\n');
            }
            index += len;
            continue;
        }
        if (ch == '#' || ch == '&')
            && let Some((decoded, len)) = entity(rest)
        {
            out.push(decoded);
            index += len;
            continue;
        }
        if ch == '\\' && rest.get(1) == Some(&'n') {
            out.push('\n');
            index += 2;
            continue;
        }
        if markdown && ch == '*' {
            index += 1;
            continue;
        }
        out.push(ch);
        index += 1;
    }

    let lines: Vec<String> = out
        .split('\n')
        .map(|line| {
            line.split_whitespace()
                .filter(|word| !is_icon_token(word))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    let first = lines.iter().position(|l| !l.is_empty());
    let last = lines.iter().rposition(|l| !l.is_empty());
    let joined = match (first, last) {
        (Some(first), Some(last)) => lines[first..=last].join("\n"),
        _ => String::new(),
    };
    joined.chars().take(MAX_LABEL_CHARS).collect()
}

fn is_icon_token(word: &str) -> bool {
    ["fa:", "fab:", "fas:", "far:"]
        .iter()
        .any(|prefix| word.starts_with(prefix))
}

/// Recognizes a small allow-list of inline tags; returns (length, is `<br>`).
fn html_tag(chars: &[char]) -> Option<(usize, bool)> {
    const TAGS: [&str; 16] = [
        "br", "b", "i", "u", "em", "strong", "small", "code", "sub", "sup", "span", "p", "div",
        "s", "strike", "mark",
    ];
    let mut index = 1;
    if chars.get(index) == Some(&'/') {
        index += 1;
    }
    let name_start = index;
    while chars.get(index).is_some_and(char::is_ascii_alphabetic) {
        index += 1;
    }
    let name: String = chars
        .get(name_start..index)?
        .iter()
        .map(char::to_ascii_lowercase)
        .collect();
    if !TAGS.contains(&name.as_str()) {
        return None;
    }
    // Attributes are skipped up to the closing bracket, within a bound.
    for (offset, &ch) in chars.iter().enumerate().skip(index).take(200) {
        match ch {
            '>' => return Some((offset + 1, name == "br")),
            '<' => return None,
            _ => {}
        }
    }
    None
}

/// Decodes `#quot;`, `&amp;`, `#35;`, `&#x27;` and friends; returns (char, length).
fn entity(chars: &[char]) -> Option<(char, usize)> {
    let end = chars.iter().take(12).position(|&c| c == ';')?;
    let body: String = chars.get(1..end)?.iter().collect();
    if body.is_empty() {
        return None;
    }
    let numeric = body.strip_prefix('#').unwrap_or(&body);
    let decoded = if let Some(hex) = numeric
        .strip_prefix('x')
        .or_else(|| numeric.strip_prefix('X'))
        .filter(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()))
    {
        char::from_u32(u32::from_str_radix(hex, 16).ok()?)?
    } else if numeric.chars().all(|c| c.is_ascii_digit()) {
        char::from_u32(numeric.parse().ok()?)?
    } else {
        match body.as_str() {
            "quot" => '"',
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "apos" => '\'',
            "nbsp" => ' ',
            _ => return None,
        }
    };
    if decoded.is_control() && decoded != '\n' {
        return None;
    }
    Some((decoded, end + 1))
}

fn sane(value: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value.min(MAX_EXTENT)
    } else {
        0.0
    }
}

/// Size of possibly multi-line text: widest line by summed line heights.
fn text_size(measure: &impl Fn(&str, Font) -> (f32, f32), text: &str, font: Font) -> (f32, f32) {
    if text.is_empty() {
        return (0.0, 0.0);
    }
    text.split('\n')
        .fold((0.0f32, 0.0f32), |(width, height), line| {
            let (w, h) = measure(if line.is_empty() { " " } else { line }, font);
            (width.max(sane(w)), height + sane(h))
        })
}

/// Collects primitives and normalizes them into a margin-framed scene.
#[derive(Default)]
struct SceneBuilder {
    shapes: Vec<Shape>,
    lines: Vec<Line>,
    labels: Vec<Label>,
    /// Parallel to `labels`: measured extents, used only for bounds.
    label_sizes: Vec<(f32, f32)>,
}

impl SceneBuilder {
    fn shape(&mut self, kind: ShapeKind, bounds: Bounds, tone: Tone) {
        self.shapes.push(Shape {
            kind,
            x: bounds.x0,
            y: bounds.y0,
            width: (bounds.x1 - bounds.x0).max(0.0),
            height: (bounds.y1 - bounds.y0).max(0.0),
            tone,
        });
    }

    fn line(
        &mut self,
        points: Vec<(f32, f32)>,
        style: Stroke,
        arrow_start: Arrow,
        arrow_end: Arrow,
    ) {
        if points.len() < 2 {
            return;
        }
        self.lines.push(Line {
            points,
            style,
            arrow_start,
            arrow_end,
        });
    }

    fn label(
        &mut self,
        text: &str,
        center: (f32, f32),
        size: (f32, f32),
        font: Font,
        backdrop: bool,
    ) {
        if text.is_empty() {
            return;
        }
        self.labels.push(Label {
            text: text.to_owned(),
            x: center.0,
            y: center.1,
            font,
            backdrop,
        });
        self.label_sizes.push(size);
    }

    /// Appends `front` so its primitives paint over this builder's.
    fn extend(&mut self, front: SceneBuilder) {
        self.shapes.extend(front.shapes);
        self.lines.extend(front.lines);
        self.labels.extend(front.labels);
        self.label_sizes.extend(front.label_sizes);
    }

    fn finish(mut self) -> Scene {
        let mut bounds: Option<Bounds> = None;
        let mut include = |b: Bounds| {
            bounds = Some(match bounds {
                Some(current) => current.union(b),
                None => b,
            });
        };
        for shape in &self.shapes {
            include(Bounds::new(
                shape.x,
                shape.y,
                shape.x + shape.width,
                shape.y + shape.height,
            ));
        }
        for line in &self.lines {
            for &(x, y) in &line.points {
                include(Bounds::new(x, y, x, y));
            }
        }
        for (label, &(w, h)) in self.labels.iter().zip(&self.label_sizes) {
            include(Bounds::centered((label.x, label.y), (w, h)));
        }
        let Some(bounds) = bounds else {
            return Scene {
                width: 2.0 * MARGIN,
                height: 2.0 * MARGIN,
                ..Scene::default()
            };
        };
        let (dx, dy) = (MARGIN - bounds.x0, MARGIN - bounds.y0);
        for shape in &mut self.shapes {
            shape.x += dx;
            shape.y += dy;
        }
        for line in &mut self.lines {
            for point in &mut line.points {
                point.0 += dx;
                point.1 += dy;
            }
        }
        for label in &mut self.labels {
            label.x += dx;
            label.y += dy;
        }
        Scene {
            width: bounds.x1 - bounds.x0 + 2.0 * MARGIN,
            height: bounds.y1 - bounds.y0 + 2.0 * MARGIN,
            shapes: self.shapes,
            lines: self.lines,
            labels: self.labels,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Bounds {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl Bounds {
    fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Self {
        Self { x0, y0, x1, y1 }
    }

    fn centered((cx, cy): (f32, f32), (w, h): (f32, f32)) -> Self {
        Self::new(cx - w / 2.0, cy - h / 2.0, cx + w / 2.0, cy + h / 2.0)
    }

    fn union(self, other: Self) -> Self {
        Self::new(
            self.x0.min(other.x0),
            self.y0.min(other.y0),
            self.x1.max(other.x1),
            self.y1.max(other.y1),
        )
    }

    fn grow(self, by: f32) -> Self {
        Self::new(self.x0 - by, self.y0 - by, self.x1 + by, self.y1 + by)
    }
}

// ---------------------------------------------------------------------------
// Flowchart model and parser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Down,
    Up,
    Right,
    Left,
}

#[derive(Debug, Clone, PartialEq)]
struct Flowchart {
    direction: Direction,
    nodes: Vec<FlowNode>,
    edges: Vec<FlowEdge>,
    clusters: Vec<Cluster>,
}

#[derive(Debug, Clone, PartialEq)]
struct FlowNode {
    id: String,
    label: String,
    shape: ShapeKind,
    /// Innermost subgraph containing the node.
    cluster: Option<usize>,
    tone: Tone,
}

#[derive(Debug, Clone, PartialEq)]
struct FlowEdge {
    from: usize,
    to: usize,
    label: String,
    stroke: Stroke,
    arrow_start: Arrow,
    arrow_end: Arrow,
    /// `~~~` links shape the layout but are not drawn.
    visible: bool,
}

/// A subgraph. Parents always precede their children in `Flowchart::clusters`.
#[derive(Debug, Clone, PartialEq)]
struct Cluster {
    id: String,
    title: String,
    parent: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
struct LinkSpec {
    stroke: Stroke,
    arrow_start: Arrow,
    arrow_end: Arrow,
    label: String,
    visible: bool,
}

/// A char cursor, so slicing can never split a UTF-8 sequence.
struct Cursor {
    chars: Vec<char>,
    pos: usize,
}

impl Cursor {
    fn new(text: &str) -> Self {
        Self {
            chars: text.chars().collect(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn bump(&mut self) {
        if self.pos < self.chars.len() {
            self.pos += 1;
        }
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.pos += 1;
        }
    }

    fn matches_at(&self, at: usize, token: &str) -> bool {
        token
            .chars()
            .enumerate()
            .all(|(k, c)| self.chars.get(at + k) == Some(&c))
    }

    fn eat(&mut self, token: &str) -> bool {
        if !self.matches_at(self.pos, token) {
            return false;
        }
        self.pos += token.chars().count();
        true
    }

    fn find(&self, token: &str) -> Option<usize> {
        (self.pos..self.chars.len()).find(|&at| self.matches_at(at, token))
    }

    fn slice(&self, start: usize, end: usize) -> String {
        self.chars
            .get(start..end.min(self.chars.len()))
            .map(|s| s.iter().collect())
            .unwrap_or_default()
    }

    /// Consumes through `close` (or to the end) and returns the text before it.
    fn take_until(&mut self, close: &str) -> String {
        let start = self.pos;
        match self.find(close) {
            Some(at) => {
                self.pos = at + close.chars().count();
                self.slice(start, at)
            }
            None => {
                self.pos = self.chars.len();
                self.slice(start, self.chars.len())
            }
        }
    }
}

fn is_id_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

struct FlowParser {
    flow: Flowchart,
    ids: HashMap<String, usize>,
    /// Whether a node was ever given an explicit shape or label.
    explicit: Vec<bool>,
    open: Vec<usize>,
    accents: Vec<String>,
    overflow: bool,
}

fn parse_flowchart<'a>(
    direction: Direction,
    lines: impl Iterator<Item = &'a str>,
) -> Option<Flowchart> {
    let mut parser = FlowParser {
        flow: Flowchart {
            direction,
            nodes: Vec::new(),
            edges: Vec::new(),
            clusters: Vec::new(),
        },
        ids: HashMap::new(),
        explicit: Vec::new(),
        open: Vec::new(),
        accents: Vec::new(),
        overflow: false,
    };
    for line in lines {
        for statement in split_statements(line) {
            parser.statement(statement);
            if parser.overflow {
                return None;
            }
        }
    }
    parser.finish()
}

impl FlowParser {
    fn statement(&mut self, statement: &str) {
        let statement = statement.trim();
        if statement.is_empty() || statement.starts_with("%%") {
            return;
        }
        let (word, rest) = split_word(statement);
        match word {
            "subgraph" => self.open_cluster(rest),
            "end" => {
                self.open.pop();
            }
            "classDef" | "linkStyle" | "click" | "direction" | "accTitle" | "accTitle:"
            | "accDescr" | "accDescr:" | "title" => {}
            "style" => {
                if rest.contains("fill") {
                    self.accents.push(split_word(rest).0.to_owned());
                }
            }
            "class" => {
                let ids = split_word(rest).0;
                self.accents
                    .extend(ids.split(',').map(|id| id.trim().to_owned()));
            }
            _ => self.chain(statement),
        }
    }

    fn open_cluster(&mut self, rest: &str) {
        if self.flow.clusters.len() >= MAX_CLUSTERS || self.open.len() >= MAX_NESTING {
            self.overflow = true;
            return;
        }
        let rest = rest.trim();
        let (id, title) = if let Some(bracket) = rest.find('[') {
            let inner = rest[bracket + 1..].trim_end();
            let inner = inner.strip_suffix(']').unwrap_or(inner);
            (rest[..bracket].trim().to_owned(), clean_label(inner))
        } else {
            (rest.to_owned(), clean_label(rest))
        };
        let id = if id.is_empty() {
            format!("subgraph-{}", self.flow.clusters.len())
        } else {
            id.trim_matches('"').to_owned()
        };
        self.flow.clusters.push(Cluster {
            id,
            title,
            parent: self.open.last().copied(),
        });
        self.open.push(self.flow.clusters.len() - 1);
    }

    fn chain(&mut self, statement: &str) {
        let mut cursor = Cursor::new(statement);
        let mut previous = self.node_group(&mut cursor);
        if previous.is_empty() {
            return;
        }
        loop {
            cursor.skip_ws();
            let Some(link) = parse_link(&mut cursor) else {
                return;
            };
            let next = self.node_group(&mut cursor);
            if next.is_empty() {
                return;
            }
            for &from in &previous {
                for &to in &next {
                    if self.flow.edges.len() >= MAX_EDGES {
                        self.overflow = true;
                        return;
                    }
                    self.flow.edges.push(FlowEdge {
                        from,
                        to,
                        label: link.label.clone(),
                        stroke: link.stroke,
                        arrow_start: link.arrow_start,
                        arrow_end: link.arrow_end,
                        visible: link.visible,
                    });
                }
            }
            previous = next;
        }
    }

    /// `A & B & C`
    fn node_group(&mut self, cursor: &mut Cursor) -> Vec<usize> {
        let mut group = Vec::new();
        loop {
            let Some(node) = self.node(cursor) else {
                return group;
            };
            group.push(node);
            let save = cursor.pos;
            cursor.skip_ws();
            if !cursor.eat("&") {
                cursor.pos = save;
                return group;
            }
        }
    }

    fn node(&mut self, cursor: &mut Cursor) -> Option<usize> {
        cursor.skip_ws();
        let start = cursor.pos;
        while let Some(ch) = cursor.peek() {
            // A hyphen belongs to the id only between word characters, so
            // `my-node-->x` splits at the arrow.
            let keep = is_id_char(ch)
                || (ch == '-' && cursor.pos > start && cursor.peek_at(1).is_some_and(is_id_char));
            if !keep {
                break;
            }
            cursor.bump();
        }
        let id = cursor.slice(start, cursor.pos);
        if id.is_empty() {
            return None;
        }
        let spec = node_shape(cursor);
        let mut accent = false;
        if cursor.eat(":::") {
            accent = true;
            while cursor.peek().is_some_and(|c| is_id_char(c) || c == '-') {
                cursor.bump();
            }
        }
        let index = self.intern(id, spec)?;
        if accent {
            self.flow.nodes[index].tone = Tone::Accent;
        }
        Some(index)
    }

    fn intern(&mut self, id: String, spec: Option<(String, ShapeKind)>) -> Option<usize> {
        let cluster = self.open.last().copied();
        if let Some(&index) = self.ids.get(&id) {
            let node = &mut self.flow.nodes[index];
            if let Some((label, shape)) = spec {
                node.label = label;
                node.shape = shape;
                self.explicit[index] = true;
            }
            if node.cluster.is_none() {
                node.cluster = cluster;
            }
            return Some(index);
        }
        if self.flow.nodes.len() >= MAX_NODES {
            self.overflow = true;
            return None;
        }
        let explicit = spec.is_some();
        let (label, shape) = spec.unwrap_or_else(|| (clean_label(&id), ShapeKind::Rect));
        let index = self.flow.nodes.len();
        self.flow.nodes.push(FlowNode {
            id: id.clone(),
            label,
            shape,
            cluster,
            tone: Tone::Normal,
        });
        self.explicit.push(explicit);
        self.ids.insert(id, index);
        Some(index)
    }

    fn finish(mut self) -> Option<Flowchart> {
        for id in &self.accents {
            if let Some(&index) = self.ids.get(id) {
                self.flow.nodes[index].tone = Tone::Accent;
            }
        }
        self.redirect_cluster_references();
        (!self.flow.nodes.is_empty()).then_some(self.flow)
    }

    /// Edges may target a subgraph by id. Such a reference creates a bare node;
    /// replace it with the subgraph's first member so the edge lands inside.
    fn redirect_cluster_references(&mut self) {
        let flow = &mut self.flow;
        let mut replacement: Vec<Option<usize>> = vec![None; flow.nodes.len()];
        for (c, cluster) in flow.clusters.iter().enumerate() {
            let Some(&placeholder) = self.ids.get(&cluster.id) else {
                continue;
            };
            if self.explicit.get(placeholder).copied().unwrap_or(true) {
                continue;
            }
            let member = (0..flow.nodes.len()).find(|&m| {
                m != placeholder && cluster_path(&flow.clusters, flow.nodes[m].cluster).contains(&c)
            });
            if let Some(member) = member {
                replacement[placeholder] = Some(member);
            }
        }
        if replacement.iter().all(Option::is_none) {
            return;
        }
        let mut remap = vec![0usize; flow.nodes.len()];
        let mut kept = Vec::with_capacity(flow.nodes.len());
        for (index, node) in flow.nodes.drain(..).enumerate() {
            if replacement[index].is_none() {
                remap[index] = kept.len();
                kept.push(node);
            }
        }
        let resolve = |index: usize| remap[replacement[index].unwrap_or(index)];
        flow.nodes = kept;
        for edge in &mut flow.edges {
            edge.from = resolve(edge.from);
            edge.to = resolve(edge.to);
        }
        flow.edges
            .retain(|edge| edge.from != edge.to || edge.visible);
    }
}

/// Parses an optional shape suffix right after a node id.
fn node_shape(cursor: &mut Cursor) -> Option<(String, ShapeKind)> {
    const SHAPES: [(&str, &str, ShapeKind); 12] = [
        ("(((", ")))", ShapeKind::Circle),
        ("([", "])", ShapeKind::Stadium),
        ("((", "))", ShapeKind::Circle),
        ("[(", ")]", ShapeKind::Cylinder),
        ("[[", "]]", ShapeKind::Subroutine),
        ("{{", "}}", ShapeKind::Hexagon),
        ("[/", "]", ShapeKind::Rect),
        ("[\\", "]", ShapeKind::Rect),
        ("[", "]", ShapeKind::Rect),
        ("(", ")", ShapeKind::Rounded),
        ("{", "}", ShapeKind::Diamond),
        (">", "]", ShapeKind::Rect),
    ];
    if cursor.eat("@{") {
        let body = cursor.take_until("}");
        return Some(shape_from_attributes(&body));
    }
    let &(open, close, shape) = SHAPES
        .iter()
        .find(|(open, _, _)| cursor.matches_at(cursor.pos, open))?;
    cursor.eat(open);
    let save = cursor.pos;
    cursor.skip_ws();
    let raw = if cursor.peek() == Some('"') {
        cursor.bump();
        let quoted = cursor.take_until("\"");
        cursor.take_until(close);
        quoted
    } else {
        cursor.pos = save;
        cursor.take_until(close)
    };
    // Parallelogram and trapezoid markers are not part of the text.
    let raw = raw.trim().trim_end_matches(['/', '\\']);
    Some((clean_label(raw), shape))
}

/// `A@{ shape: diam, label: "Ready?" }`
fn shape_from_attributes(body: &str) -> (String, ShapeKind) {
    let value = |key: &str| -> Option<String> {
        let start = body.find(key)? + key.len();
        let rest = body[start..].trim_start();
        let rest = rest.strip_prefix(':')?.trim_start();
        Some(match rest.strip_prefix('"') {
            Some(quoted) => quoted.split('"').next().unwrap_or_default().to_owned(),
            None => rest.split(',').next().unwrap_or_default().trim().to_owned(),
        })
    };
    let shape = match value("shape").as_deref() {
        Some("rounded" | "event") => ShapeKind::Rounded,
        Some("stadium" | "pill" | "terminal") => ShapeKind::Stadium,
        Some("diam" | "diamond" | "decision" | "question") => ShapeKind::Diamond,
        Some("circle" | "circ" | "dbl-circ" | "double-circle" | "sm-circ" | "start" | "stop") => {
            ShapeKind::Circle
        }
        Some("cyl" | "cylinder" | "database" | "db" | "h-cyl" | "das") => ShapeKind::Cylinder,
        Some("hex" | "hexagon" | "prepare") => ShapeKind::Hexagon,
        Some("subproc" | "subprocess" | "subroutine" | "fr-rect") => ShapeKind::Subroutine,
        _ => ShapeKind::Rect,
    };
    (clean_label(&value("label").unwrap_or_default()), shape)
}

fn stroke_of(body: &str) -> Stroke {
    if body.contains('=') {
        Stroke::Thick
    } else if body.contains('.') {
        Stroke::Dotted
    } else {
        Stroke::Solid
    }
}

/// An arrowhead after a link body. `o`/`x` count only when not starting a word.
fn link_head(cursor: &mut Cursor) -> Arrow {
    let arrow = match cursor.peek() {
        Some('>') => Arrow::Filled,
        Some('o') if !cursor.peek_at(1).is_some_and(is_id_char) => Arrow::Circle,
        Some('x') if !cursor.peek_at(1).is_some_and(is_id_char) => Arrow::Cross,
        _ => return Arrow::None,
    };
    cursor.bump();
    arrow
}

fn link_body(cursor: &mut Cursor) -> String {
    let start = cursor.pos;
    while matches!(cursor.peek(), Some('-' | '=' | '.')) {
        cursor.bump();
    }
    cursor.slice(start, cursor.pos)
}

/// Parses `-->`, `-.->`, `==>`, `---`, `<-->`, `--o`, `~~~`, the
/// `-- text -->` forms, and a trailing `|label|`.
fn parse_link(cursor: &mut Cursor) -> Option<LinkSpec> {
    let save = cursor.pos;
    if cursor.matches_at(cursor.pos, "~~~") {
        while cursor.peek() == Some('~') {
            cursor.bump();
        }
        return Some(LinkSpec {
            stroke: Stroke::Solid,
            arrow_start: Arrow::None,
            arrow_end: Arrow::None,
            label: pipe_label(cursor).unwrap_or_default(),
            visible: false,
        });
    }
    let arrow_start = match (cursor.peek(), cursor.peek_at(1)) {
        (Some('<'), Some('-' | '=' | '.')) => Arrow::Filled,
        (Some('o'), Some('-' | '=' | '.')) => Arrow::Circle,
        (Some('x'), Some('-' | '=' | '.')) => Arrow::Cross,
        _ => Arrow::None,
    };
    if arrow_start != Arrow::None {
        cursor.bump();
    }
    let body = link_body(cursor);
    if body.chars().count() < 2 {
        cursor.pos = save;
        return None;
    }
    let stroke = stroke_of(&body);
    let mut arrow_end = link_head(cursor);
    let mut label = String::new();
    let text_form = arrow_end == Arrow::None
        && matches!(body.as_str(), "--" | "==" | "-.")
        && cursor.peek().is_some_and(char::is_whitespace);
    if text_form {
        let closer = if body == "-." { ".-" } else { body.as_str() };
        if let Some(at) = cursor.find(closer) {
            label = clean_label(&cursor.slice(cursor.pos, at));
            cursor.pos = at;
            link_body(cursor);
            arrow_end = link_head(cursor);
        }
    }
    if let Some(piped) = pipe_label(cursor) {
        label = piped;
    }
    Some(LinkSpec {
        stroke,
        arrow_start,
        arrow_end,
        label,
        visible: true,
    })
}

fn pipe_label(cursor: &mut Cursor) -> Option<String> {
    let save = cursor.pos;
    cursor.skip_ws();
    if cursor.peek() != Some('|') {
        cursor.pos = save;
        return None;
    }
    cursor.bump();
    Some(clean_label(&cursor.take_until("|")))
}

/// Cluster ids from outermost to `innermost`.
fn cluster_path(clusters: &[Cluster], innermost: Option<usize>) -> Vec<usize> {
    let mut path = Vec::new();
    let mut current = innermost;
    while let Some(cluster) = current {
        if path.len() > MAX_NESTING || path.contains(&cluster) {
            break;
        }
        path.push(cluster);
        current = clusters.get(cluster).and_then(|c| c.parent);
    }
    path.reverse();
    path
}

fn common_prefix(a: &[usize], b: &[usize]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

// ---------------------------------------------------------------------------
// Flowchart layout
// ---------------------------------------------------------------------------

/// A vertex of the layered graph. Real nodes come first, indexed as in
/// `Flowchart::nodes`; dummies (edge bends and label slots) follow. Sizes are
/// in layout space: `width` runs across the layer, `height` along the flow.
struct Vertex {
    width: f32,
    height: f32,
    /// Extra room on the +x side for self-loops.
    right_extra: f32,
    layer: usize,
    dummy: bool,
    clusters: Vec<usize>,
    seed: usize,
}

/// An edge's path through the layered graph, in layout (possibly reversed) order.
struct Route {
    vertices: Vec<usize>,
    reversed: bool,
    label_vertex: Option<usize>,
}

/// Which layout-space side of a cluster maps to the screen top, where its
/// title band goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TitleSide {
    MinY,
    MaxY,
    MinX,
}

struct ClusterStyle<'a> {
    clusters: &'a [Cluster],
    /// Title band thickness per cluster (zero for untitled clusters).
    bands: Vec<f32>,
    title_widths: Vec<f32>,
    side: TitleSide,
    direction: Direction,
}

type Neighbors = Vec<Vec<(usize, f32)>>;

fn node_size(shape: ShapeKind, (tw, th): (f32, f32)) -> (f32, f32) {
    let width = (tw + 2.0 * NODE_PAD_X).max(MIN_NODE_WIDTH);
    let height = th + 2.0 * NODE_PAD_Y;
    match shape {
        // Leaves the text box's corners inside |x|/a + |y|/b <= 1.
        ShapeKind::Diamond => ((tw * 1.6 + 24.0).max(48.0), (th * 2.2 + 12.0).max(40.0)),
        ShapeKind::Circle => {
            let diameter = (tw * tw + th * th).sqrt() + 16.0;
            let diameter = diameter.max(height).max(40.0);
            (diameter, diameter)
        }
        ShapeKind::Stadium => (tw + 2.0 * NODE_PAD_X + height * 0.5, height),
        ShapeKind::Hexagon => (tw + 2.0 * NODE_PAD_X + height * 0.5, height),
        ShapeKind::Cylinder => (width, height + CYLINDER_CAP),
        ShapeKind::Subroutine => (width + 16.0, height),
        _ => (width, height),
    }
}

fn layout_flowchart(flow: &Flowchart, measure: &impl Fn(&str, Font) -> (f32, f32)) -> Scene {
    let direction = flow.direction;
    let horizontal = matches!(direction, Direction::Right | Direction::Left);
    let across = |(w, h): (f32, f32)| if horizontal { (h, w) } else { (w, h) };
    let node_count = flow.nodes.len();

    let texts: Vec<(f32, f32)> = flow
        .nodes
        .iter()
        .map(|node| text_size(measure, &node.label, Font::Node))
        .collect();
    let sizes: Vec<(f32, f32)> = flow
        .nodes
        .iter()
        .zip(&texts)
        .map(|(node, &text)| node_size(node.shape, text))
        .collect();
    let edge_labels: Vec<(f32, f32)> = flow
        .edges
        .iter()
        .map(|edge| {
            if edge.label.is_empty() {
                return (0.0, 0.0);
            }
            let (w, h) = text_size(measure, &edge.label, Font::Edge);
            (w + 2.0 * LABEL_PAD_X, h + 2.0 * LABEL_PAD_Y)
        })
        .collect();
    let titles: Vec<(f32, f32)> = flow
        .clusters
        .iter()
        .map(|cluster| text_size(measure, &cluster.title, Font::Edge))
        .collect();
    let paths: Vec<Vec<usize>> = flow
        .nodes
        .iter()
        .map(|node| cluster_path(&flow.clusters, node.cluster))
        .collect();

    let mut loop_room = vec![0.0f32; node_count];
    let mut loop_count = vec![0usize; node_count];
    for (edge, &(lw, lh)) in flow.edges.iter().zip(&edge_labels) {
        if edge.from != edge.to || !edge.visible {
            continue;
        }
        let reach = SELF_LOOP_REACH + loop_count[edge.from] as f32 * SELF_LOOP_STEP;
        loop_count[edge.from] += 1;
        let room = reach + if horizontal { lh } else { lw } + 8.0;
        loop_room[edge.from] = loop_room[edge.from].max(room);
    }

    let reversed = break_cycles(flow);
    // Labeled or parallel edges get a middle layer of their own, so every
    // label is a sized vertex and never overlaps nodes or other labels.
    let doubled = needs_label_layers(flow);
    let min_span = if doubled { 2 } else { 1 };
    let layer_of = assign_layers(flow, &reversed, min_span);
    let seeds = preorder(flow, &reversed);

    let mut vertices: Vec<Vertex> = (0..node_count)
        .map(|i| {
            let (width, height) = across(sizes[i]);
            Vertex {
                width,
                height,
                right_extra: loop_room[i],
                layer: layer_of[i],
                dummy: false,
                clusters: paths[i].clone(),
                seed: seeds[i],
            }
        })
        .collect();
    let mut routes: Vec<Option<Route>> = Vec::with_capacity(flow.edges.len());
    let mut budget = MAX_DUMMIES;
    for (e, edge) in flow.edges.iter().enumerate() {
        if edge.from == edge.to {
            routes.push(None);
            continue;
        }
        let (u, v) = if reversed[e] {
            (edge.to, edge.from)
        } else {
            (edge.from, edge.to)
        };
        let span = layer_of[v].saturating_sub(layer_of[u]);
        let mut chain = vec![u];
        let mut label_vertex = None;
        if span >= 2 && span - 1 <= budget {
            budget -= span - 1;
            let shared = common_prefix(&paths[u], &paths[v]);
            // Label slots sit on odd offsets, between node layers.
            let half = span / 2;
            let label_step = if doubled && half.is_multiple_of(2) {
                half - 1
            } else {
                half
            };
            for step in 1..span {
                let is_label = !edge.label.is_empty() && step == label_step;
                let (width, height) = if is_label {
                    across(edge_labels[e])
                } else {
                    (0.0, 0.0)
                };
                vertices.push(Vertex {
                    width,
                    height,
                    right_extra: 0.0,
                    layer: layer_of[u] + step,
                    dummy: true,
                    clusters: paths[u][..shared].to_vec(),
                    seed: seeds[u],
                });
                let id = vertices.len() - 1;
                chain.push(id);
                if is_label {
                    label_vertex = Some(id);
                }
            }
        }
        chain.push(v);
        routes.push(Some(Route {
            vertices: chain,
            reversed: reversed[e],
            label_vertex,
        }));
    }

    let layer_count = vertices.iter().map(|v| v.layer + 1).max().unwrap_or(1);
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); layer_count];
    for (index, vertex) in vertices.iter().enumerate() {
        layers[vertex.layer].push(index);
    }
    for row in &mut layers {
        row.sort_by_key(|&v| (vertices[v].seed, v));
    }

    let mut up: Neighbors = vec![Vec::new(); vertices.len()];
    let mut down: Neighbors = vec![Vec::new(); vertices.len()];
    for route in routes.iter().flatten() {
        // Direct edges pull their endpoints into line; long edges pull only
        // weakly, so one skip-level edge cannot drag a whole chain diagonal.
        let long = route.vertices.len() > 3 || (!doubled && route.vertices.len() > 2);
        for pair in route.vertices.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if vertices[b].layer != vertices[a].layer + 1 {
                continue;
            }
            // Heavier dummy links keep long edges straight (Gansner's omega).
            let weight = match (vertices[a].dummy, vertices[b].dummy) {
                (true, true) => 4.0,
                _ if long => 0.35,
                _ => 2.0,
            };
            down[a].push((b, weight));
            up[b].push((a, weight));
        }
    }

    order_layers(&mut layers, &vertices, &up, &down);

    let side = match direction {
        Direction::Down => TitleSide::MinY,
        Direction::Up => TitleSide::MaxY,
        Direction::Right | Direction::Left => TitleSide::MinX,
    };
    let style = ClusterStyle {
        clusters: &flow.clusters,
        bands: titles
            .iter()
            .map(|&(_, th)| if th > 0.0 { th + TITLE_INSET_Y } else { 0.0 })
            .collect(),
        title_widths: titles.iter().map(|&(tw, _)| tw).collect(),
        side,
        direction,
    };

    let centers = layer_centers(&layers, &vertices, &style, doubled);
    let mut xs = assign_x(&layers, &vertices, &up, &down, &style);
    separate_clusters(&style, &layers, &vertices, &mut xs, &centers);
    let boxes = cluster_bounds(&style, &vertices, &xs, &centers);

    let screen = |x: f32, y: f32| -> (f32, f32) {
        match direction {
            Direction::Down => (x, y),
            Direction::Up => (x, -y),
            Direction::Right => (y, x),
            Direction::Left => (-y, x),
        }
    };
    let center_of = |v: usize| screen(xs[v], centers[vertices[v].layer]);

    let mut scene = SceneBuilder::default();
    for (c, bounds) in boxes.iter().enumerate() {
        let Some(b) = bounds else {
            continue;
        };
        let (ax, ay) = screen(b.x0, b.y0);
        let (bx, by) = screen(b.x1, b.y1);
        let rect = Bounds::new(ax.min(bx), ay.min(by), ax.max(bx), ay.max(by));
        scene.shape(ShapeKind::Cluster, rect, Tone::Muted);
        let (tw, th) = titles[c];
        scene.label(
            &flow.clusters[c].title,
            (
                rect.x0 + TITLE_INSET_X + tw / 2.0,
                rect.y0 + TITLE_INSET_Y + th / 2.0,
            ),
            (tw, th),
            Font::Edge,
            false,
        );
    }
    for (index, node) in flow.nodes.iter().enumerate() {
        let center = center_of(index);
        scene.shape(
            node.shape,
            Bounds::centered(center, sizes[index]),
            node.tone,
        );
        let offset = if node.shape == ShapeKind::Cylinder {
            CYLINDER_CAP / 4.0
        } else {
            0.0
        };
        scene.label(
            &node.label,
            (center.0, center.1 + offset),
            texts[index],
            Font::Node,
            false,
        );
    }

    let mut loops_drawn = vec![0usize; node_count];
    for (e, edge) in flow.edges.iter().enumerate() {
        if !edge.visible {
            continue;
        }
        if edge.from == edge.to {
            let k = loops_drawn[edge.from];
            loops_drawn[edge.from] += 1;
            let reach = SELF_LOOP_REACH + k as f32 * SELF_LOOP_STEP;
            let node = &flow.nodes[edge.from];
            let (points, label_at) = self_loop(
                node.shape,
                center_of(edge.from),
                sizes[edge.from],
                reach,
                edge_labels[e],
                horizontal,
            );
            scene.label(&edge.label, label_at, edge_labels[e], Font::Edge, true);
            scene.line(points, edge.stroke, edge.arrow_start, edge.arrow_end);
            continue;
        }
        let Some(route) = &routes[e] else {
            continue;
        };
        let mut points: Vec<(f32, f32)> = route.vertices.iter().map(|&v| center_of(v)).collect();
        if route.reversed {
            points.reverse();
        }
        let last = points.len() - 1;
        let (after_start, before_end) = (points[1], points[last - 1]);
        points[0] = clip_to_outline(
            flow.nodes[edge.from].shape,
            points[0],
            sizes[edge.from],
            after_start,
        );
        points[last] = clip_to_outline(
            flow.nodes[edge.to].shape,
            points[last],
            sizes[edge.to],
            before_end,
        );
        let curve = smooth(&points, !horizontal);
        if !edge.label.is_empty() {
            let at = route
                .label_vertex
                .map(center_of)
                .unwrap_or_else(|| midpoint(&curve));
            scene.label(&edge.label, at, edge_labels[e], Font::Edge, true);
        }
        scene.line(curve, edge.stroke, edge.arrow_start, edge.arrow_end);
    }
    scene.finish()
}

fn needs_label_layers(flow: &Flowchart) -> bool {
    let mut pairs: HashMap<(usize, usize), usize> = HashMap::new();
    for edge in flow.edges.iter().filter(|e| e.from != e.to) {
        if !edge.label.is_empty() {
            return true;
        }
        let key = (edge.from.min(edge.to), edge.from.max(edge.to));
        let count = pairs.entry(key).or_insert(0);
        *count += 1;
        if *count > 1 {
            return true;
        }
    }
    false
}

/// Edge indices leaving each node, ignoring self-loops.
fn outgoing(flow: &Flowchart) -> Vec<Vec<usize>> {
    let mut out = vec![Vec::new(); flow.nodes.len()];
    for (index, edge) in flow.edges.iter().enumerate() {
        if edge.from != edge.to {
            out[edge.from].push(index);
        }
    }
    out
}

/// Nodes without incoming edges first (declaration order), then the rest, so
/// DFS starts where the author's flow starts.
fn dfs_roots(flow: &Flowchart) -> Vec<usize> {
    let mut has_incoming = vec![false; flow.nodes.len()];
    for edge in flow.edges.iter().filter(|e| e.from != e.to) {
        has_incoming[edge.to] = true;
    }
    let mut roots: Vec<usize> = (0..flow.nodes.len())
        .filter(|&v| !has_incoming[v])
        .collect();
    roots.extend(0..flow.nodes.len());
    roots
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visit {
    New,
    Active,
    Done,
}

/// Marks DFS back edges; reversing them leaves the graph acyclic.
fn break_cycles(flow: &Flowchart) -> Vec<bool> {
    let out = outgoing(flow);
    let mut state = vec![Visit::New; flow.nodes.len()];
    let mut reversed = vec![false; flow.edges.len()];
    for root in dfs_roots(flow) {
        if state[root] != Visit::New {
            continue;
        }
        state[root] = Visit::Active;
        let mut stack = vec![(root, 0usize)];
        while let Some(top) = stack.last_mut() {
            let (node, next) = *top;
            let Some(&edge) = out[node].get(next) else {
                state[node] = Visit::Done;
                stack.pop();
                continue;
            };
            top.1 += 1;
            let target = flow.edges[edge].to;
            match state[target] {
                Visit::New => {
                    state[target] = Visit::Active;
                    stack.push((target, 0));
                }
                Visit::Active => reversed[edge] = true,
                Visit::Done => {}
            }
        }
    }
    reversed
}

/// Successors after cycle breaking.
fn oriented(flow: &Flowchart, reversed: &[bool]) -> Vec<Vec<usize>> {
    let mut succ = vec![Vec::new(); flow.nodes.len()];
    for (index, edge) in flow.edges.iter().enumerate() {
        if edge.from == edge.to {
            continue;
        }
        if reversed[index] {
            succ[edge.to].push(edge.from);
        } else {
            succ[edge.from].push(edge.to);
        }
    }
    succ
}

/// DFS preorder over the acyclic orientation, used to seed layer ordering.
fn preorder(flow: &Flowchart, reversed: &[bool]) -> Vec<usize> {
    let succ = oriented(flow, reversed);
    let mut seed = vec![usize::MAX; flow.nodes.len()];
    let mut counter = 0;
    for root in dfs_roots(flow) {
        if seed[root] != usize::MAX {
            continue;
        }
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if seed[node] != usize::MAX {
                continue;
            }
            seed[node] = counter;
            counter += 1;
            stack.extend(succ[node].iter().rev().filter(|&&t| seed[t] == usize::MAX));
        }
    }
    seed
}

/// Longest-path layering, then sources are pulled down next to their nearest
/// successor so side inputs do not float at the top.
fn assign_layers(flow: &Flowchart, reversed: &[bool], min_span: usize) -> Vec<usize> {
    let succ = oriented(flow, reversed);
    let count = flow.nodes.len();
    let mut indegree = vec![0usize; count];
    for targets in &succ {
        for &t in targets {
            indegree[t] += 1;
        }
    }
    let is_source: Vec<bool> = indegree.iter().map(|&d| d == 0).collect();
    let mut queue: Vec<usize> = (0..count).filter(|&v| indegree[v] == 0).collect();
    let mut layer = vec![0usize; count];
    let mut head = 0;
    while let Some(&node) = queue.get(head) {
        head += 1;
        for &t in &succ[node] {
            layer[t] = layer[t].max(layer[node] + min_span);
            indegree[t] -= 1;
            if indegree[t] == 0 {
                queue.push(t);
            }
        }
    }
    for &node in queue.iter().rev() {
        if !is_source[node] {
            continue;
        }
        if let Some(nearest) = succ[node].iter().map(|&t| layer[t]).min() {
            layer[node] = layer[node].max(nearest.saturating_sub(min_span));
        }
    }
    layer
}

fn refresh_positions(layers: &[Vec<usize>], pos: &mut [usize]) {
    for row in layers {
        for (index, &v) in row.iter().enumerate() {
            pos[v] = index;
        }
    }
}

/// Barycenter sweeps plus adjacent transposition, keeping the best ordering
/// seen. Cluster members stay contiguous in every layer.
fn order_layers(
    layers: &mut Vec<Vec<usize>>,
    vertices: &[Vertex],
    up: &Neighbors,
    down: &Neighbors,
) {
    let mut pos = vec![0usize; vertices.len()];
    refresh_positions(layers, &mut pos);
    let mut best = layers.clone();
    let mut best_cost = crossings(layers, down, &pos);
    let mut bary = vec![0.0f32; vertices.len()];
    for sweep in 0..ORDER_SWEEPS {
        if best_cost == 0 {
            break;
        }
        let downward = sweep % 2 == 0;
        let order: Vec<usize> = if downward {
            (1..layers.len()).collect()
        } else {
            (0..layers.len().saturating_sub(1)).rev().collect()
        };
        let fixed = if downward { up } else { down };
        for l in order {
            for &v in &layers[l] {
                let neighbors = &fixed[v];
                bary[v] = if neighbors.is_empty() {
                    pos[v] as f32
                } else {
                    neighbors.iter().map(|&(n, _)| pos[n] as f32).sum::<f32>()
                        / neighbors.len() as f32
                };
            }
            layers[l] = group_order(&layers[l], &bary, vertices, 0);
            for (index, &v) in layers[l].iter().enumerate() {
                pos[v] = index;
            }
        }
        transpose(layers, &mut pos, vertices, up, down);
        let cost = crossings(layers, down, &pos);
        if cost < best_cost {
            best_cost = cost;
            best.clone_from(layers);
        }
    }
    *layers = best;
}

/// Sorts a layer by barycenter while keeping each cluster's members together:
/// a cluster sorts as one item keyed by its members' mean, then recurses.
fn group_order(row: &[usize], bary: &[f32], vertices: &[Vertex], depth: usize) -> Vec<usize> {
    // (key, members, is a cluster group)
    let mut entries: Vec<(f32, Vec<usize>, bool)> = Vec::new();
    let mut groups: HashMap<usize, usize> = HashMap::new();
    for &v in row {
        match vertices[v].clusters.get(depth) {
            Some(&cluster) if depth < MAX_NESTING => match groups.get(&cluster) {
                Some(&entry) => entries[entry].1.push(v),
                None => {
                    groups.insert(cluster, entries.len());
                    entries.push((0.0, vec![v], true));
                }
            },
            _ => entries.push((bary[v], vec![v], false)),
        }
    }
    for entry in &mut entries {
        if entry.2 {
            entry.0 = entry.1.iter().map(|&v| bary[v]).sum::<f32>() / entry.1.len() as f32;
        }
    }
    // Stable, so ties keep the previous order.
    entries.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut ordered = Vec::with_capacity(row.len());
    for (_, members, is_group) in entries {
        if is_group {
            ordered.extend(group_order(&members, bary, vertices, depth + 1));
        } else {
            ordered.extend(members);
        }
    }
    ordered
}

fn pair_crossings(u: usize, v: usize, pos: &[usize], up: &Neighbors, down: &Neighbors) -> usize {
    let count = |adj: &Neighbors| {
        adj[u]
            .iter()
            .map(|&(a, _)| adj[v].iter().filter(|&&(b, _)| pos[a] > pos[b]).count())
            .sum::<usize>()
    };
    count(up) + count(down)
}

/// Swaps neighbors within the same cluster while that removes crossings.
fn transpose(
    layers: &mut [Vec<usize>],
    pos: &mut [usize],
    vertices: &[Vertex],
    up: &Neighbors,
    down: &Neighbors,
) {
    for _ in 0..TRANSPOSE_PASSES {
        let mut improved = false;
        for row in layers.iter_mut() {
            for i in 0..row.len().saturating_sub(1) {
                let (u, v) = (row[i], row[i + 1]);
                if vertices[u].clusters != vertices[v].clusters {
                    continue;
                }
                if pair_crossings(v, u, pos, up, down) < pair_crossings(u, v, pos, up, down) {
                    row.swap(i, i + 1);
                    pos[u] = i + 1;
                    pos[v] = i;
                    improved = true;
                }
            }
        }
        if !improved {
            break;
        }
    }
}

/// Counts crossings between adjacent layers via inversions (Fenwick tree).
fn crossings(layers: &[Vec<usize>], down: &Neighbors, pos: &[usize]) -> usize {
    let mut total = 0;
    for pair in layers.windows(2) {
        let mut segments: Vec<(usize, usize)> = pair[0]
            .iter()
            .flat_map(|&a| down[a].iter().map(move |&(b, _)| (pos[a], pos[b])))
            .collect();
        segments.sort_unstable();
        let mut tree = vec![0usize; pair[1].len() + 1];
        for (seen, &(_, b)) in segments.iter().enumerate() {
            let mut not_greater = 0;
            let mut i = (b + 1).min(tree.len() - 1);
            while i > 0 {
                not_greater += tree[i];
                i -= i & i.wrapping_neg();
            }
            total += seen - not_greater;
            let mut i = b + 1;
            while i < tree.len() {
                tree[i] += 1;
                i += i & i.wrapping_neg();
            }
        }
    }
    total
}

/// Layer centers along the flow axis. Gaps grow where clusters open or close
/// so nested padding and title bands never collide with the neighbor layer.
fn layer_centers(
    layers: &[Vec<usize>],
    vertices: &[Vertex],
    style: &ClusterStyle<'_>,
    doubled: bool,
) -> Vec<f32> {
    let count = layers.len();
    let thickness: Vec<f32> = layers
        .iter()
        .map(|row| row.iter().map(|&v| vertices[v].height).fold(0.0, f32::max))
        .collect();
    let clusters = style.clusters.len();
    let mut range: Vec<Option<(usize, usize)>> = vec![None; clusters];
    for vertex in vertices.iter().filter(|v| !v.dummy) {
        for &c in &vertex.clusters {
            range[c] = Some(match range[c] {
                Some((lo, hi)) => (lo.min(vertex.layer), hi.max(vertex.layer)),
                None => (vertex.layer, vertex.layer),
            });
        }
    }
    let mut top_need = vec![0.0f32; clusters];
    let mut bottom_need = vec![0.0f32; clusters];
    let mut child_top = vec![0.0f32; clusters];
    let mut child_bottom = vec![0.0f32; clusters];
    // Children have larger indices than parents, so they finish first.
    for c in (0..clusters).rev() {
        let Some((lo, hi)) = range[c] else {
            continue;
        };
        let band = style.bands[c];
        top_need[c] = CLUSTER_PAD
            + if style.side == TitleSide::MinY {
                band
            } else {
                0.0
            }
            + child_top[c];
        bottom_need[c] = CLUSTER_PAD
            + if style.side == TitleSide::MaxY {
                band
            } else {
                0.0
            }
            + child_bottom[c];
        let Some(parent) = style.clusters[c].parent else {
            continue;
        };
        if let Some((parent_lo, parent_hi)) = range.get(parent).copied().flatten() {
            if parent_lo == lo {
                child_top[parent] = child_top[parent].max(top_need[c]);
            }
            if parent_hi == hi {
                child_bottom[parent] = child_bottom[parent].max(bottom_need[c]);
            }
        }
    }
    let mut before = vec![0.0f32; count];
    let mut after = vec![0.0f32; count];
    for c in 0..clusters {
        if let Some((lo, hi)) = range[c] {
            before[lo] = before[lo].max(top_need[c]);
            after[hi] = after[hi].max(bottom_need[c]);
        }
    }
    let gap = if doubled { LAYER_GAP / 2.0 } else { LAYER_GAP };
    let mut centers = vec![0.0f32; count];
    let mut cursor = before.first().copied().unwrap_or(0.0);
    for l in 0..count {
        centers[l] = cursor + thickness[l] / 2.0;
        cursor += thickness[l] + after[l];
        if let Some(next) = before.get(l + 1) {
            cursor += gap + next;
        }
    }
    centers
}

/// Minimum center distance between neighbors `a` then `b` in one layer.
fn separation(a: &Vertex, b: &Vertex, style: &ClusterStyle<'_>) -> f32 {
    let gap = match (a.dummy, b.dummy) {
        (true, true) => DUMMY_GAP,
        (false, false) => NODE_GAP,
        _ => DUMMY_NODE_GAP,
    };
    let shared = common_prefix(&a.clusters, &b.clusters);
    let leaving = a.clusters.len().saturating_sub(shared) as f32 * CLUSTER_PAD;
    let entering: f32 = b
        .clusters
        .iter()
        .skip(shared)
        .map(|&c| {
            CLUSTER_PAD
                + if style.side == TitleSide::MinX {
                    style.bands[c]
                } else {
                    0.0
                }
        })
        .sum();
    a.width / 2.0 + a.right_extra + b.width / 2.0 + gap + leaving + entering
}

/// Weighted least-squares placement: each layer in turn moves toward the mean
/// of its neighbors, solved exactly under the ordering/separation constraints.
fn assign_x(
    layers: &[Vec<usize>],
    vertices: &[Vertex],
    up: &Neighbors,
    down: &Neighbors,
    style: &ClusterStyle<'_>,
) -> Vec<f32> {
    let mut xs = vec![0.0f32; vertices.len()];
    let separations: Vec<Vec<f32>> = layers
        .iter()
        .map(|row| {
            row.windows(2)
                .map(|p| separation(&vertices[p[0]], &vertices[p[1]], style))
                .collect()
        })
        .collect();
    for (row, seps) in layers.iter().zip(&separations) {
        let total: f32 = seps.iter().sum();
        let mut x = -total / 2.0;
        for (index, &v) in row.iter().enumerate() {
            xs[v] = x;
            x += seps.get(index).copied().unwrap_or(0.0);
        }
    }
    let count = layers.len();
    for iteration in 0..X_ITERATIONS {
        // Least squares converges quickly to a balanced drawing but spreads
        // every offset evenly (a staircase). The second half reweights toward
        // an L1 objective, which instead keeps chains straight and puts bends
        // in as few edges as possible.
        let robust = iteration >= X_ITERATIONS / 2;
        for step in 0..count {
            let l = if iteration % 2 == 0 {
                step
            } else {
                count - 1 - step
            };
            let row = &layers[l];
            if row.is_empty() {
                continue;
            }
            let mut targets = Vec::with_capacity(row.len());
            let mut weights = Vec::with_capacity(row.len());
            for &v in row {
                let (mut sum, mut total) = (0.0, 0.0);
                for &(n, w) in up[v].iter().chain(&down[v]) {
                    let w = if robust {
                        w / ((xs[v] - xs[n]).abs() + L1_SOFTNESS)
                    } else {
                        w
                    };
                    sum += w * xs[n];
                    total += w;
                }
                if total > 0.0 {
                    targets.push(sum / total);
                    weights.push(total);
                } else {
                    targets.push(xs[v]);
                    weights.push(1e-4);
                }
            }
            for (&v, x) in row
                .iter()
                .zip(place_row(&targets, &weights, &separations[l]))
            {
                xs[v] = x;
            }
        }
    }
    snap_straight(layers, up, down, &separations, &mut xs);
    xs
}

/// Moves a vertex onto a nearly aligned neighbor when its row has room, so
/// edges that are almost vertical become exactly vertical.
fn snap_straight(
    layers: &[Vec<usize>],
    up: &Neighbors,
    down: &Neighbors,
    separations: &[Vec<f32>],
    xs: &mut [f32],
) {
    for pass in 0..2 {
        let adjacent = if pass == 0 { up } else { down };
        for (row, seps) in layers.iter().zip(separations) {
            for (i, &v) in row.iter().enumerate() {
                let Some(target) = adjacent[v]
                    .iter()
                    .map(|&(n, _)| xs[n])
                    .filter(|&x| (x - xs[v]).abs() < SNAP_DISTANCE)
                    .min_by(|a, b| (a - xs[v]).abs().total_cmp(&(b - xs[v]).abs()))
                else {
                    continue;
                };
                let left_ok = i == 0 || target - xs[row[i - 1]] >= seps[i - 1] - 0.01;
                let right_ok = i + 1 == row.len() || xs[row[i + 1]] - target >= seps[i] - 0.01;
                if left_ok && right_ok {
                    xs[v] = target;
                }
            }
        }
    }
}

/// Minimizes sum w_i (x_i - t_i)^2 subject to x_{i+1} - x_i >= s_i. Shifting by
/// the cumulative separations turns this into isotonic regression, which pool
/// adjacent violators solves exactly.
fn place_row(targets: &[f32], weights: &[f32], seps: &[f32]) -> Vec<f32> {
    let mut offsets = Vec::with_capacity(targets.len());
    let mut acc = 0.0;
    for index in 0..targets.len() {
        offsets.push(acc);
        acc += seps.get(index).copied().unwrap_or(0.0);
    }
    // (weight sum, weighted value sum, member count)
    let mut blocks: Vec<(f32, f32, usize)> = Vec::new();
    for ((&t, &w), &offset) in targets.iter().zip(weights).zip(&offsets) {
        blocks.push((w, w * (t - offset), 1));
        while blocks.len() >= 2 {
            let n = blocks.len();
            let (w1, s1, _) = blocks[n - 2];
            let (w2, s2, _) = blocks[n - 1];
            if s1 / w1 <= s2 / w2 {
                break;
            }
            if let Some((w, s, k)) = blocks.pop()
                && let Some(previous) = blocks.last_mut()
            {
                previous.0 += w;
                previous.1 += s;
                previous.2 += k;
            }
        }
    }
    let mut placed = Vec::with_capacity(targets.len());
    for (w, s, k) in blocks {
        let mean = s / w;
        for _ in 0..k {
            placed.push(mean + offsets[placed.len()]);
        }
    }
    placed
}

/// Layout-space cluster outlines: members plus padding, nested boxes inside
/// their parents, and a title band on the side that becomes the screen top.
fn cluster_bounds(
    style: &ClusterStyle<'_>,
    vertices: &[Vertex],
    xs: &[f32],
    centers: &[f32],
) -> Vec<Option<Bounds>> {
    let count = style.clusters.len();
    let mut raw: Vec<Option<Bounds>> = vec![None; count];
    for (index, vertex) in vertices.iter().enumerate() {
        let cy = centers[vertex.layer];
        let b = Bounds::new(
            xs[index] - vertex.width / 2.0,
            cy - vertex.height / 2.0,
            xs[index] + vertex.width / 2.0 + vertex.right_extra,
            cy + vertex.height / 2.0,
        );
        for &c in &vertex.clusters {
            raw[c] = Some(raw[c].map_or(b, |r| r.union(b)));
        }
    }
    let mut boxes = vec![None; count];
    for c in (0..count).rev() {
        let Some(inner) = raw[c] else {
            continue;
        };
        let mut b = inner.grow(CLUSTER_PAD);
        let band = style.bands[c];
        match style.side {
            TitleSide::MinY => b.y0 -= band,
            TitleSide::MaxY => b.y1 += band,
            TitleSide::MinX => b.x0 -= band,
        }
        let title_room = style.title_widths[c] + 2.0 * TITLE_INSET_X;
        match style.direction {
            Direction::Down | Direction::Up => b.x1 = b.x1.max(b.x0 + title_room),
            Direction::Right => b.y1 = b.y1.max(b.y0 + title_room),
            Direction::Left => b.y0 = b.y0.min(b.y1 - title_room),
        }
        boxes[c] = Some(b);
        if let Some(parent) = style.clusters[c].parent
            && let Some(slot) = raw.get_mut(parent)
        {
            *slot = Some(slot.map_or(b, |r| r.union(b)));
        }
    }
    boxes
}

/// Pushes non-members out of cluster outlines. Ordering keeps members
/// contiguous within a layer, but a cluster's outline spans several layers,
/// so outsiders in one layer can still fall inside it.
fn separate_clusters(
    style: &ClusterStyle<'_>,
    layers: &[Vec<usize>],
    vertices: &[Vertex],
    xs: &mut [f32],
    centers: &[f32],
) {
    if style.clusters.is_empty() {
        return;
    }
    for _ in 0..OVERLAP_PASSES {
        let boxes = cluster_bounds(style, vertices, xs, centers);
        let mut moved = false;
        for (c, bounds) in boxes.iter().enumerate() {
            let Some(b) = *bounds else {
                continue;
            };
            for row in layers {
                let first_member = row.iter().position(|&u| vertices[u].clusters.contains(&c));
                for (i, &v) in row.iter().enumerate() {
                    let vertex = &vertices[v];
                    if vertex.clusters.contains(&c) {
                        continue;
                    }
                    let cy = centers[vertex.layer];
                    if cy + vertex.height / 2.0 <= b.y0 || cy - vertex.height / 2.0 >= b.y1 {
                        continue;
                    }
                    let left = xs[v] - vertex.width / 2.0 - CLUSTER_CLEARANCE;
                    let right = xs[v] + vertex.width / 2.0 + vertex.right_extra + CLUSTER_CLEARANCE;
                    if right <= b.x0 || left >= b.x1 {
                        continue;
                    }
                    let go_left = match first_member {
                        Some(member) => i < member,
                        None => xs[v] < (b.x0 + b.x1) / 2.0,
                    };
                    if go_left {
                        let shift = right - b.x0;
                        for &u in &row[..=i] {
                            xs[u] -= shift;
                        }
                    } else {
                        let shift = b.x1 - left;
                        for &u in &row[i..] {
                            xs[u] += shift;
                        }
                    }
                    moved = true;
                }
            }
        }
        if !moved {
            break;
        }
    }
}

/// Where the ray from a shape's center toward `toward` leaves its outline.
fn clip_to_outline(
    kind: ShapeKind,
    center: (f32, f32),
    (w, h): (f32, f32),
    toward: (f32, f32),
) -> (f32, f32) {
    let (dx, dy) = (toward.0 - center.0, toward.1 - center.1);
    let (hw, hh) = ((w / 2.0).max(0.5), (h / 2.0).max(0.5));
    if dx.abs() < 1e-3 && dy.abs() < 1e-3 {
        return (center.0, center.1 + hh);
    }
    let scale = match kind {
        ShapeKind::Diamond => 1.0 / (dx.abs() / hw + dy.abs() / hh),
        ShapeKind::Circle => hw.min(hh) / (dx * dx + dy * dy).sqrt(),
        _ => {
            let sx = if dx.abs() > 1e-6 {
                hw / dx.abs()
            } else {
                f32::INFINITY
            };
            let sy = if dy.abs() > 1e-6 {
                hh / dy.abs()
            } else {
                f32::INFINITY
            };
            sx.min(sy)
        }
    };
    let scale = scale.min(1.0);
    (center.0 + dx * scale, center.1 + dy * scale)
}

fn cubic(p0: (f32, f32), c1: (f32, f32), c2: (f32, f32), p3: (f32, f32), t: f32) -> (f32, f32) {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    (
        a * p0.0 + b * c1.0 + c * c2.0 + d * p3.0,
        a * p0.1 + b * c1.1 + c * c2.1 + d * p3.1,
    )
}

/// Draws a polyline as a Catmull-Rom spline sampled into short segments.
/// Endpoint tangents follow the flow axis, so edges leave and enter nodes
/// along the flow; interior bend points get smooth tangents. Endpoints are
/// preserved exactly, and edges that are already straight stay two points.
fn smooth(points: &[(f32, f32)], vertical: bool) -> Vec<(f32, f32)> {
    let count = points.len();
    let (Some(&first), Some(&last)) = (points.first(), points.last()) else {
        return Vec::new();
    };
    let lateral = |p: (f32, f32)| if vertical { p.0 } else { p.1 };
    if points
        .iter()
        .all(|&p| (lateral(p) - lateral(first)).abs() < 0.5)
    {
        return vec![first, last];
    }
    let along = |p: (f32, f32), q: (f32, f32)| {
        if vertical {
            (0.0, q.1 - p.1)
        } else {
            (q.0 - p.0, 0.0)
        }
    };
    let tangents: Vec<(f32, f32)> = (0..count)
        .map(|i| {
            match (
                i.checked_sub(1).and_then(|j| points.get(j)),
                points.get(i + 1),
            ) {
                (Some(&prev), Some(&next)) => ((next.0 - prev.0) / 2.0, (next.1 - prev.1) / 2.0),
                (None, Some(&next)) => along(points[i], next),
                (Some(&prev), None) => along(prev, points[i]),
                (None, None) => (0.0, 0.0),
            }
        })
        .collect();
    let mut out = Vec::with_capacity(count * CURVE_STEPS);
    out.push(first);
    for i in 0..count - 1 {
        let (p, q) = (points[i], points[i + 1]);
        let (tp, tq) = (tangents[i], tangents[i + 1]);
        let c1 = (p.0 + tp.0 / 3.0, p.1 + tp.1 / 3.0);
        let c2 = (q.0 - tq.0 / 3.0, q.1 - tq.1 / 3.0);
        for k in 1..=CURVE_STEPS {
            out.push(cubic(p, c1, c2, q, k as f32 / CURVE_STEPS as f32));
        }
    }
    out
}

/// The point halfway along a polyline.
fn midpoint(points: &[(f32, f32)]) -> (f32, f32) {
    let length = |p: (f32, f32), q: (f32, f32)| ((q.0 - p.0).powi(2) + (q.1 - p.1).powi(2)).sqrt();
    let total: f32 = points.windows(2).map(|p| length(p[0], p[1])).sum();
    let mut remaining = total / 2.0;
    for pair in points.windows(2) {
        let segment = length(pair[0], pair[1]);
        if segment >= remaining && segment > 0.0 {
            let t = remaining / segment;
            return (
                pair[0].0 + (pair[1].0 - pair[0].0) * t,
                pair[0].1 + (pair[1].1 - pair[0].1) * t,
            );
        }
        remaining -= segment;
    }
    points.first().copied().unwrap_or((0.0, 0.0))
}

/// A loop off the node's +x layout side (screen right, or bottom when the flow
/// is horizontal); returns the curve and its label center.
fn self_loop(
    shape: ShapeKind,
    center: (f32, f32),
    size: (f32, f32),
    reach: f32,
    (lw, lh): (f32, f32),
    horizontal: bool,
) -> (Vec<(f32, f32)>, (f32, f32)) {
    let (cx, cy) = center;
    let (w, h) = size;
    let (start, end, c1, c2, label) = if horizontal {
        let spread = (w / 4.0).min(8.0);
        let start = clip_to_outline(shape, center, size, (cx - spread, cy + h));
        let end = clip_to_outline(shape, center, size, (cx + spread, cy + h));
        (
            start,
            end,
            (start.0 - reach * 0.35, start.1 + reach * 1.3),
            (end.0 + reach * 0.35, end.1 + reach * 1.3),
            (cx, start.1.max(end.1) + reach + 4.0 + lh / 2.0),
        )
    } else {
        let spread = (h / 4.0).min(8.0);
        let start = clip_to_outline(shape, center, size, (cx + w, cy - spread));
        let end = clip_to_outline(shape, center, size, (cx + w, cy + spread));
        (
            start,
            end,
            (start.0 + reach * 1.3, start.1 - reach * 0.35),
            (end.0 + reach * 1.3, end.1 + reach * 0.35),
            (start.0.max(end.0) + reach + 4.0 + lw / 2.0, cy),
        )
    };
    let mut points = vec![start];
    for k in 1..=CURVE_STEPS {
        points.push(cubic(start, c1, c2, end, k as f32 / CURVE_STEPS as f32));
    }
    (points, label)
}

// ---------------------------------------------------------------------------
// Sequence diagrams
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct Sequence {
    participants: Vec<Participant>,
    events: Vec<SeqEvent>,
    autonumber: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct Participant {
    id: String,
    label: String,
    actor: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct Message {
    from: usize,
    to: usize,
    text: String,
    stroke: Stroke,
    arrow_start: Arrow,
    arrow_end: Arrow,
    /// `+` suffix: activates the receiver.
    activate: bool,
    /// `-` suffix: deactivates the sender.
    deactivate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotePlacement {
    LeftOf(usize),
    RightOf(usize),
    Over(usize, usize),
}

#[derive(Debug, Clone, PartialEq)]
enum SeqEvent {
    Message(Message),
    Note {
        placement: NotePlacement,
        text: String,
    },
    Activate(usize),
    Deactivate(usize),
    /// `loop`, `alt`, ... (`title`) or `rect`/`box` (no title). `box` groups
    /// participants and is not drawn.
    BlockStart {
        title: Option<String>,
        drawn: bool,
    },
    BlockDivider(String),
    BlockEnd,
}

#[derive(Default)]
struct SeqParser {
    participants: Vec<Participant>,
    events: Vec<SeqEvent>,
    autonumber: bool,
    ids: HashMap<String, usize>,
    depth: usize,
    messages: usize,
}

fn parse_sequence<'a>(lines: impl Iterator<Item = &'a str>) -> Option<Sequence> {
    let mut parser = SeqParser::default();
    for line in lines {
        parser.line(line.trim())?;
    }
    if parser.participants.is_empty() {
        return None;
    }
    Some(Sequence {
        participants: parser.participants,
        events: parser.events,
        autonumber: parser.autonumber,
    })
}

/// Arrow tokens, longest first at any position.
const SEQ_ARROWS: [(&str, Stroke, Arrow, Arrow); 10] = [
    ("<<-->>", Stroke::Dotted, Arrow::Filled, Arrow::Filled),
    ("<<->>", Stroke::Solid, Arrow::Filled, Arrow::Filled),
    ("-->>", Stroke::Dotted, Arrow::None, Arrow::Filled),
    ("->>", Stroke::Solid, Arrow::None, Arrow::Filled),
    ("--x", Stroke::Dotted, Arrow::None, Arrow::Cross),
    ("-x", Stroke::Solid, Arrow::None, Arrow::Cross),
    ("--)", Stroke::Dotted, Arrow::None, Arrow::Open),
    ("-)", Stroke::Solid, Arrow::None, Arrow::Open),
    ("-->", Stroke::Dotted, Arrow::None, Arrow::None),
    ("->", Stroke::Solid, Arrow::None, Arrow::None),
];

impl SeqParser {
    /// `None` only when a cap is exceeded; unknown lines are ignored.
    fn line(&mut self, line: &str) -> Option<()> {
        if line.is_empty() || line.starts_with("%%") {
            return Some(());
        }
        let line = line.strip_suffix(';').unwrap_or(line).trim();
        let (word, rest) = split_word(line);
        let keyword = word.to_ascii_lowercase();
        match keyword.as_str() {
            "participant" | "actor" => self.declare(rest, keyword == "actor"),
            "create" => {
                let (inner, rest) = split_word(rest);
                let inner = inner.to_ascii_lowercase();
                if inner == "participant" || inner == "actor" {
                    self.declare(rest, inner == "actor")?;
                }
                Some(())
            }
            "destroy" | "title" | "acctitle" | "accdescr" | "link" | "links" | "properties"
            | "details" => Some(()),
            "autonumber" => {
                self.autonumber = true;
                Some(())
            }
            "activate" | "deactivate" => {
                let participant = self.intern(rest)?;
                self.push(if keyword == "activate" {
                    SeqEvent::Activate(participant)
                } else {
                    SeqEvent::Deactivate(participant)
                })
            }
            "note" => self.note(rest),
            "loop" | "alt" | "opt" | "par" | "par_over" | "critical" | "break" => {
                let name = if keyword == "par_over" {
                    "par"
                } else {
                    keyword.as_str()
                };
                let text = clean_label(rest);
                let title = if text.is_empty() {
                    name.to_owned()
                } else {
                    format!("{name}: {text}")
                };
                self.open_block(Some(title), true)
            }
            "rect" => self.open_block(None, true),
            "box" => self.open_block(None, false),
            "else" | "and" | "option" => {
                if self.depth == 0 {
                    return Some(());
                }
                self.push(SeqEvent::BlockDivider(clean_label(rest)))
            }
            "end" => {
                if self.depth == 0 {
                    return Some(());
                }
                self.depth -= 1;
                self.push(SeqEvent::BlockEnd)
            }
            _ => self.message(line),
        }
    }

    fn push(&mut self, event: SeqEvent) -> Option<()> {
        if self.events.len() >= MAX_SEQUENCE_EVENTS {
            return None;
        }
        self.events.push(event);
        Some(())
    }

    fn open_block(&mut self, title: Option<String>, drawn: bool) -> Option<()> {
        if self.depth >= MAX_NESTING {
            return None;
        }
        self.depth += 1;
        self.push(SeqEvent::BlockStart { title, drawn })
    }

    fn intern(&mut self, name: &str) -> Option<usize> {
        let id = name.trim().trim_matches('"').trim();
        if let Some(&index) = self.ids.get(id) {
            return Some(index);
        }
        if self.participants.len() >= MAX_NODES {
            return None;
        }
        self.participants.push(Participant {
            id: id.to_owned(),
            label: clean_label(id),
            actor: false,
        });
        self.ids.insert(id.to_owned(), self.participants.len() - 1);
        Some(self.participants.len() - 1)
    }

    /// `participant A as Alice`, `actor B`
    fn declare(&mut self, rest: &str, actor: bool) -> Option<()> {
        let rest = rest.split("@{").next().unwrap_or(rest).trim();
        let (id, alias) = match rest.split_once(" as ") {
            Some((id, alias)) => (id.trim(), Some(alias)),
            None => (rest, None),
        };
        if id.is_empty() {
            return Some(());
        }
        let index = self.intern(id)?;
        let participant = &mut self.participants[index];
        participant.actor = actor;
        if let Some(alias) = alias {
            participant.label = clean_label(alias);
        }
        Some(())
    }

    fn note(&mut self, rest: &str) -> Option<()> {
        let lower = rest.to_ascii_lowercase();
        // ASCII lowercasing preserves byte offsets, so `rest` slices safely.
        let (kind, after) = if lower.starts_with("left of") {
            (0, &rest[7..])
        } else if lower.starts_with("right of") {
            (1, &rest[8..])
        } else if lower.starts_with("over") {
            (2, &rest[4..])
        } else {
            return Some(());
        };
        let (who, text) = after.split_once(':').unwrap_or((after, ""));
        let mut names = who.split(',').map(str::trim).filter(|n| !n.is_empty());
        let Some(first) = names.next() else {
            return Some(());
        };
        let first = self.intern(first)?;
        let placement = match kind {
            0 => NotePlacement::LeftOf(first),
            1 => NotePlacement::RightOf(first),
            _ => {
                let last = match names.next_back() {
                    Some(name) => self.intern(name)?,
                    None => first,
                };
                NotePlacement::Over(first, last)
            }
        };
        self.push(SeqEvent::Note {
            placement,
            text: clean_label(text),
        })
    }

    fn message(&mut self, line: &str) -> Option<()> {
        let cursor = Cursor::new(line);
        let found = (1..cursor.chars.len()).find_map(|at| {
            SEQ_ARROWS
                .iter()
                .find(|(token, ..)| cursor.matches_at(at, token))
                .map(|arrow| (at, arrow))
        });
        let Some((at, &(token, stroke, arrow_start, arrow_end))) = found else {
            return Some(());
        };
        let from = cursor.slice(0, at);
        let after = cursor.slice(at + token.chars().count(), cursor.chars.len());
        let mut after = after.trim_start();
        let (mut activate, mut deactivate) = (false, false);
        if let Some(rest) = after.strip_prefix('+') {
            activate = true;
            after = rest;
        } else if let Some(rest) = after.strip_prefix('-') {
            deactivate = true;
            after = rest;
        }
        let (to, text) = after.split_once(':').unwrap_or((after, ""));
        if from.trim().is_empty() || to.trim().is_empty() {
            return Some(());
        }
        if self.messages >= MAX_MESSAGES {
            return None;
        }
        self.messages += 1;
        let from = self.intern(&from)?;
        let to = self.intern(to)?;
        self.push(SeqEvent::Message(Message {
            from,
            to,
            text: clean_label(text),
            stroke,
            arrow_start,
            arrow_end,
            activate,
            deactivate,
        }))
    }
}

struct OpenBlock {
    title: Option<String>,
    title_size: (f32, f32),
    drawn: bool,
    top: f32,
    /// Horizontal extent of everything drawn inside, padded children included.
    min_x: f32,
    max_x: f32,
    dividers: Vec<(f32, String, (f32, f32))>,
}

/// Widens every open block to cover `x0..x1`.
fn touch(blocks: &mut [OpenBlock], x0: f32, x1: f32) {
    for block in blocks {
        block.min_x = block.min_x.min(x0);
        block.max_x = block.max_x.max(x1);
    }
}

/// Where a message meets participant `p`'s lifeline or its top activation bar.
fn attach_x(centers: &[f32], active: &[Vec<f32>], p: usize, toward_right: bool) -> f32 {
    let depth = active[p].len();
    let center = centers[p];
    if depth == 0 {
        center
    } else if toward_right {
        center + ACTIVATION_WIDTH / 2.0 + ACTIVATION_STEP * (depth - 1) as f32
    } else {
        center - ACTIVATION_WIDTH / 2.0
    }
}

fn layout_sequence(seq: &Sequence, measure: &impl Fn(&str, Font) -> (f32, f32)) -> Scene {
    let count = seq.participants.len();
    let mut back = SceneBuilder::default();
    if count == 0 {
        return back.finish();
    }
    let names: Vec<(f32, f32)> = seq
        .participants
        .iter()
        .map(|p| text_size(measure, &p.label, Font::Node))
        .collect();
    let box_widths: Vec<f32> = names
        .iter()
        .map(|&(w, _)| (w + 2.0 * SEQ_BOX_PAD_X).max(SEQ_MIN_BOX_WIDTH))
        .collect();
    let box_height = names.iter().map(|&(_, h)| h).fold(0.0, f32::max) + 2.0 * SEQ_BOX_PAD_Y;

    let mut number = 0;
    let texts: Vec<String> = seq
        .events
        .iter()
        .map(|event| match event {
            SeqEvent::Message(m) if seq.autonumber => {
                number += 1;
                if m.text.is_empty() {
                    number.to_string()
                } else {
                    format!("{number}. {}", m.text)
                }
            }
            SeqEvent::Message(m) => m.text.clone(),
            SeqEvent::Note { text, .. } => text.clone(),
            SeqEvent::BlockStart { title: Some(t), .. } => t.clone(),
            SeqEvent::BlockDivider(t) if !t.is_empty() => format!("[{t}]"),
            _ => String::new(),
        })
        .collect();
    let sizes: Vec<(f32, f32)> = texts
        .iter()
        .map(|t| text_size(measure, t, Font::Edge))
        .collect();
    let line_height = text_size(measure, "Mg", Font::Edge).1;

    // Column spacing: start from the boxes, then widen spans whose labels or
    // side notes need more room, shortest spans first.
    let mut gaps: Vec<f32> = box_widths
        .windows(2)
        .map(|p| p[0] / 2.0 + p[1] / 2.0 + SEQ_COLUMN_GAP)
        .collect();
    let mut needs: Vec<(usize, usize, f32)> = Vec::new();
    for (event, &(tw, _)) in seq.events.iter().zip(&sizes) {
        match event {
            SeqEvent::Message(m) if m.from != m.to => {
                needs.push((
                    m.from.min(m.to),
                    m.from.max(m.to),
                    tw + 2.0 * SEQ_LABEL_CLEARANCE,
                ));
            }
            SeqEvent::Message(m) => needs.push((m.from, m.from + 1, tw + SEQ_LABEL_CLEARANCE)),
            SeqEvent::Note { placement, .. } => {
                let room = tw + 2.0 * NOTE_PAD_X + NOTE_OFFSET + SEQ_LABEL_CLEARANCE;
                match *placement {
                    NotePlacement::RightOf(p) => needs.push((p, p + 1, room)),
                    NotePlacement::LeftOf(p) if p > 0 => needs.push((p - 1, p, room)),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    needs.sort_by_key(|&(lo, hi, _)| hi - lo);
    for (lo, hi, need) in needs {
        if hi <= lo || hi > gaps.len() {
            continue;
        }
        let have: f32 = gaps[lo..hi].iter().sum();
        if have < need {
            let add = (need - have) / (hi - lo) as f32;
            for gap in &mut gaps[lo..hi] {
                *gap += add;
            }
        }
    }
    let mut centers = Vec::with_capacity(count);
    let mut x = box_widths[0] / 2.0;
    centers.push(x);
    for gap in &gaps {
        x += gap;
        centers.push(x);
    }

    let mut main = SceneBuilder::default();
    let mut notes = SceneBuilder::default();
    let mut active: Vec<Vec<f32>> = vec![Vec::new(); count];
    let mut bars: Vec<(usize, usize, f32, f32)> = Vec::new();
    let mut open: Vec<OpenBlock> = Vec::new();
    let mut blocks: Vec<(usize, SceneBuilder)> = Vec::new();
    let mut last_line: Option<f32> = None;
    let mut y = box_height + SEQ_TOP_GAP;

    for ((event, text), &(tw, th)) in seq.events.iter().zip(&texts).zip(&sizes) {
        match event {
            SeqEvent::Message(m) => {
                let line_y = if text.is_empty() {
                    y + 4.0
                } else {
                    y + th + SEQ_LABEL_GAP
                };
                if m.activate {
                    active[m.to].push(line_y);
                }
                if m.from != m.to {
                    let rightward = centers[m.to] >= centers[m.from];
                    let x1 = attach_x(&centers, &active, m.from, rightward);
                    let x2 = attach_x(&centers, &active, m.to, !rightward);
                    let cx = (x1 + x2) / 2.0;
                    main.line(
                        vec![(x1, line_y), (x2, line_y)],
                        m.stroke,
                        m.arrow_start,
                        m.arrow_end,
                    );
                    main.label(text, (cx, y + th / 2.0), (tw, th), Font::Edge, false);
                    let (lo, hi) = (
                        centers[m.from].min(centers[m.to]),
                        centers[m.from].max(centers[m.to]),
                    );
                    touch(
                        &mut open,
                        (lo - SEQ_BLOCK_REACH).min(cx - tw / 2.0),
                        (hi + SEQ_BLOCK_REACH).max(cx + tw / 2.0),
                    );
                    y = line_y + SEQ_ROW_GAP;
                } else {
                    let x = attach_x(&centers, &active, m.from, true);
                    let points = vec![
                        (x, line_y),
                        (x + SEQ_SELF_WIDTH, line_y),
                        (x + SEQ_SELF_WIDTH, line_y + SEQ_SELF_HEIGHT),
                        (x, line_y + SEQ_SELF_HEIGHT),
                    ];
                    main.line(points, m.stroke, m.arrow_start, m.arrow_end);
                    main.label(
                        text,
                        (x + 6.0 + tw / 2.0, y + th / 2.0),
                        (tw, th),
                        Font::Edge,
                        false,
                    );
                    touch(
                        &mut open,
                        centers[m.from] - SEQ_BLOCK_REACH,
                        (x + SEQ_SELF_WIDTH)
                            .max(x + 6.0 + tw)
                            .max(centers[m.from] + SEQ_BLOCK_REACH),
                    );
                    y = line_y + SEQ_SELF_HEIGHT + SEQ_ROW_GAP;
                }
                if m.deactivate
                    && let Some(start) = active[m.from].pop()
                {
                    bars.push((
                        m.from,
                        active[m.from].len(),
                        start,
                        line_y.max(start + ACTIVATION_MIN),
                    ));
                }
                last_line = Some(line_y);
            }
            SeqEvent::Note { placement, .. } => {
                let (w, h) = (tw + 2.0 * NOTE_PAD_X, th + 2.0 * NOTE_PAD_Y);
                let (x0, width) = match *placement {
                    NotePlacement::LeftOf(p) => {
                        (attach_x(&centers, &active, p, false) - NOTE_OFFSET - w, w)
                    }
                    NotePlacement::RightOf(p) => {
                        (attach_x(&centers, &active, p, true) + NOTE_OFFSET, w)
                    }
                    NotePlacement::Over(a, b) => {
                        let (lo, hi) = (centers[a].min(centers[b]), centers[a].max(centers[b]));
                        let width = w.max(hi - lo + 2.0 * NOTE_OVERHANG);
                        ((lo + hi) / 2.0 - width / 2.0, width)
                    }
                };
                notes.shape(
                    ShapeKind::Note,
                    Bounds::new(x0, y, x0 + width, y + h),
                    Tone::Muted,
                );
                notes.label(
                    text,
                    (x0 + width / 2.0, y + h / 2.0),
                    (tw, th),
                    Font::Edge,
                    false,
                );
                touch(&mut open, x0, x0 + width);
                y += h + SEQ_ROW_GAP * 0.75;
                last_line = None;
            }
            SeqEvent::Activate(p) => active[*p].push(last_line.unwrap_or(y)),
            SeqEvent::Deactivate(p) => {
                if let Some(start) = active[*p].pop() {
                    let end = last_line.unwrap_or(y).max(start + ACTIVATION_MIN);
                    bars.push((*p, active[*p].len(), start, end));
                }
            }
            SeqEvent::BlockStart { title, drawn } => {
                y += 4.0;
                let top = y;
                y += if title.is_some() { th + 12.0 } else { 10.0 };
                open.push(OpenBlock {
                    title: title.clone(),
                    title_size: (tw, th),
                    drawn: *drawn,
                    top,
                    min_x: f32::INFINITY,
                    max_x: f32::NEG_INFINITY,
                    dividers: Vec::new(),
                });
                last_line = None;
            }
            SeqEvent::BlockDivider(_) => {
                if let Some(block) = open.last_mut() {
                    block.dividers.push((y, text.clone(), (tw, th)));
                }
                y += th.max(line_height) + 12.0;
                last_line = None;
            }
            SeqEvent::BlockEnd => {
                close_block(&mut open, &mut blocks, &mut y, centers[0]);
                last_line = None;
            }
        }
    }
    while !open.is_empty() {
        close_block(&mut open, &mut blocks, &mut y, centers[0]);
    }
    // Blocks close innermost first; paint outer blocks first so they stay behind.
    blocks.sort_by_key(|&(depth, _)| depth);
    for (_, block) in blocks {
        back.extend(block);
    }
    for (p, stack) in active.iter().enumerate() {
        for (depth, &start) in stack.iter().enumerate() {
            bars.push((p, depth, start, y.max(start + ACTIVATION_MIN)));
        }
    }

    let bottom = y + 6.0;
    let mut frame = SceneBuilder::default();
    for (index, participant) in seq.participants.iter().enumerate() {
        let c = centers[index];
        frame.shape(
            ShapeKind::Lifeline,
            Bounds::new(c, box_height, c, bottom),
            Tone::Normal,
        );
        let tone = if participant.actor {
            Tone::Accent
        } else {
            Tone::Normal
        };
        let half = box_widths[index] / 2.0;
        frame.shape(
            ShapeKind::Rounded,
            Bounds::new(c - half, 0.0, c + half, box_height),
            tone,
        );
        frame.label(
            &participant.label,
            (c, box_height / 2.0),
            names[index],
            Font::Node,
            false,
        );
    }
    for (p, depth, start, end) in bars {
        let x0 = centers[p] - ACTIVATION_WIDTH / 2.0 + ACTIVATION_STEP * depth as f32;
        frame.shape(
            ShapeKind::Activation,
            Bounds::new(x0, start, x0 + ACTIVATION_WIDTH, end),
            Tone::Normal,
        );
    }
    back.extend(frame);
    back.extend(main);
    back.extend(notes);
    back.finish()
}

/// Closes the innermost open block, queuing its primitives with its depth.
fn close_block(
    open: &mut Vec<OpenBlock>,
    closed: &mut Vec<(usize, SceneBuilder)>,
    y: &mut f32,
    fallback_x: f32,
) {
    let Some(block) = open.pop() else {
        return;
    };
    let mut back = SceneBuilder::default();
    *y += 6.0;
    let bottom = *y;
    *y += 10.0;
    let (mut x0, mut x1) = if block.min_x <= block.max_x {
        (block.min_x - BLOCK_PAD, block.max_x + BLOCK_PAD)
    } else {
        (fallback_x - 40.0, fallback_x + 40.0)
    };
    let (tw, th) = block.title_size;
    x1 = x1.max(x0 + tw + 2.0 * TITLE_INSET_X);
    for (_, _, (dw, _)) in &block.dividers {
        x1 = x1.max(x0 + dw + 2.0 * TITLE_INSET_X);
    }
    if block.drawn {
        back.shape(
            ShapeKind::Cluster,
            Bounds::new(x0, block.top, x1, bottom),
            Tone::Muted,
        );
        if let Some(title) = &block.title {
            back.label(
                title,
                (x0 + TITLE_INSET_X + tw / 2.0, block.top + 4.0 + th / 2.0),
                (tw, th),
                Font::Edge,
                false,
            );
        }
        for (dy, text, (dw, dh)) in &block.dividers {
            back.line(
                vec![(x0, *dy), (x1, *dy)],
                Stroke::Dotted,
                Arrow::None,
                Arrow::None,
            );
            back.label(
                text,
                (x0 + TITLE_INSET_X + dw / 2.0, dy + 4.0 + dh / 2.0),
                (*dw, *dh),
                Font::Edge,
                false,
            );
        }
    } else {
        // Undrawn groups take no horizontal room of their own.
        x0 += BLOCK_PAD;
        x1 -= BLOCK_PAD;
    }
    closed.push((open.len(), back));
    touch(open, x0, x1);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn measure(text: &str, font: Font) -> (f32, f32) {
        let (per_char, height) = match font {
            Font::Node => (7.0, 17.0),
            Font::Edge => (6.2, 15.0),
        };
        (text.chars().count() as f32 * per_char, height)
    }

    fn flow(source: &str) -> Flowchart {
        match parse(source).unwrap().body {
            Body::Flow(flow) => flow,
            Body::Sequence(_) => panic!("expected a flowchart"),
        }
    }

    fn sequence(source: &str) -> Sequence {
        match parse(source).unwrap().body {
            Body::Sequence(sequence) => sequence,
            Body::Flow(_) => panic!("expected a sequence diagram"),
        }
    }

    fn node<'a>(flow: &'a Flowchart, id: &str) -> &'a FlowNode {
        flow.nodes.iter().find(|n| n.id == id).unwrap()
    }

    fn node_index(flow: &Flowchart, id: &str) -> usize {
        flow.nodes.iter().position(|n| n.id == id).unwrap()
    }

    fn layout(source: &str) -> Scene {
        parse(source).unwrap().layout(measure)
    }

    fn shape_with_label<'a>(scene: &'a Scene, text: &str) -> &'a Shape {
        let label = scene.labels.iter().find(|l| l.text == text).unwrap();
        scene
            .shapes
            .iter()
            .filter(|s| !matches!(s.kind, ShapeKind::Cluster))
            .find(|s| {
                (s.x + s.width / 2.0 - label.x).abs() < 0.5
                    && (s.y + s.height / 2.0 - label.y).abs() < 4.0
            })
            .unwrap()
    }

    fn is_node(shape: &Shape) -> bool {
        !matches!(
            shape.kind,
            ShapeKind::Cluster | ShapeKind::Lifeline | ShapeKind::Activation | ShapeKind::Note
        )
    }

    fn overlaps(a: &Shape, b: &Shape) -> bool {
        a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
    }

    fn segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len2 = dx * dx + dy * dy;
        let t = if len2 > 0.0 {
            (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        ((p.0 - a.0 - t * dx).powi(2) + (p.1 - a.1 - t * dy).powi(2)).sqrt()
    }

    /// Distance from `p` to the outline of `shape`.
    fn outline_distance(shape: &Shape, p: (f32, f32)) -> f32 {
        let (cx, cy) = (shape.x + shape.width / 2.0, shape.y + shape.height / 2.0);
        let (hw, hh) = (shape.width / 2.0, shape.height / 2.0);
        let polygon: Vec<(f32, f32)> = match shape.kind {
            ShapeKind::Circle => {
                return (((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt() - hw.min(hh)).abs();
            }
            ShapeKind::Diamond => vec![(cx, cy - hh), (cx + hw, cy), (cx, cy + hh), (cx - hw, cy)],
            _ => vec![
                (shape.x, shape.y),
                (shape.x + shape.width, shape.y),
                (shape.x + shape.width, shape.y + shape.height),
                (shape.x, shape.y + shape.height),
            ],
        };
        (0..polygon.len())
            .map(|i| segment_distance(p, polygon[i], polygon[(i + 1) % polygon.len()]))
            .fold(f32::INFINITY, f32::min)
    }

    fn assert_in_bounds(scene: &Scene) {
        let inside = |x: f32, y: f32| {
            x >= -0.01 && y >= -0.01 && x <= scene.width + 0.01 && y <= scene.height + 0.01
        };
        for shape in &scene.shapes {
            assert!(
                inside(shape.x, shape.y) && inside(shape.x + shape.width, shape.y + shape.height),
                "{shape:?}"
            );
        }
        for line in &scene.lines {
            for &(x, y) in &line.points {
                assert!(
                    inside(x, y),
                    "line point ({x}, {y}) outside {}x{}",
                    scene.width,
                    scene.height
                );
            }
        }
        for label in &scene.labels {
            let lines: Vec<&str> = label.text.split('\n').collect();
            let width = lines
                .iter()
                .map(|l| measure(l, label.font).0)
                .fold(0.0, f32::max);
            let height = lines.len() as f32 * measure("x", label.font).1;
            assert!(
                inside(label.x - width / 2.0, label.y - height / 2.0),
                "{label:?}"
            );
            assert!(
                inside(label.x + width / 2.0, label.y + height / 2.0),
                "{label:?}"
            );
        }
    }

    /// Node boxes never overlap, and every edge starts and ends on an outline.
    fn assert_flow_invariants(scene: &Scene) {
        assert_in_bounds(scene);
        let nodes: Vec<&Shape> = scene.shapes.iter().filter(|s| is_node(s)).collect();
        for (i, a) in nodes.iter().enumerate() {
            for b in &nodes[i + 1..] {
                assert!(!overlaps(a, b), "overlap: {a:?} {b:?}");
            }
        }
        for line in &scene.lines {
            for end in [line.points[0], line.points[line.points.len() - 1]] {
                let nearest = nodes
                    .iter()
                    .map(|n| outline_distance(n, end))
                    .fold(f32::INFINITY, f32::min);
                assert!(
                    nearest <= 1.0,
                    "endpoint {end:?} is {nearest}px from any outline"
                );
            }
        }
    }

    const FLOW_SAMPLES: [&str; 7] = [
        "flowchart TD
    A[User prompt] --> B{Needs tools?}
    B -->|yes| C[Call tool]
    B -->|no| D[Answer]
    C --> E[(Results cache)]
    E --> F[Summarize]
    F --> B
    D --> G([Done])",
        "graph LR
    src[Source files] --> parse[Parser] --> ast((AST))
    ast --> check{{Type check}} --> emit[[Codegen]]
    ast -.-> lint[Linter]
    lint -.->|warnings| report>Report]
    emit ==> bin[(Binary)]",
        "flowchart TB
    subgraph client [Client]
        ui[GPUI view] --> bridge[Connection bridge]
    end
    subgraph daemon [Herdr daemon]
        sock[Socket worker] --> sess[Session state]
        sess --> term[Terminals]
    end
    bridge -->|commands| sock
    sess -->|snapshots| bridge
    term --> pty[(PTY)]",
        "graph TD
    root[Orchestrator] --> a[Planner]
    root --> b[Researcher]
    root --> c[Coder]
    root --> d[Reviewer]
    a & b --> merge[Merge findings]
    c --> tests[Run tests]
    tests -- fail --> c
    tests -- pass --> d
    d --> merge
    merge --> out([Final answer])",
        "graph RL
    idle((Idle)) --> run[Running]
    run --> wait[Waiting]
    wait --> run
    run --> done[Done]
    run --> run",
        "flowchart BT
    subgraph outer [Platform]
      subgraph inner [Core]
        k[Kernel] --> d[Drivers]
      end
      svc[Services] --> k
    end
    app[App] --> svc
    app --> lib[Library]",
        "flowchart LR
    a[\"`**Bold** start`\"] --> b(Rounded)
    b --> c([Stadium]) --> d{Decide}
    d -->|one| e((Circle))
    d -->|two<br>lines| f[(Store)]
    d --> g{{Hex}} --o h[[Sub]] --x i>Asym]
    a <--> i",
    ];

    const SEQUENCE_SAMPLE: &str = "sequenceDiagram
    autonumber
    actor U as User
    participant C as Chat view
    participant A as Agent
    participant T as Tool runner
    U->>C: Send prompt
    C->>+A: Stream request
    loop Until done
        A->>T: Invoke tool
        T-->>A: Result
        A->>A: Think
    end
    alt Success
        A-->>C: Final answer
    else Failure
        A--xC: Error
    end
    Note over C,A: Streaming continues
    A-->>-C: Close stream
    Note right of U: Reads reply";

    #[test]
    fn recognizes_supported_headers() {
        assert_eq!(parse("graph TD\nA-->B").unwrap().kind, Kind::Flowchart);
        assert_eq!(parse("flowchart LR; A-->B").unwrap().kind, Kind::Flowchart);
        assert_eq!(
            parse("%% intro\n\nsequenceDiagram\nA->>B: hi")
                .unwrap()
                .kind,
            Kind::Sequence
        );
        assert_eq!(
            parse("---\ntitle: Demo\n---\nflowchart TD\nA-->B")
                .unwrap()
                .kind,
            Kind::Flowchart
        );
        assert_eq!(flow("graph LR\nA-->B").direction, Direction::Right);
        assert_eq!(flow("graph RL\nA-->B").direction, Direction::Left);
        assert_eq!(flow("graph BT\nA-->B").direction, Direction::Up);
        assert_eq!(flow("graph TB\nA-->B").direction, Direction::Down);
        assert_eq!(flow("graph\nA-->B").direction, Direction::Down);
        assert_eq!(flow("flowchart TD; A-->B; B-->C").edges.len(), 2);
    }

    #[test]
    fn rejects_unsupported_or_empty_sources() {
        for source in [
            "",
            "   \n\n",
            "pie title Pets\n\"Dogs\" : 386",
            "gantt\ntitle A Gantt\nsection A\ntask :a1, 2024-01-01, 30d",
            "classDiagram\nAnimal <|-- Duck",
            "stateDiagram-v2\n[*] --> Still",
            "hello world",
            "graph TD",
            "graph TD\n%% only a comment",
            "sequenceDiagram",
            "sequenceDiagram\nautonumber",
        ] {
            assert!(parse(source).is_none(), "{source:?}");
        }
    }

    #[test]
    fn parses_every_node_shape() {
        let f = flow(
            "graph TD
            a[rect] --> b(round) --> c([stadium]) --> d{diamond} --> e((circle))
            f[(cylinder)] --> g{{hexagon}} --> h[[subroutine]] --> i>asym] --> j(((double)))
            k[/lean/] --> l[\\alt\\] --> m",
        );
        let expect = [
            ("a", ShapeKind::Rect, "rect"),
            ("b", ShapeKind::Rounded, "round"),
            ("c", ShapeKind::Stadium, "stadium"),
            ("d", ShapeKind::Diamond, "diamond"),
            ("e", ShapeKind::Circle, "circle"),
            ("f", ShapeKind::Cylinder, "cylinder"),
            ("g", ShapeKind::Hexagon, "hexagon"),
            ("h", ShapeKind::Subroutine, "subroutine"),
            ("i", ShapeKind::Rect, "asym"),
            ("j", ShapeKind::Circle, "double"),
            ("k", ShapeKind::Rect, "lean"),
            ("l", ShapeKind::Rect, "alt"),
            ("m", ShapeKind::Rect, "m"),
        ];
        for (id, shape, label) in expect {
            let n = node(&f, id);
            assert_eq!((n.shape, n.label.as_str()), (shape, label), "{id}");
        }
        let attrs = flow("graph TD\nq@{ shape: diam, label: \"Ready?\" } --> r@{ shape: cyl }");
        assert_eq!(node(&attrs, "q").shape, ShapeKind::Diamond);
        assert_eq!(node(&attrs, "q").label, "Ready?");
        assert_eq!(node(&attrs, "r").shape, ShapeKind::Cylinder);
    }

    #[test]
    fn cleans_label_text() {
        let f = flow(
            "graph TD
            a[\"Quoted (with) [brackets]\"]
            b[\"`**Bold** and *em*`\"]
            c[Say #quot;hi#quot; &amp; #35;1]
            d[one<br>two<br/>three<br />four]
            e[<b>tag</b> soup]
            f[line\\nbreak]
            g(fa:fa-car Car)
            h[my-node]",
        );
        assert_eq!(node(&f, "a").label, "Quoted (with) [brackets]");
        assert_eq!(node(&f, "b").label, "Bold and em");
        assert_eq!(node(&f, "c").label, "Say \"hi\" & #1");
        assert_eq!(node(&f, "d").label, "one\ntwo\nthree\nfour");
        assert_eq!(node(&f, "e").label, "tag soup");
        assert_eq!(node(&f, "f").label, "line\nbreak");
        assert_eq!(node(&f, "g").label, "Car");
        assert_eq!(node(&f, "h").label, "my-node");
        assert_eq!(clean_label("a < b > c"), "a < b > c");
        assert_eq!(clean_label("#1 priority; now"), "#1 priority; now");
        assert!(clean_label(&"x".repeat(5_000)).chars().count() <= MAX_LABEL_CHARS);
    }

    #[test]
    fn parses_link_styles_and_labels() {
        let cases = [
            ("A --> B", Stroke::Solid, Arrow::None, Arrow::Filled, ""),
            ("A --- B", Stroke::Solid, Arrow::None, Arrow::None, ""),
            ("A -.-> B", Stroke::Dotted, Arrow::None, Arrow::Filled, ""),
            ("A -.- B", Stroke::Dotted, Arrow::None, Arrow::None, ""),
            ("A ==> B", Stroke::Thick, Arrow::None, Arrow::Filled, ""),
            ("A === B", Stroke::Thick, Arrow::None, Arrow::None, ""),
            ("A --o B", Stroke::Solid, Arrow::None, Arrow::Circle, ""),
            ("A --x B", Stroke::Solid, Arrow::None, Arrow::Cross, ""),
            ("A <--> B", Stroke::Solid, Arrow::Filled, Arrow::Filled, ""),
            ("A o--o B", Stroke::Solid, Arrow::Circle, Arrow::Circle, ""),
            ("A ---> B", Stroke::Solid, Arrow::None, Arrow::Filled, ""),
            ("A-->B", Stroke::Solid, Arrow::None, Arrow::Filled, ""),
            (
                "A -->|yes| B",
                Stroke::Solid,
                Arrow::None,
                Arrow::Filled,
                "yes",
            ),
            (
                "A --> |spaced| B",
                Stroke::Solid,
                Arrow::None,
                Arrow::Filled,
                "spaced",
            ),
            (
                "A -- some text --> B",
                Stroke::Solid,
                Arrow::None,
                Arrow::Filled,
                "some text",
            ),
            (
                "A -- non-blocking --- B",
                Stroke::Solid,
                Arrow::None,
                Arrow::None,
                "non-blocking",
            ),
            (
                "A -. maybe .-> B",
                Stroke::Dotted,
                Arrow::None,
                Arrow::Filled,
                "maybe",
            ),
            (
                "A == must ==> B",
                Stroke::Thick,
                Arrow::None,
                Arrow::Filled,
                "must",
            ),
            (
                "A -.->|\"quoted\"| B",
                Stroke::Dotted,
                Arrow::None,
                Arrow::Filled,
                "quoted",
            ),
        ];
        for (link, stroke, start, end, label) in cases {
            let f = flow(&format!("graph TD\n{link}"));
            assert_eq!(f.nodes.len(), 2, "{link}");
            let e = &f.edges[0];
            assert_eq!((e.from, e.to), (0, 1), "{link}");
            assert_eq!(
                (e.stroke, e.arrow_start, e.arrow_end, e.label.as_str()),
                (stroke, start, end, label),
                "{link}"
            );
            assert!(e.visible);
        }
        let hidden = flow("graph TD\nA ~~~ B");
        assert!(!hidden.edges[0].visible);
        assert!(layout("graph TD\nA ~~~ B").lines.is_empty());
    }

    #[test]
    fn parses_chains_and_groups() {
        let f = flow("graph TD\nA --> B --> C\nD & E --> F & G");
        let pairs: Vec<(&str, &str)> = f
            .edges
            .iter()
            .map(|e| (f.nodes[e.from].id.as_str(), f.nodes[e.to].id.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("A", "B"),
                ("B", "C"),
                ("D", "F"),
                ("D", "G"),
                ("E", "F"),
                ("E", "G")
            ]
        );
        // Redeclaring a node updates its label but keeps one node.
        let f = flow("graph TD\nA --> B\nA[Start]");
        assert_eq!(f.nodes.len(), 2);
        assert_eq!(node(&f, "A").label, "Start");
    }

    #[test]
    fn parses_subgraphs() {
        let f = flow(
            "graph TD
            subgraph one [First group]
              a --> b
              subgraph two[\"Inner\"]
                c
              end
            end
            subgraph Plain title
              d
            end
            subgraph three
              e
            end
            x --> a
            x --> three",
        );
        assert_eq!(f.clusters.len(), 4);
        assert_eq!(
            (f.clusters[0].id.as_str(), f.clusters[0].title.as_str()),
            ("one", "First group")
        );
        assert_eq!(
            (f.clusters[1].title.as_str(), f.clusters[1].parent),
            ("Inner", Some(0))
        );
        assert_eq!(f.clusters[2].title, "Plain title");
        assert_eq!(node(&f, "a").cluster, Some(0));
        assert_eq!(node(&f, "c").cluster, Some(1));
        assert_eq!(node(&f, "d").cluster, Some(2));
        assert_eq!(node(&f, "x").cluster, None);
        // An edge to a subgraph id lands on its first member, not a new node.
        assert!(f.nodes.iter().all(|n| n.id != "three"));
        let e = node_index(&f, "e");
        assert!(f.edges.iter().any(|edge| edge.to == e));
    }

    #[test]
    fn ignores_styling_statements_and_marks_accents() {
        let f = flow(
            "graph TD
            %% a comment
            classDef hot fill:#f96
            A:::hot --> B
            B --> C
            class C hot
            style B fill:#bbf,stroke:#333
            click A callback
            linkStyle 0 stroke:#f00
            accTitle: Accessible",
        );
        assert_eq!(f.nodes.len(), 3);
        assert_eq!(f.edges.len(), 2);
        assert!(f.nodes.iter().all(|n| n.tone == Tone::Accent));
        let plain = flow("graph TD\nA --> B");
        assert!(plain.nodes.iter().all(|n| n.tone == Tone::Normal));
    }

    #[test]
    fn layouts_satisfy_invariants() {
        for source in FLOW_SAMPLES {
            assert_flow_invariants(&layout(source));
        }
        assert_in_bounds(&layout(SEQUENCE_SAMPLE));
    }

    #[test]
    fn directions_orient_the_flow() {
        let centers = |source: &str| -> Vec<(f32, f32)> {
            let scene = layout(source);
            ["A", "B", "C"]
                .iter()
                .map(|id| {
                    let s = shape_with_label(&scene, id);
                    (s.x + s.width / 2.0, s.y + s.height / 2.0)
                })
                .collect()
        };
        let lr = centers("graph LR\nA --> B --> C");
        assert!(lr[0].0 < lr[1].0 && lr[1].0 < lr[2].0, "{lr:?}");
        assert!(lr.iter().all(|c| (c.1 - lr[0].1).abs() < 0.5), "{lr:?}");
        let rl = centers("graph RL\nA --> B --> C");
        assert!(rl[0].0 > rl[1].0 && rl[1].0 > rl[2].0, "{rl:?}");
        let td = centers("graph TD\nA --> B --> C");
        assert!(td[0].1 < td[1].1 && td[1].1 < td[2].1, "{td:?}");
        assert!(td.iter().all(|c| (c.0 - td[0].0).abs() < 0.5), "{td:?}");
        let bt = centers("graph BT\nA --> B --> C");
        assert!(bt[0].1 > bt[1].1 && bt[1].1 > bt[2].1, "{bt:?}");
    }

    #[test]
    fn horizontal_layouts_keep_boxes_upright() {
        let scene = layout("graph LR\nA[A rather long label] --> B");
        let a = shape_with_label(&scene, "A rather long label");
        assert!(a.width > a.height * 2.0, "{a:?}");
    }

    #[test]
    fn nodes_in_a_layer_do_not_overlap() {
        let mut source = String::from("graph TD\nroot");
        for i in 0..12 {
            source.push_str(&format!("\nroot --> n{i}[Node number {i}]"));
        }
        let scene = layout(&source);
        assert_flow_invariants(&scene);
        let mut row: Vec<&Shape> = scene
            .shapes
            .iter()
            .filter(|s| {
                scene.labels.iter().any(|l| {
                    l.text.starts_with("Node number") && (s.y + s.height / 2.0 - l.y).abs() < 0.5
                })
            })
            .collect();
        assert_eq!(row.len(), 12);
        row.sort_by(|a, b| a.x.total_cmp(&b.x));
        for pair in row.windows(2) {
            assert!(
                pair[0].x + pair[0].width + NODE_GAP - 0.01 <= pair[1].x,
                "{pair:?}"
            );
        }
    }

    #[test]
    fn edges_touch_node_outlines() {
        let scene = layout("graph TD\nA{Decide} --> B((Round))\nA --> C[Box]\nC --> B\nB --> A");
        assert_flow_invariants(&scene);
        let diamond = scene
            .shapes
            .iter()
            .find(|s| s.kind == ShapeKind::Diamond)
            .unwrap();
        let touching = scene
            .lines
            .iter()
            .filter(|l| {
                outline_distance(diamond, l.points[0]) <= 1.0
                    || outline_distance(diamond, *l.points.last().unwrap()) <= 1.0
            })
            .count();
        assert_eq!(touching, 3);
    }

    #[test]
    fn clusters_contain_members_and_exclude_others() {
        let scene = layout(FLOW_SAMPLES[2]);
        let clusters: Vec<&Shape> = scene
            .shapes
            .iter()
            .filter(|s| s.kind == ShapeKind::Cluster)
            .collect();
        assert_eq!(clusters.len(), 2);
        let contains = |outer: &Shape, inner: &Shape| {
            outer.x <= inner.x
                && outer.y <= inner.y
                && inner.x + inner.width <= outer.x + outer.width
                && inner.y + inner.height <= outer.y + outer.height
        };
        let client = clusters
            .iter()
            .find(|c| contains(c, shape_with_label(&scene, "GPUI view")))
            .unwrap();
        let daemon = clusters
            .iter()
            .find(|c| contains(c, shape_with_label(&scene, "Terminals")))
            .unwrap();
        assert!(contains(
            client,
            shape_with_label(&scene, "Connection bridge")
        ));
        assert!(contains(daemon, shape_with_label(&scene, "Socket worker")));
        assert!(!overlaps(client, daemon));
        let pty = shape_with_label(&scene, "PTY");
        assert!(!overlaps(client, pty) && !overlaps(daemon, pty));
        // Titles sit inside the top of their cluster.
        let title = scene.labels.iter().find(|l| l.text == "Client").unwrap();
        assert!(title.y > client.y && title.y < client.y + 24.0);

        let nested = layout(FLOW_SAMPLES[5]);
        let boxes: Vec<&Shape> = nested
            .shapes
            .iter()
            .filter(|s| s.kind == ShapeKind::Cluster)
            .collect();
        assert_eq!(boxes.len(), 2);
        assert!(contains(boxes[0], boxes[1]));
        assert!(contains(boxes[1], shape_with_label(&nested, "Kernel")));
        assert!(!contains(boxes[1], shape_with_label(&nested, "Services")));
        assert!(!overlaps(boxes[0], shape_with_label(&nested, "App")));
    }

    #[test]
    fn edge_labels_get_backdrops() {
        let scene = layout(FLOW_SAMPLES[0]);
        let yes = scene.labels.iter().find(|l| l.text == "yes").unwrap();
        assert!(yes.backdrop && yes.font == Font::Edge);
        let node = scene.labels.iter().find(|l| l.text == "Answer").unwrap();
        assert!(!node.backdrop && node.font == Font::Node);
        // Labels never sit on top of a node.
        for label in scene.labels.iter().filter(|l| l.backdrop) {
            for shape in scene.shapes.iter().filter(|s| is_node(s)) {
                let inside = label.x > shape.x
                    && label.x < shape.x + shape.width
                    && label.y > shape.y
                    && label.y < shape.y + shape.height;
                assert!(!inside, "{label:?} inside {shape:?}");
            }
        }
    }

    #[test]
    fn self_loops_and_cycles_render() {
        let scene = layout("graph TD\nA --> B\nB --> A\nB --> B\nB -->|again| B");
        assert_flow_invariants(&scene);
        assert_eq!(scene.lines.len(), 4);
        assert!(scene.labels.iter().any(|l| l.text == "again" && l.backdrop));
    }

    #[test]
    fn parses_sequence_features() {
        let s = sequence(SEQUENCE_SAMPLE);
        let names: Vec<(&str, &str, bool)> = s
            .participants
            .iter()
            .map(|p| (p.id.as_str(), p.label.as_str(), p.actor))
            .collect();
        assert_eq!(
            names,
            [
                ("U", "User", true),
                ("C", "Chat view", false),
                ("A", "Agent", false),
                ("T", "Tool runner", false)
            ]
        );
        assert!(s.autonumber);
        let messages: Vec<&Message> = s
            .events
            .iter()
            .filter_map(|e| match e {
                SeqEvent::Message(m) => Some(m),
                _ => None,
            })
            .collect();
        assert_eq!(messages.len(), 8);
        assert!(messages[1].activate && messages[7].deactivate);
        assert_eq!((messages[4].from, messages[4].to), (2, 2));
        assert_eq!(
            (messages[6].stroke, messages[6].arrow_end),
            (Stroke::Dotted, Arrow::Cross)
        );
        assert!(s.events.contains(&SeqEvent::BlockStart {
            title: Some("loop: Until done".into()),
            drawn: true
        }));
        assert!(s.events.contains(&SeqEvent::BlockDivider("Failure".into())));
        assert!(s.events.contains(&SeqEvent::Note {
            placement: NotePlacement::Over(1, 2),
            text: "Streaming continues".into()
        }));
        assert!(s.events.contains(&SeqEvent::Note {
            placement: NotePlacement::RightOf(0),
            text: "Reads reply".into()
        }));
        assert_eq!(
            s.events
                .iter()
                .filter(|e| **e == SeqEvent::BlockEnd)
                .count(),
            2
        );
    }

    #[test]
    fn parses_sequence_arrows() {
        let cases = [
            ("A->>B: m", Stroke::Solid, Arrow::None, Arrow::Filled),
            ("A-->>B: m", Stroke::Dotted, Arrow::None, Arrow::Filled),
            ("A->B: m", Stroke::Solid, Arrow::None, Arrow::None),
            ("A-->B: m", Stroke::Dotted, Arrow::None, Arrow::None),
            ("A-xB: m", Stroke::Solid, Arrow::None, Arrow::Cross),
            ("A--xB: m", Stroke::Dotted, Arrow::None, Arrow::Cross),
            ("A-)B: m", Stroke::Solid, Arrow::None, Arrow::Open),
            ("A--)B: m", Stroke::Dotted, Arrow::None, Arrow::Open),
            ("A<<->>B: m", Stroke::Solid, Arrow::Filled, Arrow::Filled),
            (
                "Web server->>My-DB: m",
                Stroke::Solid,
                Arrow::None,
                Arrow::Filled,
            ),
        ];
        for (line, stroke, start, end) in cases {
            let s = sequence(&format!("sequenceDiagram\n{line}"));
            let SeqEvent::Message(m) = &s.events[0] else {
                panic!("{line}");
            };
            assert_eq!(
                (m.stroke, m.arrow_start, m.arrow_end, m.text.as_str()),
                (stroke, start, end, "m"),
                "{line}"
            );
            assert_eq!(s.participants.len(), 2, "{line}");
        }
        let s = sequence(
            "sequenceDiagram\nparticipant B\nparticipant A\nA->>B: hi\nactivate A\ndeactivate A\nNote left of B: n",
        );
        assert_eq!(s.participants[0].id, "B");
        assert!(s.events.contains(&SeqEvent::Activate(1)));
        assert!(s.events.contains(&SeqEvent::Deactivate(1)));
        assert!(s.events.contains(&SeqEvent::Note {
            placement: NotePlacement::LeftOf(0),
            text: "n".into()
        }));
    }

    #[test]
    fn sequence_layout_orders_rows_and_columns() {
        let scene = layout(SEQUENCE_SAMPLE);
        let boxes: Vec<&Shape> = scene
            .shapes
            .iter()
            .filter(|s| s.kind == ShapeKind::Rounded)
            .collect();
        assert_eq!(boxes.len(), 4);
        assert!(boxes.windows(2).all(|p| p[0].x + p[0].width < p[1].x));
        assert_eq!(boxes[0].tone, Tone::Accent);
        assert_eq!(
            scene
                .shapes
                .iter()
                .filter(|s| s.kind == ShapeKind::Lifeline)
                .count(),
            4
        );
        assert_eq!(
            scene
                .shapes
                .iter()
                .filter(|s| s.kind == ShapeKind::Activation)
                .count(),
            1
        );
        assert_eq!(
            scene
                .shapes
                .iter()
                .filter(|s| s.kind == ShapeKind::Note)
                .count(),
            2
        );
        assert_eq!(
            scene
                .shapes
                .iter()
                .filter(|s| s.kind == ShapeKind::Cluster)
                .count(),
            2
        );

        let numbered: Vec<&Label> = scene
            .labels
            .iter()
            .filter(|l| l.text.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .collect();
        assert_eq!(numbered.len(), 8);
        for (i, label) in numbered.iter().enumerate() {
            assert!(label.text.starts_with(&format!("{}. ", i + 1)), "{label:?}");
        }
        assert!(numbered.windows(2).all(|p| p[0].y < p[1].y));
        let message_lines: Vec<&Line> = scene
            .lines
            .iter()
            .filter(|l| l.style != Stroke::Dotted || l.arrow_end != Arrow::None)
            .collect();
        let starts: Vec<f32> = message_lines.iter().map(|l| l.points[0].1).collect();
        assert!(starts.windows(2).all(|p| p[0] < p[1]), "{starts:?}");
        // A message runs between its participants' lifelines.
        let first = &message_lines[0];
        let (u, c) = (boxes[0], boxes[1]);
        assert!((first.points[0].0 - (u.x + u.width / 2.0)).abs() < 0.5);
        assert!((first.points[1].0 - (c.x + c.width / 2.0)).abs() < 0.5);
        // Blocks cover the lifelines of the participants they involve.
        let loop_box = scene
            .shapes
            .iter()
            .find(|s| s.kind == ShapeKind::Cluster && s.x > c.x + c.width / 2.0)
            .unwrap();
        let tool = boxes[3];
        assert!(loop_box.x + loop_box.width > tool.x + tool.width / 2.0);
    }

    #[test]
    fn nested_blocks_paint_outer_first_and_contain_inner() {
        let scene = layout(
            "sequenceDiagram
            par Write
                A->>B: frame
            and Read
                B-->>A: patch
                opt stale
                    A-xA: drop
                end
            end",
        );
        let blocks: Vec<&Shape> = scene
            .shapes
            .iter()
            .filter(|s| s.kind == ShapeKind::Cluster)
            .collect();
        assert_eq!(blocks.len(), 2);
        let (outer, inner) = (blocks[0], blocks[1]);
        assert!(outer.x < inner.x && outer.y < inner.y);
        assert!(
            inner.x + inner.width < outer.x + outer.width
                && inner.y + inner.height < outer.y + outer.height
        );
        assert!(scene.labels.iter().any(|l| l.text == "par: Write"));
        assert!(scene.labels.iter().any(|l| l.text == "[Read]"));
        assert!(
            scene
                .lines
                .iter()
                .any(|l| l.style == Stroke::Dotted && l.arrow_end == Arrow::None)
        );
        // `box` groups are not drawn; unclosed blocks still close.
        let boxed =
            layout("sequenceDiagram\nbox Aqua Team\nparticipant A\nend\nloop forever\nA->>A: spin");
        assert_eq!(
            boxed
                .shapes
                .iter()
                .filter(|s| s.kind == ShapeKind::Cluster)
                .count(),
            1
        );
    }

    #[test]
    fn wide_message_labels_widen_columns() {
        let narrow = layout("sequenceDiagram\nA->>B: hi");
        let wide = layout(&format!("sequenceDiagram\nA->>B: {}", "long ".repeat(30)));
        let gap = |scene: &Scene| {
            let lifelines: Vec<f32> = scene
                .shapes
                .iter()
                .filter(|s| s.kind == ShapeKind::Lifeline)
                .map(|s| s.x)
                .collect();
            lifelines[1] - lifelines[0]
        };
        assert!(gap(&wide) > gap(&narrow) + 100.0);
        let label = wide
            .labels
            .iter()
            .find(|l| l.text.starts_with("long"))
            .unwrap();
        let width = measure(&label.text, Font::Edge).0;
        let lifelines: Vec<f32> = wide
            .shapes
            .iter()
            .filter(|s| s.kind == ShapeKind::Lifeline)
            .map(|s| s.x)
            .collect();
        assert!(label.x - width / 2.0 >= lifelines[0] && label.x + width / 2.0 <= lifelines[1]);
    }

    #[test]
    fn enforces_size_caps() {
        let mut nodes = String::from("graph TD");
        for i in 0..=MAX_NODES {
            nodes.push_str(&format!("\nn{i}"));
        }
        assert!(parse(&nodes).is_none());
        let mut edges = String::from("graph TD");
        for _ in 0..=MAX_EDGES {
            edges.push_str("\nA --> B");
        }
        assert!(parse(&edges).is_none());
        let mut messages = String::from("sequenceDiagram");
        for _ in 0..=MAX_MESSAGES {
            messages.push_str("\nA->>B: hi");
        }
        assert!(parse(&messages).is_none());
        let mut ok = String::from("graph TD");
        for i in 0..MAX_NODES {
            ok.push_str(&format!("\nn{i} --> n{}", (i * 7 + 3) % MAX_NODES));
        }
        let scene = layout(&ok);
        assert!(scene.width.is_finite() && scene.height.is_finite());
        assert!(parse(&"graph TD\nA-->B\n".repeat(MAX_SOURCE_BYTES / 8)).is_none());
    }

    #[test]
    fn layout_is_deterministic() {
        for source in FLOW_SAMPLES.iter().chain([&SEQUENCE_SAMPLE]) {
            assert_eq!(layout(source), layout(source));
        }
    }

    #[test]
    fn survives_hostile_measurements() {
        let hostile = |text: &str, _font: Font| match text.len() % 3 {
            0 => (f32::NAN, f32::INFINITY),
            1 => (-5.0, -1.0),
            _ => (1e30, 0.0),
        };
        for source in FLOW_SAMPLES.iter().chain([&SEQUENCE_SAMPLE]) {
            let scene = parse(source).unwrap().layout(hostile);
            assert!(scene.width.is_finite() && scene.height.is_finite());
            assert!(
                scene
                    .shapes
                    .iter()
                    .all(|s| s.x.is_finite() && s.y.is_finite())
            );
        }
    }

    #[test]
    fn truncated_prefixes_never_panic() {
        for source in FLOW_SAMPLES.iter().chain([&SEQUENCE_SAMPLE]) {
            let mut ends: Vec<usize> = source.char_indices().map(|(i, _)| i).collect();
            ends.push(source.len());
            for end in ends {
                if let Some(diagram) = parse(&source[..end]) {
                    let scene = diagram.layout(measure);
                    assert!(scene.width.is_finite() && scene.height.is_finite());
                }
            }
        }
    }

    #[test]
    fn random_sources_never_panic() {
        const TOKENS: [&str; 48] = [
            "graph TD\n",
            "flowchart LR\n",
            "sequenceDiagram\n",
            "A",
            "B",
            "c1",
            "é",
            "日本",
            "-->",
            "---",
            "-.->",
            "==>",
            "--o",
            "--x",
            "<-->",
            "~~~",
            "--",
            "-.",
            "|",
            "\"",
            "`",
            "[",
            "]",
            "(",
            ")",
            "{",
            "}",
            ">",
            "&",
            ";",
            ":",
            ":::",
            "\n",
            " ",
            "subgraph ",
            "end\n",
            "->>",
            "-->>",
            "-x",
            "-)",
            "+",
            "-",
            "Note over A,B: ",
            "loop ",
            "alt x\n",
            "else\n",
            "#quot;",
            "<br>",
        ];
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..3_000 {
            let len = (next() % 40) as usize;
            let source: String = (0..len)
                .map(|_| TOKENS[(next() % TOKENS.len() as u64) as usize])
                .collect();
            if let Some(diagram) = parse(&source) {
                let scene = diagram.layout(measure);
                assert!(
                    scene.width.is_finite() && scene.height.is_finite(),
                    "{source:?}"
                );
            }
        }
    }
}
