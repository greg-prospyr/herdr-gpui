//! Conversation items as elements, drawn as Tern draws its native surface
//! (see `tern` for every value). Motion is computed from when an item
//! arrived, so a frame is a pure function of the clock and a finished
//! animation costs nothing.

use super::{
    highlight::{self, Language, LineKind, Token},
    markdown::{self, Align, Block, ListItem, Span},
    mermaid,
    tern::{self, color},
    transcript::{Dialect, Entry, Todo, TodoState, Tool, ToolKind, ToolState},
    view::ChatPanel,
};
use gpui::{prelude::*, *};
use std::{
    collections::HashMap,
    ops::Range,
    rc::Rc,
    time::{Duration, Instant},
};

/// Diff and output lines shown before a card is opened.
const PREVIEW_LINES: usize = 12;
/// Lines an opened card shows.
const OPEN_LINES: usize = 400;
/// Cached layouts kept before the caches start over.
const MAX_CACHED: usize = 256;

/// An item's id and version, and a block's place in it.
pub(super) type BlockKey = (u64, u64, usize);
/// A laid-out diagram, and when it began drawing itself in, if it does.
pub(super) type Diagram = Rc<(mermaid::Kind, mermaid::Scene, Option<Instant>)>;
pub(super) type Formula = Rc<(super::math::Layout, super::math::Metrics)>;
type Tokens = Rc<Vec<Vec<(Range<usize>, Token)>>>;

/// Layouts that cost a text measurement pass, by the block they are of.
#[derive(Default)]
pub(super) struct Caches {
    pub diagrams: HashMap<BlockKey, Option<Diagram>>,
    pub formulas: HashMap<BlockKey, Option<Formula>>,
    pub highlights: HashMap<BlockKey, Tokens>,
}

impl Caches {
    fn trim(&mut self) {
        if self.diagrams.len() > MAX_CACHED {
            self.diagrams.clear();
        }
        if self.formulas.len() > MAX_CACHED {
            self.formulas.clear();
        }
        if self.highlights.len() > MAX_CACHED {
            self.highlights.clear();
        }
    }
}

/// `path` relative to the pane's directory when inside it, and from the
/// home directory otherwise, whole: a card truncates it to fit.
pub(super) fn display_path(path: &str, cwd: Option<&str>) -> String {
    if let Some(rest) = cwd
        .map(|cwd| cwd.trim_end_matches('/'))
        .filter(|cwd| !cwd.is_empty())
        .and_then(|cwd| path.strip_prefix(cwd))
        .and_then(|rest| rest.strip_prefix('/'))
    {
        return rest.to_owned();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    match path.strip_prefix(&home) {
        Some(rest) if !home.is_empty() && rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_owned(),
    }
}

/// `~/…` for paths under the home directory, and at most the last three
/// components otherwise.
pub(super) fn short_path(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let path = match path.strip_prefix(&home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
            format!("~{rest}")
        }
        _ => path.to_owned(),
    };
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() > 4 {
        format!("…/{}", parts[parts.len() - 3..].join("/"))
    } else {
        path
    }
}

/// Tern's syntax colours; `text` is the token, since the receiver words
/// (`this`, `self`) are keywords to Tern though constants to the lexer.
fn token_color(token: Token, text: &str) -> u32 {
    match token {
        Token::Constant if matches!(text, "this" | "self" | "Self" | "super") => tern::MAGENTA,
        Token::Plain | Token::Operator | Token::Punctuation | Token::Property => tern::INK,
        Token::Keyword => tern::MAGENTA,
        Token::Type => tern::YELLOW,
        Token::Function => tern::BLUE,
        Token::String => tern::GREEN,
        Token::Number | Token::Constant => tern::ORANGE,
        Token::Comment => tern::GRAY,
        Token::Tag => tern::RED,
        Token::Attribute | Token::Variable => tern::CYAN,
    }
}

/// Where a block sits in its item, counted depth first, so nested code and
/// diagrams keep their cache entries between frames.
pub(super) struct Place {
    pub item: u64,
    pub version: u64,
    next: usize,
    /// When the item arrived, for items that animate in.
    pub born: Option<Instant>,
}

impl Place {
    pub(super) fn next(&mut self) -> BlockKey {
        self.next += 1;
        (self.item, self.version, self.next)
    }
}

/// Progress from 0 to 1 of an animation of `duration` that starts `delay`
/// after `born`; `None` once it is over, or when nothing animates.
pub(super) fn progress(born: Option<Instant>, delay: Duration, duration: Duration) -> Option<f32> {
    let elapsed = born?.elapsed();
    let Some(running) = elapsed.checked_sub(delay) else {
        return Some(0.);
    };
    (running < duration).then(|| running.as_secs_f32() / duration.as_secs_f32())
}

/// The byte range of `line` that differs from its partner line, compared
/// token by token, as Tern marks `>` against `>=`.
fn changed_range(line: &str, other: &str) -> Option<Range<usize>> {
    fn tokens(text: &str) -> Vec<Range<usize>> {
        let class = |ch: char| {
            if ch.is_alphanumeric() || ch == '_' {
                0
            } else if ch.is_whitespace() {
                1
            } else {
                2
            }
        };
        let mut out: Vec<Range<usize>> = Vec::new();
        let mut last = None;
        for (index, ch) in text.char_indices() {
            let kind = class(ch);
            match out.last_mut() {
                Some(range) if last == Some(kind) => range.end = index + ch.len_utf8(),
                _ => out.push(index..index + ch.len_utf8()),
            }
            last = Some(kind);
        }
        out
    }
    let (ours, theirs) = (tokens(line), tokens(other));
    let same = |a: &Range<usize>, b: &Range<usize>| line[a.clone()] == other[b.clone()];
    let prefix = ours
        .iter()
        .zip(&theirs)
        .take_while(|(a, b)| same(a, b))
        .count();
    let suffix = ours[prefix..]
        .iter()
        .rev()
        .zip(theirs[prefix..].iter().rev())
        .take_while(|(a, b)| same(a, b))
        .count();
    let changed = &ours[prefix..ours.len() - suffix];
    // A line rewritten throughout is shown by its row colour alone.
    let (first, last) = (changed.first()?, changed.last()?);
    (changed.len() * 2 <= ours.len()).then_some(first.start..last.end)
}

