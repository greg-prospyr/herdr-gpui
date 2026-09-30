//! An agent's Markdown as blocks the chat view lays out: paragraphs of styled
//! spans, headings, lists, quotes, tables, code, diagrams, and math. Raw HTML
//! is shown as text, never interpreted, and images show their description:
//! the chat never fetches what a message points at.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// Blocks kept of one message; the rest are summarized as omitted.
const MAX_BLOCKS: usize = 400;
/// Nesting kept for lists and quotes; deeper content joins its parent.
const MAX_DEPTH: usize = 8;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Block {
    Paragraph(Vec<Span>),
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    Code {
        /// The fence's first word, lowercase: `rust`, `diff`, `mermaid`.
        language: String,
        code: String,
    },
    /// Math set apart from text: `$$…$$`, a ```math fence, or a paragraph
    /// that is only an inline `$…$` formula (typeset in inline style).
    Math {
        tex: String,
        display: bool,
    },
    List {
        /// The first number of an ordered list.
        start: Option<u64>,
        items: Vec<ListItem>,
    },
    Quote(Vec<Block>),
    Table {
        alignments: Vec<Align>,
        head: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
    },
    Rule,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ListItem {
    /// `Some(done)` for a task-list item.
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Span {
    pub text: String,
    pub style: SpanStyle,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SpanStyle {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    /// Inline math, shown with Unicode symbols in the flow of the text.
    pub math: bool,
    pub link: Option<String>,
}

enum Frame {
    Blocks(Vec<Block>),
    Quote(Vec<Block>),
    List {
        start: Option<u64>,
        items: Vec<ListItem>,
    },
    Item {
        task: Option<bool>,
        blocks: Vec<Block>,
    },
}

#[derive(Default)]
struct Table {
    alignments: Vec<Align>,
    head: Vec<Vec<Span>>,
    rows: Vec<Vec<Vec<Span>>>,
    in_head: bool,
}

struct Builder {
    stack: Vec<Frame>,
    inline: Vec<Span>,
    bold: u32,
    italic: u32,
    strike: u32,
    links: Vec<String>,
    heading: Option<u8>,
    /// The TeX of an inline formula that began the paragraph being collected.
    lone_math: Option<String>,
    code: Option<(String, String)>,
    table: Option<Table>,
    /// Container frames skipped for depth; their ends are skipped too.
    flattened: u32,
    blocks: usize,
}

/// Parses `source` as CommonMark with tables, task lists, strikethrough,
/// and `$` math.
pub(crate) fn parse(source: &str) -> Vec<Block> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH
        | Options::ENABLE_GFM;
    let mut builder = Builder {
        stack: vec![Frame::Blocks(Vec::new())],
        inline: Vec::new(),
        bold: 0,
        italic: 0,
        strike: 0,
        links: Vec::new(),
        heading: None,
        lone_math: None,
        code: None,
        table: None,
        flattened: 0,
        blocks: 0,
    };
    for event in Parser::new_ext(source, options) {
        builder.event(event);
    }
    builder.finish()
}

impl Builder {
    fn style(&self, code: bool, math: bool) -> SpanStyle {
        SpanStyle {
            bold: self.bold > 0,
            italic: self.italic > 0,
            strike: self.strike > 0,
            code,
            math,
            link: self.links.last().cloned(),
        }
    }

    fn text(&mut self, text: &str, code: bool, math: bool) {
        if let Some((_, buffer)) = &mut self.code {
            buffer.push_str(text);
            return;
        }
        let style = self.style(code, math);
        match self.inline.last_mut() {
            Some(last) if last.style == style => last.text.push_str(text),
            _ => self.inline.push(Span {
                text: text.to_owned(),
                style,
            }),
        }
    }

    fn depth(&self) -> usize {
        self.stack.len()
    }

    fn push_block(&mut self, block: Block) {
        if self.blocks >= MAX_BLOCKS {
            return;
        }
        self.blocks += 1;
        let blocks = match self.stack.last_mut() {
            Some(Frame::Blocks(blocks) | Frame::Quote(blocks) | Frame::Item { blocks, .. }) => {
                blocks
            }
            // Content directly in a list (not in an item) joins a new item.
            Some(Frame::List { items, .. }) => {
                if items.is_empty() {
                    items.push(ListItem {
                        task: None,
                        blocks: Vec::new(),
                    });
                }
                match items.last_mut() {
                    Some(item) => &mut item.blocks,
                    None => return,
                }
            }
            None => return,
        };
        blocks.push(block);
    }

    /// Ends the paragraph being collected, if any.
    fn flush(&mut self) {
        let spans = trim_spans(std::mem::take(&mut self.inline));
        let lone_math = self.lone_math.take();
        if spans.is_empty() {
            return;
        }
        // A paragraph that is one formula and nothing else is typeset.
        if let (Some(tex), [span], None) = (lone_math, spans.as_slice(), self.heading)
            && span.style.math
        {
            self.push_block(Block::Math {
                tex,
                display: false,
            });
            return;
        }
        let block = match self.heading {
            Some(level) => Block::Heading { level, spans },
            None => Block::Paragraph(spans),
        };
        self.push_block(block);
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text, false, false),
            Event::Code(text) => self.text(&text, true, false),
            Event::InlineMath(text) => {
                self.lone_math = self.inline.is_empty().then(|| text.to_string());
                self.text(&unicode_math(&text), false, true);
            }
            Event::DisplayMath(text) => {
                self.flush();
                self.push_block(Block::Math {
                    tex: text.trim().to_owned(),
                    display: true,
                });
            }
            // Shown as written: markup in a message is text, not a page.
            Event::Html(text) | Event::InlineHtml(text) => self.text(&text, false, false),
            Event::SoftBreak => self.text(" ", false, false),
            Event::HardBreak => self.text("\n", false, false),
            Event::Rule => {
                self.flush();
                self.push_block(Block::Rule);
            }
            Event::TaskListMarker(done) => {
                if let Some(Frame::Item { task, .. }) = self.stack.last_mut() {
                    *task = Some(done);
                }
            }
            Event::FootnoteReference(label) => self.text(&format!("[{label}]"), false, false),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.flush(),
            Tag::Heading { level, .. } => {
                self.flush();
                self.heading = Some(level as u8);
            }
            Tag::BlockQuote(_) => {
                self.flush();
                if self.depth() >= MAX_DEPTH {
                    self.flattened += 1;
                } else {
                    self.stack.push(Frame::Quote(Vec::new()));
                }
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                let language = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == '{')
                        .next()
                        .unwrap_or("")
                        .to_ascii_lowercase(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((language, String::new()));
            }
            Tag::List(start) => {
                self.flush();
                if self.depth() >= MAX_DEPTH {
                    self.flattened += 1;
                } else {
                    self.stack.push(Frame::List {
                        start,
                        items: Vec::new(),
                    });
                }
            }
            Tag::Item => {
                self.flush();
                if matches!(self.stack.last(), Some(Frame::List { .. })) {
                    self.stack.push(Frame::Item {
                        task: None,
                        blocks: Vec::new(),
                    });
                } else {
                    self.flattened += 1;
                }
            }
            Tag::Table(alignments) => {
                self.flush();
                self.table = Some(Table {
                    alignments: alignments
                        .into_iter()
                        .map(|alignment| match alignment {
                            Alignment::Center => Align::Center,
                            Alignment::Right => Align::Right,
                            Alignment::Left | Alignment::None => Align::Left,
                        })
                        .collect(),
                    ..Table::default()
                });
            }
            Tag::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_head = true;
                }
            }
            Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(Vec::new());
                }
            }
            Tag::TableCell => self.inline.clear(),
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => self.links.push(dest_url.into_string()),
            Tag::Image { dest_url, .. } => {
                // The description follows as text; the picture is not fetched.
                self.links.push(dest_url.into_string());
                self.text("🖼 ", false, false);
            }
            Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_)
            | Tag::Superscript
            | Tag::Subscript => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Heading(_) => {
                self.flush();
                self.heading = None;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                if self.flattened > 0 {
                    self.flattened -= 1;
                } else if let Some(Frame::Quote(blocks)) = self.stack.pop() {
                    self.push_block(Block::Quote(blocks));
                }
            }
            TagEnd::CodeBlock => {
                if let Some((language, code)) = self.code.take() {
                    let code = code.strip_suffix('\n').unwrap_or(&code).to_owned();
                    let block = if language == "math" || language == "latex" && is_formula(&code) {
                        Block::Math {
                            tex: code,
                            display: true,
                        }
                    } else {
                        Block::Code { language, code }
                    };
                    self.push_block(block);
                }
            }
            TagEnd::List(_) => {
                self.flush();
                if self.flattened > 0 {
                    self.flattened -= 1;
                } else if let Some(Frame::List { start, items }) = self.stack.pop() {
                    self.push_block(Block::List { start, items });
                }
            }
            TagEnd::Item => {
                self.flush();
                if matches!(self.stack.last(), Some(Frame::Item { .. })) {
                    if let Some(Frame::Item { task, blocks }) = self.stack.pop()
                        && let Some(Frame::List { items, .. }) = self.stack.last_mut()
                    {
                        items.push(ListItem { task, blocks });
                    }
                } else {
                    self.flattened = self.flattened.saturating_sub(1);
                }
            }
            TagEnd::TableCell => {
                let cell = trim_spans(std::mem::take(&mut self.inline));
                if let Some(table) = &mut self.table {
                    if table.in_head {
                        table.head.push(cell);
                    } else if let Some(row) = table.rows.last_mut() {
                        row.push(cell);
                    }
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_head = false;
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.push_block(Block::Table {
                        alignments: table.alignments,
                        head: table.head,
                        rows: table.rows,
                    });
                }
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link | TagEnd::Image => {
                self.links.pop();
            }
            _ => {}
        }
    }

    fn finish(mut self) -> Vec<Block> {
        self.flush();
        if let Some((language, code)) = self.code.take() {
            self.push_block(Block::Code { language, code });
        }
        // Close whatever an unfinished message left open.
        while self.stack.len() > 1 {
            match self.stack.pop() {
                Some(Frame::Quote(blocks)) => self.push_block(Block::Quote(blocks)),
                Some(Frame::List { start, items }) => self.push_block(Block::List { start, items }),
                Some(Frame::Item { task, blocks }) => {
                    if let Some(Frame::List { items, .. }) = self.stack.last_mut() {
                        items.push(ListItem { task, blocks });
                    }
                }
                Some(Frame::Blocks(blocks)) => {
                    for block in blocks {
                        self.push_block(block);
                    }
                }
                None => break,
            }
        }
        match self.stack.pop() {
            Some(Frame::Blocks(blocks)) => blocks,
            _ => Vec::new(),
        }
    }
}