/// A parsed diff row.
enum Row<'a> {
    Line {
        kind: LineKind,
        number: Option<u64>,
        text: &'a str,
        mark: Option<Range<usize>>,
    },
    /// A gap between hunks.
    Gap,
}

fn diff_rows(diff: &[String]) -> Vec<Row<'_>> {
    let (mut old, mut new) = (None::<u64>, None::<u64>);
    let mut rows = Vec::with_capacity(diff.len());
    for (index, line) in diff.iter().enumerate() {
        if let Some(header) = line.strip_prefix("@@") {
            let number = |sign: char| {
                header
                    .split_whitespace()
                    .find_map(|part| part.strip_prefix(sign))
                    .and_then(|part| part.split(',').next())
                    .and_then(|start| start.parse::<u64>().ok())
            };
            old = number('-');
            new = number('+');
            if index > 0 {
                rows.push(Row::Gap);
            }
            continue;
        }
        let kind = highlight::diff_line_kind(line);
        let number = match kind {
            LineKind::Removed => {
                let at = old;
                old = old.map(|n| n + 1);
                at
            }
            LineKind::Added => {
                let at = new;
                new = new.map(|n| n + 1);
                at
            }
            _ => {
                let at = new;
                old = old.map(|n| n + 1);
                new = new.map(|n| n + 1);
                at
            }
        };
        rows.push(Row::Line {
            kind,
            number,
            text: line.get(1..).unwrap_or_default(),
            mark: None,
        });
    }
    // Pair each run of removals with the additions right after it.
    let mut index = 0;
    while index < rows.len() {
        let removed = rows[index..]
            .iter()
            .take_while(|row| {
                matches!(
                    row,
                    Row::Line {
                        kind: LineKind::Removed,
                        ..
                    }
                )
            })
            .count();
        let added = rows[index + removed..]
            .iter()
            .take_while(|row| {
                matches!(
                    row,
                    Row::Line {
                        kind: LineKind::Added,
                        ..
                    }
                )
            })
            .count();
        for pair in 0..removed.min(added) {
            let (a, b) = (index + pair, index + removed + pair);
            let (Row::Line { text: old, .. }, Row::Line { text: new, .. }) = (&rows[a], &rows[b])
            else {
                continue;
            };
            let (old_mark, new_mark) = (changed_range(old, new), changed_range(new, old));
            if let Row::Line { mark, .. } = &mut rows[a] {
                *mark = old_mark;
            }
            if let Row::Line { mark, .. } = &mut rows[b] {
                *mark = new_mark;
            }
        }
        index += (removed + added).max(1);
    }
    rows
}

impl ChatPanel {
    /// The pane's font size; Tern states every size in ems of it.
    pub(super) fn em(&self) -> f32 {
        tern::pane_em(self.ui.size)
    }

    /// The feed's font size.
    pub(super) fn feed_em(&self) -> f32 {
        self.em() * 0.95
    }

    pub(super) fn sans(&self) -> Font {
        let mut font = self.ui.font();
        font.family = tern::SANS.into();
        font.fallbacks = Some(FontFallbacks::from_fonts(vec![self.ui.family.clone()]));
        font
    }

    pub(super) fn mono(&self) -> Font {
        let mut font = self.mono.font();
        font.family = tern::MONO.into();
        font.fallbacks = Some(FontFallbacks::from_fonts(vec![self.mono.family.clone()]));
        font
    }

    pub(super) fn render_item(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(item) = self.conversation.items.get(index) else {
            return div().into_any_element();
        };
        let (id, version) = (item.id, item.version);
        let entry = item.entry.clone();
        let born = self.born.get(&id).copied();
        self.caches.trim();
        let mut place = Place {
            item: id,
            version,
            next: 0,
            born,
        };
        let f = self.feed_em();
        let body = match &entry {
            Entry::User(text) => self.user_bubble(text),
            Entry::Assistant(text) => {
                let blocks = self.blocks(id, version, text);
                div()
                    .flex()
                    .flex_col()
                    .gap(px(f * 0.95))
                    .children(
                        blocks
                            .iter()
                            .map(|block| self.block(block, &mut place, window, cx))
                            .collect::<Vec<_>>(),
                    )
                    .into_any_element()
            }
            Entry::Tool(tool) => self.tool(id, tool, window, cx),
            Entry::Todos(todos) => self.todos(todos),
            Entry::Notice(text) => self.notice(text),
        };
        // Tern's `rise`: half an em up into place while fading in.
        let rise = progress(born, Duration::ZERO, tern::RISE).map(tern::ease_out);
        if rise.is_some() {
            window.request_animation_frame();
        }
        let t = rise.unwrap_or(1.);
        div()
            .id(("chat-item", id))
            .w_full()
            .px(px(f * 1.7))
            .pt(px(if index == 0 { f } else { 0. }))
            .pb(px(f * 0.95))
            .child(
                div()
                    .relative()
                    .top(px((1. - t) * f * 0.5))
                    .opacity(t)
                    .child(body),
            )
            .into_any_element()
    }

    fn blocks(&mut self, id: u64, version: u64, text: &str) -> Rc<Vec<Block>> {
        if let Some((parsed, blocks)) = self.parsed.get(&id)
            && *parsed == version
        {
            return blocks.clone();
        }
        let blocks = Rc::new(markdown::parse(text));
        self.parsed.insert(id, (version, blocks.clone()));
        blocks
    }

    fn user_bubble(&self, text: &str) -> AnyElement {
        let size = self.feed_em() * 1.02;
        div()
            .flex()
            .justify_end()
            .child(
                div()
                    .max_w(relative(0.78))
                    .px(px(size * 0.95))
                    .py(px(size * 0.55))
                    .rounded(px(size * 0.9))
                    .bg(color(tern::wash(tern::PERI, 0x1f)))
                    .border_1()
                    .border_color(color(tern::wash(tern::PERI, 0x33)))
                    .text_color(color(tern::BRIGHT))
                    .text_size(px(size))
                    .line_height(px(size * 1.4))
                    .child(text.to_owned()),
            )
            .into_any_element()
    }

    fn notice(&self, text: &str) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .text_size(px(self.feed_em() * 0.86))
            .text_color(color(tern::DIM))
            .child(div().flex_1().h(px(1.)).bg(color(tern::LINE)))
            .child(
                div()
                    .flex_none()
                    .max_w(relative(0.7))
                    .truncate()
                    .child(text.to_owned()),
            )
            .child(div().flex_1().h(px(1.)).bg(color(tern::LINE)))
            .into_any_element()
    }

    fn md_size(&self) -> f32 {
        self.feed_em() * 1.05
    }

    fn block(
        &mut self,
        block: &Block,
        place: &mut Place,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let md = self.md_size();
        match block {
            Block::Paragraph(spans) => {
                self.rich(spans, md, FontWeight::NORMAL, tern::INK, place, window)
            }
            Block::Heading { level, spans } => {
                let size = md
                    * match level {
                        1 => 1.3,
                        2 => 1.16,
                        3 => 1.05,
                        _ => 1.,
                    };
                // Headings in the accents, larger ones in periwinkle.
                let ink = if *level <= 2 { tern::PERI } else { tern::TEAL };
                div()
                    .pt(px(md * 0.3))
                    .child(self.rich(spans, size, FontWeight::SEMIBOLD, ink, place, window))
                    .into_any_element()
            }
            Block::Code { language, code } => {
                if language == "mermaid" {
                    let key = place.next();
                    if let Some(diagram) = self.diagram(key, code, place.born, window) {
                        return self.diagram_card(key, &diagram, code, window, cx);
                    }
                }
                self.code_card(language, code, place.next(), cx)
            }
            Block::Math { tex, display } => {
                let key = place.next();
                match self.formula(key, tex, *display, window) {
                    Some(formula) => {
                        let reveal = progress(place.born, Duration::from_millis(120), tern::INK_IN)
                            .map(tern::ease_out);
                        if reveal.is_some() {
                            window.request_animation_frame();
                        }
                        let f = self.feed_em();
                        div()
                            .id(("chat-math", key.0 * 1000 + key.2 as u64))
                            .w_full()
                            .overflow_x_scroll()
                            .when(*display, |row| row.pt(px(f * 0.2)).pl(px(f * 1.2)))
                            .child(super::diagram::formula(
                                &formula.0,
                                formula.1,
                                &self.ui,
                                reveal.unwrap_or(1.),
                            ))
                            .into_any_element()
                    }
                    None => self.code_card("latex", tex, key, cx),
                }
            }
            Block::List { start, items } => self.list_block(*start, items, place, window, cx),
            Block::Quote(blocks) => div()
                .pl(px(md * 0.8))
                .border_l_2()
                .border_color(color(tern::wash(tern::PERI, 0x66)))
                .text_color(color(tern::INK_2))
                .flex()
                .flex_col()
                .gap(px(md * 0.6))
                .children(
                    blocks
                        .iter()
                        .map(|block| self.block(block, place, window, cx))
                        .collect::<Vec<_>>(),
                )
                .into_any_element(),
            Block::Table {
                alignments,
                head,
                rows,
            } => self.table(alignments, head, rows, place, window),
            Block::Rule => div().h(px(1.)).bg(color(tern::LINE)).into_any_element(),
        }
    }

    /// Styled spans as one wrapping text. Fresh prose arrives in teal and
    /// settles into ink, as Tern's streamed chunks do; links open in the
    /// browser when clicked, and only web links do.
    fn rich(
        &self,
        spans: &[Span],
        size: f32,
        weight: FontWeight,
        base: u32,
        place: &mut Place,
        window: &mut Window,
    ) -> AnyElement {
        let chunk = progress(place.born, Duration::ZERO, tern::CHUNK);
        if chunk.is_some() {
            window.request_animation_frame();
        }
        let ink = match chunk {
            // 0–25%: teal fading in; then teal easing into ink.
            Some(t) if t < 0.25 => tern::blend(tern::TEAL & 0xffffff00, tern::TEAL, t / 0.25),
            Some(t) => tern::blend(tern::TEAL, base, tern::ease_out((t - 0.25) / 0.75)),
            None => base,
        };
        let mut text = String::new();
        let mut runs = Vec::with_capacity(spans.len());
        let mut links: Vec<(Range<usize>, String)> = Vec::new();
        for span in spans {
            let start = text.len();
            text.push_str(&span.text);
            let style = &span.style;
            let mut font = if style.code { self.mono() } else { self.sans() };
            font.weight = if style.bold {
                FontWeight(FontWeight::SEMIBOLD.0.max(weight.0))
            } else {
                weight
            };
            if style.italic || style.math {
                font.style = FontStyle::Italic;
            }
            let web = style
                .link
                .as_deref()
                .filter(|url| url.starts_with("https://") || url.starts_with("http://"));
            let ink = if web.is_some() {
                tern::BLUE
            } else if style.code {
                tern::CODE_INK
            } else if style.bold && chunk.is_none() && base == tern::INK {
                // Bold body text stands out brighter, as well as heavier.
                tern::BRIGHT
            } else {
                ink
            };
            runs.push(TextRun {
                len: span.text.len(),
                font,
                color: color(ink).into(),
                background_color: style.code.then(|| color(tern::CODE_FILL).into()),
                underline: web.map(|_| UnderlineStyle {
                    thickness: px(1.),
                    color: Some(color(0x1f9fdc66).into()),
                    wavy: false,
                }),
                strikethrough: style.strike.then(|| StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(color(tern::INK_2).into()),
                }),
            });
            if let Some(url) = web {
                links.push((start..text.len(), url.to_owned()));
            }
        }
        let styled = StyledText::new(text).with_runs(runs);
        let wrap = |element: AnyElement| {
            div()
                .text_size(px(size))
                .line_height(px(size * 1.5))
                .child(element)
                .into_any_element()
        };
        if links.is_empty() {
            return wrap(styled.into_any_element());
        }
        let key = place.next();
        let (ranges, urls): (Vec<_>, Vec<_>) = links.into_iter().unzip();
        wrap(
            InteractiveText::new(
                ElementId::Name(format!("chat-link-{}-{}", key.0, key.2).into()),
                styled,
            )
            .on_click(ranges, move |index, _, cx| {
                if let Some(url) = urls.get(index) {
                    cx.open_url(url);
                }
            })
            .into_any_element(),
        )
    }

    fn list_block(
        &mut self,
        start: Option<u64>,
        items: &[ListItem],
        place: &mut Place,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let md = self.md_size();
        let rows: Vec<AnyElement> = items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let marker = match (item.task, start) {
                    (Some(done), _) => checkbox(done, md),
                    (None, Some(first)) => div()
                        .text_size(px(md))
                        .line_height(px(md * 1.5))
                        .text_color(color(tern::PERI))
                        .child(format!("{}.", first + index as u64))
                        .into_any_element(),
                    (None, None) => div()
                        .mt(px(md * 0.75 - 2.5))
                        .size(px(5.))
                        .rounded_full()
                        .bg(color(tern::TEAL))
                        .into_any_element(),
                };
                div()
                    .flex()
                    .gap(px(md * 0.6))
                    .child(
                        div()
                            .flex_none()
                            .w(px(md * if start.is_some() { 1.5 } else { 0.9 }))
                            .flex()
                            .justify_end()
                            .child(marker),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(md * 0.4))
                            .children(
                                item.blocks
                                    .iter()
                                    .map(|block| self.block(block, place, window, cx))
                                    .collect::<Vec<_>>(),
                            ),
                    )
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(md * 0.3))
            .children(rows)
            .into_any_element()
    }

    /// A table as Tern sets one: no frame, a stronger rule under the head,
    /// faint rules between rows, and code cells as small teal chips. Rows
    /// arrive one after another, 90 ms apart.
    fn table(
        &self,
        alignments: &[Align],
        head: &[Vec<Span>],
        rows: &[Vec<Vec<Span>>],
        place: &mut Place,
        window: &mut Window,
    ) -> AnyElement {
        let size = self.feed_em() * 0.98;
        let columns = head
            .len()
            .max(rows.iter().map(Vec::len).max().unwrap_or(0))
            .max(1);
        let empty = Vec::new();
        let born = place.born;
        // Columns share the width in proportion to their widest content, as
        // a browser's automatic table layout does; code chips carry padding.
        let widths: Vec<f32> = (0..columns)
            .map(|column| {
                std::iter::once(head)
                    .chain(rows.iter().map(Vec::as_slice))
                    .filter_map(|row| row.get(column))
                    .map(|spans| {
                        spans
                            .iter()
                            .map(|span| {
                                span.text.chars().count() as f32
                                    + if span.style.code { 2. } else { 0. }
                            })
                            .sum::<f32>()
                    })
                    .fold(0., f32::max)
                    + 3.
            })
            .collect();
        let total: f32 = widths.iter().sum();
        let mut lines = Vec::with_capacity(rows.len() + 1);
        for (row_index, row) in std::iter::once(head)
            .chain(rows.iter().map(Vec::as_slice))
            .enumerate()
        {
            let header = row_index == 0;
            let rise = if header {
                None
            } else {
                progress(
                    born,
                    Duration::from_millis(90 * (row_index as u64 - 1).min(8)),
                    Duration::from_millis(460),
                )
                .map(tern::ease_out)
            };
            if rise.is_some() {
                window.request_animation_frame();
            }
            let t = rise.unwrap_or(1.);
            let mut cells = Vec::with_capacity(columns);
            for (column, share) in widths.iter().enumerate() {
                let spans = row.get(column).unwrap_or(&empty);
                let align = alignments.get(column).copied().unwrap_or(Align::Left);
                let content = match spans.as_slice() {
                    [span] if span.style.code => div()
                        .flex()
                        .child(
                            div()
                                .px(px(size * 0.9 * 0.4))
                                .py(px(size * 0.9 * 0.05))
                                .rounded(px(size * 0.9 * 0.3))
                                .bg(color(tern::CODE_FILL))
                                .text_color(color(tern::CODE_INK))
                                .font(self.mono())
                                .text_size(px(size * 0.9))
                                .child(span.text.clone()),
                        )
                        .into_any_element(),
                    _ => {
                        // Cells never replay the prose colour animation.
                        let mut cell_place = Place {
                            item: place.item,
                            version: place.version,
                            next: place.next + 1000 + row_index * columns + column,
                            born: None,
                        };
                        let weight = if header {
                            FontWeight::MEDIUM
                        } else {
                            FontWeight::NORMAL
                        };
                        let ink = if header { tern::INK_2 } else { tern::INK };
                        self.rich(spans, size, weight, ink, &mut cell_place, window)
                    }
                };
                cells.push(
                    div()
                        .flex_none()
                        .w(relative(share / total))
                        .min_w_0()
                        .pr(px(size * 1.4))
                        .when(header, |cell| cell.pb(px(size * 0.35)))
                        .when(!header, |cell| cell.py(px(size * 0.24)))
                        .border_b_1()
                        .border_color(color(if header { tern::TH_RULE } else { tern::TD_RULE }))
                        .text_color(color(if header { tern::INK_2 } else { tern::INK }))
                        .opacity(t)
                        .map(|cell| match align {
                            Align::Left => cell,
                            Align::Center => cell.text_center(),
                            Align::Right => cell.text_right(),
                        })
                        .child(content),
                );
            }
            lines.push(div().flex().children(cells));
        }
        let width = (size * 26.).max(columns as f32 * size * 13.);
        div()
            .id(("chat-table", place.next().2 as u64 + place.item * 1000))
            .w_full()
            .overflow_x_scroll()
            .child(
                div()
                    .w_full()
                    .max_w(px(width))
                    .flex()
                    .flex_col()
                    .children(lines),
            )
            .into_any_element()
    }

    /// The head of a figure: an optional glyph, a name, an optional kind,
    /// and Copy.
    pub(super) fn figure_head(
        &self,
        glyph: Option<&'static str>,
        name: &str,
        kind: Option<&str>,
        copy: (String, u64),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let size = self.feed_em() * 0.86;
        let (text, key) = copy;
        let copied = self.copied.is_some_and(|(copied, _)| copied == key);
        div()
            .flex()
            .items_center()
            .gap(px(size * 0.45))
            .h(px(size * 2.3))
            .px(px(size))
            .border_b_1()
            .border_color(color(tern::FIG_RULE))
            .text_size(px(size))
            .text_color(color(tern::INK_2))
            .whitespace_nowrap()
            .children(glyph.map(|path| {
                svg()
                    .path(path)
                    .size(px(size * 1.05))
                    .flex_none()
                    .text_color(color(tern::TEAL))
            }))
            .child(name.to_owned())
            .children(kind.map(|kind| {
                div()
                    .text_color(color(tern::DIM))
                    .child(format!("· {kind}"))
            }))
            .child(
                div()
                    .id(("chat-copy", key))
                    .ml_auto()
                    .cursor_pointer()
                    .text_color(color(if copied { tern::DONE_INK } else { tern::DIM }))
                    .hover(|s| s.text_color(color(tern::INK_2)))
                    .child(if copied { "Copied" } else { "Copy" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                        this.copied = Some((key, Instant::now()));
                        cx.notify();
                    })),
            )
    }

    pub(super) fn figure(&self) -> Div {
        div()
            .rounded(px(self.feed_em() * 0.7))
            .border_1()
            .border_color(color(tern::FIG_EDGE))
            .bg(color(tern::FIG))
            .overflow_hidden()
    }

    fn code_card(
        &mut self,
        language: &str,
        code: &str,
        key: BlockKey,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let copy_key = key.0 * 1000 + key.2 as u64;
        let kind = Language::from_hint(language);
        let name = if language.is_empty() {
            "text"
        } else {
            language
        };
        let size = self.feed_em() * 0.94;
        let lines: Vec<AnyElement> = if kind == Language::Diff {
            code.lines()
                .map(|line| {
                    let (fill, ink) = match highlight::diff_line_kind(line) {
                        LineKind::Added => (Some(tern::ADD_FILL), tern::GREEN),
                        LineKind::Removed => (Some(tern::DEL_FILL), tern::RED),
                        LineKind::Hunk => (None, tern::CYAN),
                        LineKind::Header => (None, tern::DIM),
                        LineKind::Context => (None, tern::INK),
                    };
                    self.code_line(line, &[], ink, None)
                        .when_some(fill, |row, fill| row.bg(color(fill)))
                        .into_any_element()
                })
                .collect()
        } else {
            let tokens = self
                .caches
                .highlights
                .entry(key)
                .or_insert_with(|| Rc::new(highlight::highlight(kind, code)))
                .clone();
            code.split('\n')
                .zip(tokens.iter())
                .map(|(line, tokens)| {
                    self.code_line(line, tokens, tern::INK, None)
                        .into_any_element()
                })
                .collect()
        };
        self.figure()
            .child(self.figure_head(None, name, None, (code.to_owned(), copy_key), cx))
            .child(
                div()
                    .id(("chat-code", copy_key))
                    .overflow_x_scroll()
                    .pt(px(size * 0.75))
                    .pb(px(size * 0.85))
                    .font(self.mono())
                    .text_size(px(size))
                    .line_height(px(size * 1.6))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_full()
                            .px(px(size * 1.1))
                            .children(lines),
                    ),
            )
            .into_any_element()
    }

    /// One line of code, coloured by its tokens (or all `plain`), with a
    /// changed word marked: its range, fill, and optional ink.
    fn code_line(
        &self,
        line: &str,
        tokens: &[(Range<usize>, Token)],
        plain: u32,
        mark: Option<(Range<usize>, u32, Option<u32>)>,
    ) -> Div {
        let font = self.mono();
        let run = |len: usize, ink: u32, fill: Option<u32>| TextRun {
            len,
            font: font.clone(),
            color: color(ink).into(),
            background_color: fill.map(|fill| color(fill).into()),
            underline: None,
            strikethrough: None,
        };
        if line.is_empty() {
            return div()
                .whitespace_nowrap()
                .child(StyledText::new(" ").with_runs(vec![run(1, plain, None)]));
        }
        // Tabs widen the line and shift every token after them, so a line
        // with tabs is drawn in one colour and unmarked.
        let display = line.replace('\t', "    ");
        let exact = display.len() == line.len();
        let mut tokens: Vec<(Range<usize>, u32)> = if exact {
            tokens
                .iter()
                .filter(|(range, _)| range.start < range.end && range.end <= line.len())
                .map(|(range, token)| (range.clone(), token_color(*token, &line[range.clone()])))
                .collect()
        } else {
            Vec::new()
        };
        if tokens.iter().map(|(range, _)| range.len()).sum::<usize>() != display.len() {
            tokens = vec![(0..display.len(), plain)];
        }
        let mark = mark.filter(|(range, ..)| exact && range.end <= line.len());
        let mut runs = Vec::new();
        for (range, ink) in tokens {
            // Split the token at the mark's edges so the mark gets its runs.
            let mut cuts = vec![range.start, range.end];
            if let Some((marked, ..)) = &mark {
                cuts.extend(
                    [marked.start, marked.end]
                        .into_iter()
                        .filter(|edge| range.start < *edge && *edge < range.end),
                );
            }
            cuts.sort_unstable();
            for piece in cuts.windows(2) {
                let inside = mark
                    .as_ref()
                    .filter(|(marked, ..)| marked.start <= piece[0] && piece[1] <= marked.end);
                runs.push(match inside {
                    Some((_, fill, marked_ink)) => {
                        run(piece[1] - piece[0], marked_ink.unwrap_or(ink), Some(*fill))
                    }
                    None => run(piece[1] - piece[0], ink, None),
                });
            }
        }
        div()
            .whitespace_nowrap()
            .child(StyledText::new(display).with_runs(runs))
    }

    fn tool(
        &mut self,
        id: u64,
        tool: &Tool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (glyph, name, target) = match &tool.kind {
            ToolKind::Edit {
                path,
                diff,
                created,
            } => return self.edit_card(id, tool, path, diff, *created, window, cx),
            ToolKind::Command {
                command,
                description,
            } => (
                "icons/tern-terminal.svg",
                match self.dialect() {
                    Dialect::Claude => "Bash",
                    Dialect::Codex => "Shell",
                }
                .to_owned(),
                description
                    .clone()
                    .filter(|_| command.contains('\n'))
                    .unwrap_or_else(|| command.lines().next().unwrap_or("").to_owned()),
            ),
            ToolKind::Read { path } => (
                "icons/tern-file.svg",
                "Read".to_owned(),
                display_path(path, self.context.cwd.as_deref()),
            ),
            ToolKind::Search { pattern, scope } => (
                "icons/tern-search.svg",
                "Search".to_owned(),
                match scope {
                    Some(scope) => format!("{pattern}  {}", short_path(scope)),
                    None => pattern.clone(),
                },
            ),
            ToolKind::Web { target } => {
                ("icons/tern-globe.svg", "Browser".to_owned(), target.clone())
            }
            ToolKind::Delegate { description } => (
                "icons/tern-delegate.svg",
                "Task".to_owned(),
                description.clone(),
            ),
            ToolKind::Other { name, summary } => {
                ("icons/tern-delegate.svg", name.clone(), summary.clone())
            }
        };
        let open = self.expanded.contains(&id);
        let output = match &tool.kind {
            ToolKind::Command { command, .. } if open => {
                let mut lines: Vec<String> = Vec::new();
                if command.contains('\n') {
                    lines.extend(command.lines().map(|line| format!("$ {line}")));
                }
                lines.extend(tool.output.lines().take(OPEN_LINES).map(str::to_owned));
                if lines.is_empty() {
                    lines.push("(no output)".to_owned());
                }
                Some(lines)
            }
            _ => None,
        };
        let size = self.feed_em() * 0.86;
        self.card(id, cx)
            .child(self.card_head(glyph, &name, &target, None, tool.state, id, window))
            .children(output.map(|lines| {
                div()
                    .id(("chat-output", id))
                    .mt(px(size * 0.7))
                    .overflow_x_scroll()
                    .font(self.mono())
                    .text_size(px(size))
                    .line_height(px(size * 1.6))
                    .text_color(color(tern::INK_2))
                    .children(lines.into_iter().map(|line| {
                        div().whitespace_nowrap().child(if line.is_empty() {
                            " ".to_owned()
                        } else {
                            line.replace('\t', "    ")
                        })
                    }))
            }))
            .into_any_element()
    }

    /// A tool card; it opens and closes on click.
    fn card(&self, id: u64, cx: &mut Context<Self>) -> Stateful<Div> {
        let f = self.feed_em();
        div()
            .id(("chat-card", id))
            .rounded(px(f * 0.7))
            .border_1()
            .border_color(color(tern::CARD_EDGE))
            .bg(color(tern::CARD))
            .px(px(f))
            .py(px(f * 0.75))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.expanded.remove(&id) {
                    this.expanded.insert(id);
                }
                if let Some(index) = this
                    .conversation
                    .items
                    .iter()
                    .position(|item| item.id == id)
                {
                    this.list.splice(index..index + 1, 1);
                }
                cx.notify();
            }))
    }

    #[allow(clippy::too_many_arguments)]
    fn card_head(
        &self,
        glyph: &'static str,
        name: &str,
        target: &str,
        counts: Option<(usize, usize)>,
        state: ToolState,
        id: u64,
        window: &mut Window,
    ) -> impl IntoElement {
        let size = self.feed_em() * 0.98;
        let hue = match glyph {
            "icons/tern-terminal.svg" => tern::GREEN,
            "icons/tern-file.svg" => tern::BLUE,
            "icons/tern-search.svg" => tern::AMBER,
            "icons/tern-globe.svg" => tern::CYAN,
            "icons/tern-edit.svg" => tern::ORANGE,
            _ => tern::PINK,
        };
        div()
            .flex()
            .items_center()
            .gap(px(size * 0.55))
            .text_size(px(size))
            .whitespace_nowrap()
            .child(
                // Each kind of tool in its own colour, on a chip of it.
                div()
                    .flex_none()
                    .size(px(size * 1.6))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(size * 0.4))
                    .bg(color(tern::wash(hue, 0x24)))
                    .child(
                        svg()
                            .path(glyph)
                            .size(px(size * 1.05))
                            .text_color(color(hue)),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(color(tern::INK))
                    .child(name.to_owned()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font(self.mono())
                    .text_size(px(size * 0.92))
                    .text_color(color(tern::INK_2))
                    .child(target.lines().next().unwrap_or("").to_owned()),
            )
            .children(counts.map(|(added, removed)| {
                div()
                    .flex_none()
                    .flex()
                    .gap(px(size * 0.55))
                    .font(self.mono())
                    .text_size(px(size * 0.92))
                    .child(
                        div()
                            .text_color(color(tern::GREEN))
                            .child(format!("+{added}")),
                    )
                    .child(
                        div()
                            .text_color(color(tern::RED))
                            .child(format!("−{removed}")),
                    )
            }))
            .child(self.pill(state, id, window))
    }

    /// `• Running`, `✓ Done`, or `✕ Failed`. Done pops in the moment it
    /// lands; Running's dot breathes.
    fn pill(&self, state: ToolState, id: u64, window: &mut Window) -> impl IntoElement {
        let base = self.feed_em() * 0.98 * 0.82;
        let pop = match state {
            ToolState::Done => progress(self.settled.get(&id).copied(), Duration::ZERO, tern::POP),
            _ => None,
        };
        let pulse = (state == ToolState::Running).then(|| {
            let t = (self.clock.elapsed().as_secs_f32() / tern::PULSE.as_secs_f32()).fract();
            // Opacity 1, then 0.3 at the half, then 1.
            let half = 1. - (t * 2. - 1.).abs();
            1. - 0.7 * tern::ease(half)
        });
        if pop.is_some() || pulse.is_some() {
            window.request_animation_frame();
        }
        // The pill grows from 0.6 of its size with a spring.
        let size = base * pop.map_or(1., |t| 0.6 + 0.4 * tern::pop(t));
        let (fill, edge, ink) = match state {
            ToolState::Running => (tern::RUN_FILL, None, tern::RUN_INK),
            ToolState::Done => (tern::DONE_FILL, Some(tern::DONE_EDGE), tern::DONE_INK),
            ToolState::Failed => (tern::FAIL_FILL, Some(tern::FAIL_EDGE), tern::FAIL_INK),
        };
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(size * 0.4))
            .px(px(size * 0.65))
            .py(px(size * 0.18))
            .rounded_full()
            .bg(color(fill))
            .when_some(edge, |pill, edge| pill.border_1().border_color(color(edge)))
            .text_size(px(size))
            .text_color(color(ink))
            .children(pulse.map(|opacity| {
                div()
                    .size(px(base * 0.45))
                    .rounded_full()
                    .bg(color(ink))
                    .opacity(opacity)
            }))
            .child(match state {
                ToolState::Running => "Running",
                ToolState::Done => "✓ Done",
                ToolState::Failed => "✕ Failed",
            })
    }

    #[allow(clippy::too_many_arguments)]
    fn edit_card(
        &self,
        id: u64,
        tool: &Tool,
        path: &str,
        diff: &[String],
        created: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.expanded.contains(&id);
        let rows = diff_rows(diff);
        let count = |wanted: LineKind| {
            rows.iter()
                .filter(|row| matches!(row, Row::Line { kind, .. } if *kind == wanted))
                .count()
        };
        let (added, removed) = (count(LineKind::Added), count(LineKind::Removed));
        let language = Language::from_hint(path);
        let limit = if open { OPEN_LINES } else { PREVIEW_LINES };
        let hidden = rows.len().saturating_sub(limit);
        let size = self.feed_em() * 0.86;
        let born = self.born.get(&id).copied();
        let body: Vec<AnyElement> = rows
            .iter()
            .take(limit)
            .enumerate()
            .map(|(index, row)| {
                let rise = progress(
                    born,
                    Duration::from_millis(90 * index.min(8) as u64),
                    Duration::from_millis(460),
                )
                .map(tern::ease_out);
                if rise.is_some() {
                    window.request_animation_frame();
                }
                let t = rise.unwrap_or(1.);
                self.diff_row(row, language, size)
                    .relative()
                    .top(px((1. - t) * size * 0.5))
                    .opacity(t)
                    .into_any_element()
            })
            .collect();
        self.card(id, cx)
            .child(self.card_head(
                "icons/tern-edit.svg",
                if created { "Write" } else { "Edit" },
                &display_path(path, self.context.cwd.as_deref()),
                Some((added, removed)),
                tool.state,
                id,
                window,
            ))
            .child(
                div()
                    .id(("chat-diff", id))
                    .mt(px(size * 0.7))
                    .overflow_x_scroll()
                    .font(self.mono())
                    .text_size(px(size))
                    .line_height(px(size * 1.6))
                    .child(div().flex().flex_col().min_w_full().children(body)),
            )
            .when(hidden > 0, |card| {
                card.child(
                    div()
                        .pt(px(size * 0.4))
                        .text_size(px(size))
                        .text_color(color(tern::DIM))
                        .child(format!("{hidden} more lines")),
                )
            })
            .into_any_element()
    }

    /// A diff row on Tern's grid: number, sign, code; removed and added rows
    /// tinted, with a bar at their left edge and their changed word marked.
    fn diff_row(&self, row: &Row<'_>, language: Language, size: f32) -> Div {
        let Row::Line {
            kind,
            number,
            text,
            mark,
        } = row
        else {
            return div()
                .h(px(size * 1.6))
                .flex()
                .items_center()
                .pl(px(size * 2.6))
                .text_color(color(tern::DIM))
                .child("⋯");
        };
        let bar = size * 0.16;
        let (fill, edge, sign, sign_ink, mark_fill, mark_ink) = match kind {
            LineKind::Removed => (
                Some(tern::DEL_FILL),
                Some(tern::DEL_BAR),
                "−",
                tern::RED,
                tern::DEL_MARK,
                None,
            ),
            LineKind::Added => (
                Some(tern::ADD_FILL),
                Some(tern::ADD_BAR),
                "+",
                tern::GREEN,
                tern::ADD_MARK,
                Some(tern::ADD_MARK_INK),
            ),
            _ => (None, None, "", tern::DIM, 0, None),
        };
        let tokens = highlight::highlight(language, text)
            .into_iter()
            .next()
            .unwrap_or_default();
        div()
            .flex()
            .when_some(fill, |row, fill| row.bg(color(fill)))
            .when_some(edge, |row, edge| {
                row.border_l(px(bar)).border_color(color(edge))
            })
            .child(
                div()
                    .flex_none()
                    .w(px(size * 2.6 - if edge.is_some() { bar } else { 0. }))
                    .pr(px(size * 0.6))
                    .text_right()
                    .text_color(color(tern::DIM))
                    .child(number.map(|n| n.to_string()).unwrap_or_default()),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(size * 1.3))
                    .text_color(color(sign_ink))
                    .child(sign),
            )
            .child(
                self.code_line(
                    text,
                    &tokens,
                    tern::INK,
                    mark.clone().map(|range| (range, mark_fill, mark_ink)),
                )
                .flex_1(),
            )
    }

    fn todos(&self, todos: &[Todo]) -> AnyElement {
        let f = self.feed_em();
        let done = todos
            .iter()
            .filter(|todo| todo.state == TodoState::Done)
            .count();
        div()
            .rounded(px(f * 0.7))
            .border_1()
            .border_color(color(tern::CARD_EDGE))
            .bg(color(tern::CARD))
            .px(px(f))
            .py(px(f * 0.75))
            .flex()
            .flex_col()
            .gap(px(f * 0.35))
            .child(
                div()
                    .flex()
                    .gap(px(f * 0.55))
                    .text_size(px(f * 0.98))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(color(tern::INK))
                            .child("Plan"),
                    )
                    .child(
                        div()
                            .font(self.mono())
                            .text_size(px(f * 0.9))
                            .text_color(color(tern::INK_2))
                            .child(format!("{done}/{}", todos.len())),
                    ),
            )
            .children(todos.iter().map(|todo| {
                div()
                    .flex()
                    .items_start()
                    .gap(px(f * 0.55))
                    .text_size(px(f))
                    .child(checkbox(todo.state == TodoState::Done, f))
                    .child(
                        div()
                            .min_w_0()
                            .map(|text| match todo.state {
                                TodoState::Done => text.text_color(color(tern::DIM)).line_through(),
                                TodoState::Active => text
                                    .text_color(color(tern::INK))
                                    .font_weight(FontWeight::MEDIUM),
                                TodoState::Pending => text.text_color(color(tern::INK_2)),
                            })
                            .child(todo.text.clone()),
                    )
            }))
            .into_any_element()
    }
}