fn trim_spans(mut spans: Vec<Span>) -> Vec<Span> {
    if let Some(first) = spans.first_mut() {
        first.text = first.text.trim_start().to_owned();
    }
    if let Some(last) = spans.last_mut() {
        last.text = last.text.trim_end().to_owned();
    }
    spans.retain(|span| !span.text.is_empty());
    spans
}

/// Whether a ```latex fence holds a formula rather than a document.
fn is_formula(code: &str) -> bool {
    !code.contains("\\documentclass") && !code.contains("\\begin{document}")
}

/// Inline TeX in Unicode, so it can flow with the text around it: Greek and
/// common symbols by name, and digit or letter scripts as Unicode scripts
/// where they exist. Anything else stays as written.
pub(crate) fn unicode_math(tex: &str) -> String {
    let mut out = String::new();
    let mut chars = tex.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                let mut name = String::new();
                while let Some(&next) = chars.peek() {
                    if next.is_ascii_alphabetic() {
                        name.push(next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if name.is_empty() {
                    // `\,` and friends are spacing; `\{` is a brace.
                    match chars.next() {
                        Some(',' | ';' | ':' | '!' | ' ') => out.push('\u{2009}'),
                        Some(other) => out.push(other),
                        None => {}
                    }
                    continue;
                }
                match symbol(&name) {
                    Some(symbol) => out.push_str(symbol),
                    None if matches!(
                        name.as_str(),
                        "mathrm"
                            | "mathbf"
                            | "mathit"
                            | "text"
                            | "operatorname"
                            | "left"
                            | "right"
                            | "big"
                            | "Big"
                            | "displaystyle"
                    ) => {}
                    None => {
                        out.push_str(&name);
                    }
                }
            }
            '^' | '_' => {
                let group = take_group(&mut chars);
                let converted: Option<String> = group
                    .chars()
                    .map(|c| {
                        if ch == '^' {
                            superscript(c)
                        } else {
                            subscript(c)
                        }
                    })
                    .collect();
                match converted {
                    Some(text) => out.push_str(&text),
                    None => {
                        out.push(ch);
                        out.push_str(&unicode_math(&group));
                    }
                }
            }
            '{' | '}' => {}
            _ => out.push(ch),
        }
    }
    out
}

fn take_group(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    match chars.next() {
        Some('{') => {
            let mut depth = 1;
            let mut group = String::new();
            for ch in chars.by_ref() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                group.push(ch);
            }
            group
        }
        Some(ch) => ch.to_string(),
        None => String::new(),
    }
}

fn superscript(ch: char) -> Option<char> {
    Some(match ch {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'n' => 'ⁿ',
        'i' => 'ⁱ',
        'T' => 'ᵀ',
        '*' => '*',
        '\'' => '′',
        _ => return None,
    })
}

fn subscript(ch: char) -> Option<char> {
    Some(match ch {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'h' => 'ₕ',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'k' => 'ₖ',
        'l' => 'ₗ',
        'm' => 'ₘ',
        'n' => 'ₙ',
        'o' => 'ₒ',
        'p' => 'ₚ',
        'r' => 'ᵣ',
        's' => 'ₛ',
        't' => 'ₜ',
        'u' => 'ᵤ',
        'v' => 'ᵥ',
        'x' => 'ₓ',
        _ => return None,
    })
}

fn symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" | "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" | "vartheta" => "θ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" => "π",
        "rho" => "ρ",
        "sigma" => "σ",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" | "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        "sum" => "∑",
        "prod" => "∏",
        "int" => "∫",
        "oint" => "∮",
        "partial" => "∂",
        "nabla" => "∇",
        "infty" => "∞",
        "cdot" => "·",
        "times" => "×",
        "div" => "÷",
        "pm" => "±",
        "mp" => "∓",
        "le" | "leq" => "≤",
        "ge" | "geq" => "≥",
        "ne" | "neq" => "≠",
        "approx" => "≈",
        "equiv" => "≡",
        "sim" => "∼",
        "propto" => "∝",
        "to" | "rightarrow" => "→",
        "leftarrow" | "gets" => "←",
        "Rightarrow" | "implies" => "⇒",
        "Leftrightarrow" | "iff" => "⇔",
        "mapsto" => "↦",
        "in" => "∈",
        "notin" => "∉",
        "subset" => "⊂",
        "subseteq" => "⊆",
        "cup" => "∪",
        "cap" => "∩",
        "wedge" | "land" => "∧",
        "vee" | "lor" => "∨",
        "neg" | "lnot" => "¬",
        "forall" => "∀",
        "exists" => "∃",
        "emptyset" | "varnothing" => "∅",
        "ldots" | "dots" => "…",
        "cdots" => "⋯",
        "sqrt" => "√",
        "circ" => "∘",
        "deg" => "°",
        "ell" => "ℓ",
        "hbar" => "ℏ",
        "Re" => "ℜ",
        "Im" => "ℑ",
        "langle" => "⟨",
        "rangle" => "⟩",
        "lceil" => "⌈",
        "rceil" => "⌉",
        "lfloor" => "⌊",
        "rfloor" => "⌋",
        "quad" => "\u{2003}",
        "qquad" => "\u{2003}\u{2003}",
        "sin" => "sin",
        "cos" => "cos",
        "tan" => "tan",
        "log" => "log",
        "ln" => "ln",
        "exp" => "exp",
        "lim" => "lim",
        "max" => "max",
        "min" => "min",
        "det" => "det",
        "gcd" => "gcd",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn plain(text: &str) -> Span {
        Span {
            text: text.into(),
            style: SpanStyle::default(),
        }
    }

    #[test]
    fn paragraphs_carry_inline_styles() {
        let blocks = parse("Run **`just ci`** and *read* [the docs](https://x.dev).");
        let Block::Paragraph(spans) = &blocks[0] else {
            panic!("{blocks:?}")
        };
        assert_eq!(spans[0], plain("Run "));
        assert!(spans[1].style.bold && spans[1].style.code && spans[1].text == "just ci");
        assert!(spans[3].style.italic);
        assert_eq!(spans[5].style.link.as_deref(), Some("https://x.dev"));
        assert_eq!(spans[5].text, "the docs");
    }

    #[test]
    fn blocks_nest_and_fences_keep_their_language() {
        let source = "# Plan\n\n1. Parse\n   - [x] tokens\n   - [ ] tree\n2. Paint\n\n> quoted **bold**\n\n```Rust ignore\nfn main() {}\n```\n\n```mermaid\ngraph TD; A-->B\n```\n\n$$\n\\sum_i x_i\n$$\n\n---\n";
        let blocks = parse(source);
        assert_eq!(
            blocks[0],
            Block::Heading {
                level: 1,
                spans: vec![plain("Plan")]
            }
        );
        let Block::List { start, items } = &blocks[1] else {
            panic!("{blocks:?}")
        };
        assert_eq!(*start, Some(1));
        assert_eq!(items.len(), 2);
        let Block::List { items: nested, .. } = &items[0].blocks[1] else {
            panic!("{items:?}")
        };
        assert_eq!(nested[0].task, Some(true));
        assert_eq!(nested[1].task, Some(false));
        assert!(
            matches!(&blocks[2], Block::Quote(inner) if matches!(&inner[0], Block::Paragraph(_)))
        );
        assert_eq!(
            blocks[3],
            Block::Code {
                language: "rust".into(),
                code: "fn main() {}".into()
            }
        );
        assert!(matches!(&blocks[4], Block::Code { language, .. } if language == "mermaid"));
        assert_eq!(
            blocks[5],
            Block::Math {
                tex: "\\sum_i x_i".into(),
                display: true
            }
        );
        assert_eq!(blocks[6], Block::Rule);
    }

    #[test]
    fn tables_keep_alignment_and_cells() {
        let blocks = parse("| op | key |\n|:--|--:|\n| `add` | w1 |\n| set | **p** |\n");
        let Block::Table {
            alignments,
            head,
            rows,
        } = &blocks[0]
        else {
            panic!("{blocks:?}")
        };
        assert_eq!(alignments, &[Align::Left, Align::Right]);
        assert_eq!(head.len(), 2);
        assert_eq!(rows.len(), 2);
        assert!(rows[0][0][0].style.code);
        assert!(rows[1][1][0].style.bold);
    }

    #[test]
    fn html_is_text_and_unfinished_messages_still_parse() {
        let blocks = parse("<script>alert(1)</script>\n\nhi");
        assert!(blocks.iter().any(|block| matches!(block, Block::Paragraph(spans) if spans.iter().any(|s| s.text.contains("<script>")))));
        // A message cut off mid-stream closes whatever it opened.
        for source in [
            "```rust\nfn",
            "- a\n  - b\n    > c",
            "| a |\n|--|\n| b",
            "**bold",
            "$$\\frac{1",
        ] {
            let _ = parse(source);
        }
        let deep = "> ".repeat(40) + "deep";
        assert!(!parse(&deep).is_empty());
        let many = "para\n\n".repeat(MAX_BLOCKS + 50);
        assert_eq!(parse(&many).len(), MAX_BLOCKS);
    }

    #[test]
    fn inline_math_reads_as_unicode() {
        assert_eq!(unicode_math("x_i^2 + \\alpha \\le \\infty"), "xᵢ² + α ≤ ∞");
        assert_eq!(unicode_math("e^{i\\pi} = -1"), "e^iπ = -1");
        assert_eq!(unicode_math("a_{10}"), "a₁₀");
        assert_eq!(
            parse("$\\frac{a}{b}$"),
            [Block::Math {
                tex: "\\frac{a}{b}".into(),
                display: false
            }],
            "a paragraph that is only a formula is typeset"
        );
        let blocks = parse("where $n^2$ grows");
        let Block::Paragraph(spans) = &blocks[0] else {
            panic!()
        };
        assert!(
            spans
                .iter()
                .any(|span| span.style.math && span.text == "n²")
        );
    }
}