fn checkbox(done: bool, size: f32) -> AnyElement {
    div()
        .mt(px(size * 0.25))
        .flex_none()
        .size(px(size * 0.95))
        .rounded(px(size * 0.25))
        .border_1()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(size * 0.7))
        .map(|mark| {
            if done {
                mark.bg(color(tern::DONE_FILL))
                    .border_color(color(tern::DONE_EDGE))
                    .text_color(color(tern::DONE_INK))
                    .child("✓")
            } else {
                mark.border_color(color(tern::NODE_EDGE))
            }
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn diffs_number_their_lines_and_mark_the_changed_word() {
        let diff: Vec<String> = [
            "@@ -41,2 +41,2 @@",
            "   const inFlight = this.sent - this.acked;",
            "-  if (inFlight > CREDITS) return;",
            "+  if (inFlight >= CREDITS) return;",
            "@@ -90,0 +90,1 @@",
            "+z",
        ]
        .map(str::to_owned)
        .to_vec();
        let rows = diff_rows(&diff);
        let lines: Vec<(Option<u64>, Option<&str>)> = rows
            .iter()
            .map(|row| match row {
                Row::Line {
                    number, text, mark, ..
                } => (*number, mark.clone().map(|range| &text[range])),
                Row::Gap => (None, Some("gap")),
            })
            .collect();
        assert_eq!(
            lines,
            [
                (Some(41), None),
                (Some(42), Some(">")),
                (Some(42), Some(">=")),
                (None, Some("gap")),
                (Some(90), None),
            ]
        );
        let bare_diff = ["@@".to_owned(), "-a".to_owned()];
        let bare = diff_rows(&bare_diff);
        assert!(matches!(bare[0], Row::Line { number: None, .. }));
        assert_eq!(changed_range("a b", "x y"), None, "a rewrite is not a word");
    }

    #[test]
    fn animations_report_progress_until_they_finish() {
        assert_eq!(progress(None, Duration::ZERO, tern::RISE), None);
        let now = Instant::now();
        assert!(progress(Some(now), Duration::ZERO, tern::RISE).is_some());
        assert_eq!(
            progress(Some(now), Duration::from_secs(5), tern::RISE),
            Some(0.)
        );
        if let Some(old) = now.checked_sub(Duration::from_secs(10)) {
            assert_eq!(progress(Some(old), Duration::ZERO, tern::RISE), None);
        }
    }
}
