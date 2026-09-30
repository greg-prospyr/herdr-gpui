//! Typesetting for the TeX math subset that AI agents write in chat markdown.
//!
//! [`parse`] turns the source of `$...$` or `$$...$$` into a private node tree and
//! [`Formula::layout`] places it with a simplified version of TeX's box model
//! (atom classes and inter-atom spacing, script shifts, fraction and radical
//! clearances, big-operator limits, extensible delimiters). The output is a flat list
//! of positioned glyph runs, filled rules, and radical check marks in pixels, so the
//! painter needs only the UI font (upright, italic, bold) and a way to fill
//! rectangles and stroke a polyline. No math font is assumed: vertical placement uses
//! per-character ink estimates typical of text faces, clamped to the caller's metrics,
//! and tall parentheses, braces, brackets, and bars are assembled from rules rather
//! than scaled glyphs.
//!
//! # Coordinates
//!
//! A [`Layout`] has its origin at the left end of the baseline and y grows downward,
//! so item positions above the baseline are negative. Every item lies inside
//! `[0, width] x [-ascent, descent]`; for a glyph run this means its whole line box
//! (`baseline - size * metrics.ascent` to `baseline + size * metrics.descent`) and its
//! measured advance.
//!
//! # Radical contract
//!
//! [`Item::Radical`] is only the check mark of a square root; the bar over the
//! radicand is a separate [`Item::Rule`] whose left end meets the check mark's top
//! right corner. Stroke this open three-point polyline, with round joins and a stroke
//! width equal to that bar's height:
//!
//! ```text
//! P0 = (x,             y + h - min(0.45 * h, 0.9 * w))
//! P1 = (x + 0.4 * w,   y + h)
//! P2 = (x + w,         y)
//! ```
//!
//! The box is the stroke's centre line: `y` is the vertical centre of the bar and
//! `y + h` the bottom of the V. Half the stroke width may extend past the box; the
//! layout's width, ascent, and descent already leave room for that overhang.
//!
//! # Limits
//!
//! Sources longer than 4096 characters or nested deeper than 32 groups are rejected,
//! as is input whose structure is broken (unbalanced braces, `\left` without
//! `\right`, double scripts) or that is mostly unknown commands. Unknown commands in
//! otherwise valid input render as their upright name.

use std::mem;

const MAX_SOURCE_CHARS: usize = 4096;
const MAX_DEPTH: usize = 32;

/// Height of the math axis above the baseline, in em: fraction bars and vertically
/// centred operators and delimiters are balanced around it.
const AXIS: f32 = 0.25;
/// Default rule thickness in em for fraction bars, overlines, and radical strokes.
const RULE: f32 = 0.045;
const X_HEIGHT: f32 = 0.45;
/// Bold faces are wider than the regular face the caller measures with.
const BOLD_WIDTH: f32 = 1.05;
/// Delimiters needing more than this glyph scale are drawn from rules, because a
/// uniformly scaled glyph grows unreadably heavy and wide.
const MAX_DELIM_GLYPH_SCALE: f32 = 1.3;
/// Thin, medium, and thick math spaces (3, 4, and 5 mu) in em.
const THIN: f32 = 3.0 / 18.0;
const MEDIUM: f32 = 4.0 / 18.0;
const THICK: f32 = 5.0 / 18.0;

/// A parsed formula, ready to be laid out at any size.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Formula {
    nodes: Vec<Node>,
}

/// Parses TeX math source.
///
/// Returns `None` when the source is too long, too deep, structurally broken, or uses
/// nothing that can be typeset sensibly; the caller then shows the raw TeX.
pub(crate) fn parse(tex: &str) -> Option<Formula> {
    if tex.chars().nth(MAX_SOURCE_CHARS).is_some() {
        return None;
    }
    let mut parser = Parser {
        chars: tex.chars().collect(),
        pos: 0,
        depth: 0,
        font: Font::Math,
        atoms: 0,
        unknown: 0,
    };
    let nodes = parser.parse_top()?;
    if parser.atoms == 0 || parser.unknown * 2 > parser.atoms {
        return None;
    }
    Some(Formula { nodes })
}

/// Whether the formula is set as a display block or inside a line of text.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Style {
    Display,
    Inline,
}

/// Font line metrics per unit of font size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Metrics {
    pub ascent: f32,
    pub descent: f32,
}

/// A laid-out formula in pixels; see the module documentation for coordinates.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Layout {
    pub width: f32,
    pub ascent: f32,
    pub descent: f32,
    pub items: Vec<Item>,
}

/// One positioned drawing primitive of a [`Layout`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Item {
    /// A text run whose left end sits at `(x, baseline)`.
    Glyphs {
        text: String,
        x: f32,
        baseline: f32,
        size: f32,
        italic: bool,
        bold: bool,
    },
    /// A filled rectangle: fraction bars, radical bars, overlines, drawn delimiters.
    Rule {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    },
    /// The check mark of a radical; see the module documentation for the polyline.
    Radical {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    },
}

impl Formula {
    /// Lays the formula out at `size` pixels per em.
    ///
    /// `measure(text, size, italic)` returns the advance width of `text` in the UI
    /// font; bold runs are measured with the regular face and widened slightly.
    pub(crate) fn layout(
        &self,
        style: Style,
        size: f32,
        metrics: Metrics,
        measure: impl Fn(&str, f32, bool) -> f32,
    ) -> Layout {
        if !(size.is_finite() && size > 0.0) {
            return Layout::default();
        }
        let clamp_metric = |value: f32, fallback: f32| {
            if value.is_finite() {
                value.clamp(0.0, 2.0)
            } else {
                fallback
            }
        };
        let engine = Engine {
            measure: &measure,
            ascent: clamp_metric(metrics.ascent, 0.78),
            descent: clamp_metric(metrics.descent, 0.22),
            root: size,
        };
        let level = match style {
            Style::Display => Level::Display,
            Style::Inline => Level::Text,
        };
        let st = St {
            level,
            size,
            cramped: false,
        };
        let root = engine.list(&self.nodes, st, None);
        engine.finish(root)
    }
}

// ---------------------------------------------------------------------------------
// Syntax tree
// ---------------------------------------------------------------------------------

/// TeX atom classes, which decide the spacing between neighbouring atoms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Ord,
    Op,
    Bin,
    Rel,
    Open,
    Close,
    Punct,
    Inner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    Display,
    Text,
    Script,
    ScriptScript,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Font {
    Math,
    Roman,
    Bold,
    Italic,
    BoldItalic,
    DoubleStruck,
    Script,
    Fraktur,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Delim {
    /// `\left.`: takes up the null delimiter space only.
    Null,
    Glyph(char),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Accent {
    Mark(&'static str),
    Vector,
    Bar,
    Over,
    Under,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phantom {
    Full,
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EnvKind {
    Matrix,
    Paren,
    Bracket,
    Brace,
    Vert,
    DoubleVert,
    Small,
    Cases,
    DisplayCases,
    RightCases,
    Aligned,
    Gathered,
    Array,
}

#[derive(Clone, Debug, PartialEq)]
struct Glyph {
    text: String,
    class: Class,
    italic: bool,
    bold: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct OpNode {
    text: String,
    /// A large operator symbol (`\sum`), as opposed to an upright name (`\lim`).
    big: bool,
    integral: bool,
    /// Whether scripts become limits in display style.
    display_limits: bool,
    /// `\limits` / `\nolimits` override.
    force: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
struct Scripts {
    base: Node,
    sup: Option<Node>,
    sub: Option<Node>,
    primes: usize,
}

#[derive(Clone, Debug, PartialEq)]
struct Frac {
    num: Node,
    den: Node,
    bar: bool,
    level: Option<Level>,
    parens: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct Fenced {
    left: Delim,
    right: Delim,
    body: Vec<Node>,
}

#[derive(Clone, Debug, PartialEq)]
struct Stack {
    base: Node,
    over: Option<Node>,
    under: Option<Node>,
    class: Class,
}

#[derive(Clone, Debug, PartialEq)]
struct Array {
    kind: EnvKind,
    cols: Vec<Align>,
    rows: Vec<Vec<Vec<Node>>>,
}

#[derive(Clone, Debug, PartialEq)]
enum Node {
    Glyph(Glyph),
    Group(Vec<Node>),
    Classed(Class, Vec<Node>),
    Op(OpNode),
    Scripts(Box<Scripts>),
    Frac(Box<Frac>),
    Sqrt(Box<Node>, Option<Box<Node>>),
    Accent(Box<Node>, Accent),
    Fenced(Box<Fenced>),
    Middle(Delim),
    /// `\big(` and friends: a delimiter of a fixed total height in em.
    BigDelim(Delim, f32, Class),
    /// Horizontal space in em of the current size.
    Space(f32),
    Array(Box<Array>),
    Stack(Box<Stack>),
    /// `\overbrace` (true) or `\underbrace` (false); scripts attach as limits.
    Brace(Box<Node>, bool),
    /// `\xrightarrow` (false) or `\xleftarrow` (true) with text above and below.
    XArrow(bool, Option<Box<Node>>, Option<Box<Node>>),
    Phantom(Box<Node>, Phantom),
    Style(Level),
    Boxed(Box<Node>),
}

impl Node {
    fn class(&self) -> Class {
        match self {
            Node::Glyph(glyph) => glyph.class,
            Node::Classed(class, _) | Node::BigDelim(_, _, class) => *class,
            Node::Op(_) => Class::Op,
            Node::Scripts(scripts) => scripts.base.class(),
            Node::Frac(_) | Node::Fenced(_) => Class::Inner,
            Node::Stack(stack) => stack.class,
            Node::XArrow(..) => Class::Rel,
            _ => Class::Ord,
        }
    }

    fn glyph(text: &str, class: Class) -> Node {
        Node::Glyph(Glyph {
            text: text.to_owned(),
            class,
            italic: false,
            bold: false,
        })
    }
}

// ---------------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------------

struct Parser {
    chars: Vec<char>,
    pos: usize,
    depth: usize,
    font: Font,
    /// Visible atoms produced, for the "mostly garbage" check.
    atoms: usize,
    unknown: usize,
}

/// Why a list stopped.
enum End {
    Eof,
    Brace,
    Bracket,
    Right(Delim),
    Amp,
    Row,
    Env(String),
}

type Rows = Vec<Vec<Vec<Node>>>;

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        Some(c)
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.pos += 1;
        }
    }

    fn skip_star(&mut self) {
        if self.peek() == Some('*') {
            self.pos += 1;
        }
    }

    fn enter(&mut self) -> Option<()> {
        self.depth += 1;
        (self.depth <= MAX_DEPTH).then_some(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn parse_top(&mut self) -> Option<Vec<Node>> {
        let (mut rows, end) = self.parse_cells()?;
        if !matches!(end, End::Eof) {
            return None;
        }
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        if rows.len() <= 1 && columns <= 1 {
            return Some(rows.pop().and_then(|mut row| row.pop()).unwrap_or_default());
        }
        // Bare `&` / `\\` in a display block: KaTeX rejects this, but agents often
        // omit the environment, so treat it as the obvious `aligned`/`gathered`.
        let kind = if columns > 1 {
            EnvKind::Aligned
        } else {
            EnvKind::Gathered
        };
        Some(vec![Node::Array(Box::new(Array {
            kind,
            cols: Vec::new(),
            rows,
        }))])
    }

    fn parse_cells(&mut self) -> Option<(Rows, End)> {
        let mut rows = Vec::new();
        let mut row = Vec::new();
        loop {
            let (cell, end) = self.parse_list(false)?;
            row.push(cell);
            match end {
                End::Amp => {}
                End::Row => rows.push(mem::take(&mut row)),
                other => {
                    // A trailing `\\` leaves one empty cell; it is not a row.
                    let empty = row.len() == 1 && row.first().is_some_and(Vec::is_empty);
                    if !empty || rows.is_empty() {
                        rows.push(row);
                    }
                    return Some((rows, other));
                }
            }
        }
    }

    fn parse_list(&mut self, stop_at_bracket: bool) -> Option<(Vec<Node>, End)> {
        let mut list = Vec::new();
        loop {
            self.skip_space();
            let Some(c) = self.peek() else {
                return Some((list, End::Eof));
            };
            match c {
                '}' => {
                    self.pos += 1;
                    return Some((list, End::Brace));
                }
                ']' if stop_at_bracket => {
                    self.pos += 1;
                    return Some((list, End::Bracket));
                }
                '&' => {
                    self.pos += 1;
                    return Some((list, End::Amp));
                }
                '^' | '_' => {
                    self.pos += 1;
                    let arg = self.parse_arg()?;
                    attach_script(&mut list, c == '^', arg)?;
                }
                '\'' => {
                    let mut primes = 0;
                    while self.peek() == Some('\'') {
                        self.pos += 1;
                        primes += 1;
                    }
                    let scripts = scripts_of(&mut list)?;
                    scripts.primes += primes;
                }
                '%' => while self.bump().is_some_and(|c| c != '\n') {},
                '{' => {
                    let group = self.parse_arg()?;
                    list.push(group);
                }
                '\\' => {
                    self.pos += 1;
                    let name = self.read_command()?;
                    match name.as_str() {
                        "\\" | "cr" | "newline" => {
                            self.skip_optional_bracket();
                            return Some((list, End::Row));
                        }
                        "right" => {
                            let delim = self.parse_delim()?;
                            return Some((list, End::Right(delim)));
                        }
                        "end" => {
                            let name = self.raw_group()?;
                            return Some((list, End::Env(name.trim().to_owned())));
                        }
                        "limits" | "nolimits" => set_limits(&mut list, name == "limits"),
                        "displaylimits" => {}
                        _ => {
                            let node = self.parse_command(&name)?;
                            list.push(node);
                        }
                    }
                }
                _ => {
                    self.pos += 1;
                    let node = self.char_node(c);
                    list.push(node);
                }
            }
        }
    }

    /// One argument: a braced group, a command with its own arguments, or a char.
    fn parse_arg(&mut self) -> Option<Node> {
        self.skip_space();
        let c = self.peek()?;
        self.enter()?;
        let node = match c {
            '{' => {
                self.pos += 1;
                let (list, end) = self.parse_list(false)?;
                if !matches!(end, End::Brace) {
                    return None;
                }
                Node::Group(list)
            }
            '\\' => {
                self.pos += 1;
                let name = self.read_command()?;
                if matches!(
                    name.as_str(),
                    "\\" | "right" | "end" | "limits" | "nolimits" | "cr"
                ) {
                    return None;
                }
                self.parse_command(&name)?
            }
            '}' | ']' | '&' | '^' | '_' | '%' => return None,
            _ => {
                self.pos += 1;
                self.char_node(c)
            }
        };
        self.leave();
        Some(node)
    }

    fn read_command(&mut self) -> Option<String> {
        let first = self.bump()?;
        if !first.is_ascii_alphabetic() {
            return Some(first.to_string());
        }
        let mut name = String::from(first);
        while let Some(c) = self.peek().filter(char::is_ascii_alphabetic) {
            name.push(c);
            self.pos += 1;
        }
        if name == "operatorname" && self.peek() == Some('*') {
            self.pos += 1;
            name.push('*');
        }
        Some(name)
    }

    /// The raw text of a braced group (or of a single token), braces balanced.
    fn raw_group(&mut self) -> Option<String> {
        self.skip_space();
        match self.bump()? {
            '{' => {}
            '\\' => return Some(format!("\\{}", self.read_command()?)),
            '}' => return None,
            c => return Some(c.to_string()),
        }
        let mut depth = 0usize;
        let mut out = String::new();
        loop {
            let c = self.bump()?;
            match c {
                '\\' => {
                    out.push(c);
                    out.push(self.bump()?);
                }
                '{' => {
                    depth += 1;
                    out.push(c);
                }
                '}' if depth == 0 => return Some(out),
                '}' => {
                    depth -= 1;
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
    }

    fn skip_optional_bracket(&mut self) {
        let start = self.pos;
        self.skip_space();
        if self.peek() != Some('[') {
            self.pos = start;
            return;
        }
        while let Some(c) = self.bump() {
            if c == ']' {
                return;
            }
        }
    }

    fn optional_arg(&mut self) -> Option<Option<Node>> {
        let start = self.pos;
        self.skip_space();
        if self.peek() != Some('[') {
            self.pos = start;
            return Some(None);
        }
        self.pos += 1;
        self.enter()?;
        let (list, end) = self.parse_list(true)?;
        if !matches!(end, End::Bracket) {
            return None;
        }
        self.leave();
        Some(Some(Node::Group(list)))
    }

    fn with_font(&mut self, font: Font) -> Option<Node> {
        let saved = mem::replace(&mut self.font, font);
        let arg = self.parse_arg();
        self.font = saved;
        arg
    }

    fn text_arg(&mut self, italic: bool, bold: bool) -> Option<Node> {
        let raw = self.raw_group()?;
        let text = text_content(&raw);
        if text.is_empty() {
            return Some(Node::Group(Vec::new()));
        }
        self.atoms += 1;
        Some(Node::Glyph(Glyph {
            text,
            class: Class::Ord,
            italic,
            bold,
        }))
    }

    fn parse_delim(&mut self) -> Option<Delim> {
        self.skip_space();
        let c = match self.bump()? {
            '.' => return Some(Delim::Null),
            '<' => '⟨',
            '>' => '⟩',
            c @ ('(' | ')' | '[' | ']' | '|' | '/' | '⟨' | '⟩' | '‖' | '⌊' | '⌋' | '⌈' | '⌉') => {
                c
            }
            '\\' => match self.read_command()?.as_str() {
                "{" | "lbrace" => '{',
                "}" | "rbrace" => '}',
                "|" | "Vert" | "lVert" | "rVert" => '‖',
                "vert" | "lvert" | "rvert" | "mid" => '|',
                "langle" => '⟨',
                "rangle" => '⟩',
                "lfloor" => '⌊',
                "rfloor" => '⌋',
                "lceil" => '⌈',
                "rceil" => '⌉',
                "lbrack" => '[',
                "rbrack" => ']',
                "lgroup" => '(',
                "rgroup" => ')',
                "backslash" => '\\',
                "uparrow" => '↑',
                "downarrow" => '↓',
                "updownarrow" => '↕',
                "Uparrow" => '⇑',
                "Downarrow" => '⇓',
                _ => return None,
            },
            _ => return None,
        };
        Some(Delim::Glyph(c))
    }

    fn read_dimension(&mut self) -> Option<f32> {
        self.skip_space();
        if self.peek() == Some('{') {
            return parse_dimension(&self.raw_group()?);
        }
        let mut raw = String::new();
        while let Some(c) = self
            .peek()
            .filter(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | ' '))
        {
            raw.push(c);
            self.pos += 1;
        }
        for _ in 0..2 {
            if let Some(c) = self.peek().filter(char::is_ascii_alphabetic) {
                raw.push(c);
                self.pos += 1;
            }
        }
        parse_dimension(&raw)
    }

    fn char_node(&mut self, c: char) -> Node {
        if c == '~' {
            return Node::Space(0.28);
        }
        if c == '$' {
            return Node::Space(0.0);
        }
        self.atoms += 1;
        if c.is_ascii_alphanumeric() {
            return styled_char(c, self.font);
        }
        if let Some(node) = unicode_op(c) {
            return node;
        }
        let text = match c {
            '-' => "−".to_owned(),
            '*' => "∗".to_owned(),
            _ => c.to_string(),
        };
        let (class, italic) = unicode_class(c);
        let italic = italic && matches!(self.font, Font::Math | Font::Italic | Font::BoldItalic);
        Node::Glyph(Glyph {
            text,
            class,
            italic,
            bold: matches!(self.font, Font::Bold | Font::BoldItalic),
        })
    }

    fn parse_command(&mut self, name: &str) -> Option<Node> {
        if let Some(em) = named_space(name) {
            return Some(Node::Space(em));
        }
        if let Some(c) = greek(name) {
            self.atoms += 1;
            let lower = c.is_lowercase();
            let italic = match self.font {
                Font::Roman => false,
                Font::Italic | Font::BoldItalic => true,
                _ => lower,
            };
            return Some(Node::Glyph(Glyph {
                text: c.to_string(),
                class: Class::Ord,
                italic,
                bold: matches!(self.font, Font::Bold | Font::BoldItalic),
            }));
        }
        if let Some((text, class)) = symbol(name) {
            self.atoms += 1;
            return Some(Node::glyph(text, class));
        }
        if let Some((text, integral)) = big_op(name) {
            self.atoms += 1;
            return Some(big_op_node(text, integral));
        }
        if let Some((text, display_limits)) = function(name) {
            self.atoms += 1;
            return Some(Node::Op(OpNode {
                text: text.to_owned(),
                big: false,
                integral: false,
                display_limits,
                force: None,
            }));
        }
        let node = match name {
            "frac" | "dfrac" | "tfrac" | "cfrac" | "binom" | "dbinom" | "tbinom" => {
                let num = self.parse_arg()?;
                let den = self.parse_arg()?;
                let level = match name {
                    "dfrac" | "cfrac" | "dbinom" => Some(Level::Display),
                    "tfrac" | "tbinom" => Some(Level::Text),
                    _ => None,
                };
                let binom = name.ends_with("binom");
                Node::Frac(Box::new(Frac {
                    num,
                    den,
                    bar: !binom,
                    level,
                    parens: binom,
                }))
            }
            "sqrt" => {
                let index = self.optional_arg()?;
                let body = self.parse_arg()?;
                Node::Sqrt(Box::new(body), index.map(Box::new))
            }
            "left" => {
                let left = self.parse_delim()?;
                self.enter()?;
                let (body, end) = self.parse_list(false)?;
                self.leave();
                let End::Right(right) = end else {
                    return None;
                };
                self.atoms += 1;
                Node::Fenced(Box::new(Fenced { left, right, body }))
            }
            "middle" => Node::Middle(self.parse_delim()?),
            "big" | "bigl" | "bigr" | "bigm" | "Big" | "Bigl" | "Bigr" | "Bigm" | "bigg"
            | "biggl" | "biggr" | "biggm" | "Bigg" | "Biggl" | "Biggr" | "Biggm" => {
                let delim = self.parse_delim()?;
                self.atoms += 1;
                let height = match name.trim_end_matches(['l', 'r', 'm']) {
                    "big" => 1.2,
                    "Big" => 1.8,
                    "bigg" => 2.4,
                    _ => 3.0,
                };
                let class = match name.chars().last() {
                    Some('l') => Class::Open,
                    Some('r') => Class::Close,
                    Some('m') => Class::Rel,
                    _ => match delim {
                        Delim::Glyph('(' | '[' | '{' | '⟨' | '⌊' | '⌈') => Class::Open,
                        Delim::Glyph(')' | ']' | '}' | '⟩' | '⌋' | '⌉') => Class::Close,
                        _ => Class::Ord,
                    },
                };
                Node::BigDelim(delim, height, class)
            }
            "mathrm" | "mathup" | "mathsf" | "mathtt" | "rm" | "textrm" | "mathnormal" => {
                let font = if name == "mathnormal" {
                    Font::Math
                } else {
                    Font::Roman
                };
                if name == "textrm" {
                    return self.text_arg(false, false);
                }
                self.with_font(font)?
            }
            "mathbf" | "bf" => self.with_font(Font::Bold)?,
            "mathit" | "it" => self.with_font(Font::Italic)?,
            "boldsymbol" | "bm" | "pmb" => self.with_font(Font::BoldItalic)?,
            "mathbb" | "Bbb" => self.with_font(Font::DoubleStruck)?,
            "mathcal" | "mathscr" => self.with_font(Font::Script)?,
            "mathfrak" => self.with_font(Font::Fraktur)?,
            "text" | "textup" | "textnormal" | "mbox" | "hbox" | "textsf" | "texttt" => {
                self.text_arg(false, false)?
            }
            "textbf" => self.text_arg(false, true)?,
            "textit" | "emph" | "textsl" => self.text_arg(true, false)?,
            "operatorname" | "operatorname*" => {
                let raw = self.raw_group()?;
                let text = text_content(&raw);
                self.atoms += 1;
                Node::Op(OpNode {
                    text,
                    big: false,
                    integral: false,
                    display_limits: name.ends_with('*'),
                    force: None,
                })
            }
            "hat" | "widehat" => self.accent(Accent::Mark("ˆ"))?,
            "tilde" | "widetilde" => self.accent(Accent::Mark("˜"))?,
            "dot" => self.accent(Accent::Mark("˙"))?,
            "ddot" => self.accent(Accent::Mark("¨"))?,
            "check" | "widecheck" => self.accent(Accent::Mark("ˇ"))?,
            "breve" => self.accent(Accent::Mark("˘"))?,
            "acute" => self.accent(Accent::Mark("´"))?,
            "grave" => self.accent(Accent::Mark("`"))?,
            "mathring" => self.accent(Accent::Mark("˚"))?,
            "vec" | "overrightarrow" => self.accent(Accent::Vector)?,
            "bar" => self.accent(Accent::Bar)?,
            "overline" => self.accent(Accent::Over)?,
            "underline" => self.accent(Accent::Under)?,
            "overbrace" | "underbrace" => {
                let body = self.parse_arg()?;
                Node::Brace(Box::new(body), name == "overbrace")
            }
            "overset" | "underset" | "stackrel" => {
                let script = self.parse_arg()?;
                let base = self.parse_arg()?;
                let class = match (&base, name) {
                    (_, "stackrel") => Class::Rel,
                    (Node::Glyph(glyph), _) => glyph.class,
                    _ => Class::Ord,
                };
                let (over, under) = if name == "underset" {
                    (None, Some(script))
                } else {
                    (Some(script), None)
                };
                Node::Stack(Box::new(Stack {
                    base,
                    over,
                    under,
                    class,
                }))
            }
            "xrightarrow" | "xleftarrow" => {
                let under = self.optional_arg()?;
                let over = self.parse_arg()?;
                self.atoms += 1;
                Node::XArrow(
                    name == "xleftarrow",
                    Some(Box::new(over)),
                    under.map(Box::new),
                )
            }
            "hspace" | "kern" | "mkern" | "hskip" | "mskip" | "mspace" => {
                self.skip_star();
                Node::Space(self.read_dimension()?)
            }
            "displaystyle" => Node::Style(Level::Display),
            "textstyle" => Node::Style(Level::Text),
            "scriptstyle" => Node::Style(Level::Script),
            "scriptscriptstyle" => Node::Style(Level::ScriptScript),
            "color" => {
                self.raw_group()?;
                Node::Space(0.0)
            }
            "textcolor" => {
                self.raw_group()?;
                self.parse_arg()?
            }
            "colorbox" => {
                self.raw_group()?;
                self.text_arg(false, false)?
            }
            "boxed" => Node::Boxed(Box::new(self.parse_arg()?)),
            "fbox" => Node::Boxed(Box::new(self.text_arg(false, false)?)),
            "phantom" => Node::Phantom(Box::new(self.parse_arg()?), Phantom::Full),
            "hphantom" => Node::Phantom(Box::new(self.parse_arg()?), Phantom::Horizontal),
            "vphantom" => Node::Phantom(Box::new(self.parse_arg()?), Phantom::Vertical),
            "not" => negate(self.parse_arg()?),
            "mathop" | "mathrel" | "mathbin" | "mathord" | "mathopen" | "mathclose"
            | "mathpunct" | "mathinner" => {
                let class = match name {
                    "mathop" => Class::Op,
                    "mathrel" => Class::Rel,
                    "mathbin" => Class::Bin,
                    "mathopen" => Class::Open,
                    "mathclose" => Class::Close,
                    "mathpunct" => Class::Punct,
                    "mathinner" => Class::Inner,
                    _ => Class::Ord,
                };
                let body = match self.parse_arg()? {
                    Node::Group(list) => list,
                    other => vec![other],
                };
                Node::Classed(class, body)
            }
            "bmod" => {
                self.atoms += 1;
                Node::glyph("mod", Class::Bin)
            }
            "pmod" | "pod" | "mod" => {
                let arg = self.parse_arg()?;
                self.atoms += 1;
                let mut list = vec![Node::Space(if name == "mod" { 0.67 } else { 1.0 })];
                if name != "mod" {
                    list.push(Node::glyph("(", Class::Open));
                }
                if name != "pod" {
                    list.push(Node::glyph("mod", Class::Ord));
                    list.push(Node::Space(0.28));
                }
                list.push(arg);
                if name != "mod" {
                    list.push(Node::glyph(")", Class::Close));
                }
                Node::Group(list)
            }
            "begin" => self.parse_env()?,
            "tag" | "label" => {
                self.skip_star();
                self.raw_group()?;
                Node::Space(0.0)
            }
            "nonumber" | "notag" | "hline" | "hdashline" | "relax" | "nobreak" | "allowbreak"
            | "centering" | "strut" | "mathstrut" => Node::Space(0.0),
            _ => {
                self.atoms += 1;
                self.unknown += 1;
                Node::glyph(&format!("\\{name}"), Class::Ord)
            }
        };
        Some(node)
    }

    fn accent(&mut self, accent: Accent) -> Option<Node> {
        Some(Node::Accent(Box::new(self.parse_arg()?), accent))
    }

    fn parse_env(&mut self) -> Option<Node> {
        let name = self.raw_group()?;
        let name = name.trim().to_owned();
        let kind = env_kind(&name)?;
        let mut cols = Vec::new();
        match name.as_str() {
            "array" | "darray" | "subarray" => {
                self.skip_optional_bracket();
                cols = column_spec(&self.raw_group()?);
            }
            "alignat" | "alignat*" | "alignedat" => {
                self.raw_group()?;
            }
            _ => self.skip_optional_bracket(),
        }
        self.enter()?;
        let (rows, end) = self.parse_cells()?;
        self.leave();
        match end {
            End::Env(end_name) if end_name == name => {}
            _ => return None,
        }
        Some(Node::Array(Box::new(Array { kind, cols, rows })))
    }
}

/// Makes the last list entry scriptable, wrapping it in [`Scripts`] if needed.
fn scripts_of(list: &mut Vec<Node>) -> Option<&mut Scripts> {
    let has_base = list
        .last()
        .is_some_and(|node| !matches!(node, Node::Space(_) | Node::Style(_)));
    if !has_base {
        list.push(Node::Group(Vec::new()));
    }
    let last = list.last_mut()?;
    if !matches!(last, Node::Scripts(_)) {
        let base = mem::replace(last, Node::Space(0.0));
        *last = Node::Scripts(Box::new(Scripts {
            base,
            sup: None,
            sub: None,
            primes: 0,
        }));
    }
    match last {
        Node::Scripts(scripts) => Some(scripts),
        _ => None,
    }
}

fn attach_script(list: &mut Vec<Node>, sup: bool, arg: Node) -> Option<()> {
    let scripts = scripts_of(list)?;
    // `^\prime` is a prime, set like `'` rather than as a raised script glyph.
    if sup
        && scripts.sup.is_none()
        && let Node::Glyph(glyph) = &arg
        && !glyph.text.is_empty()
        && glyph.text.chars().all(|c| c == '′')
    {
        scripts.primes += glyph.text.chars().count();
        return Some(());
    }
    let slot = if sup {
        &mut scripts.sup
    } else {
        &mut scripts.sub
    };
    if slot.is_some() {
        return None;
    }
    *slot = Some(arg);
    Some(())
}

fn set_limits(list: &mut [Node], limits: bool) {
    let op = match list.last_mut() {
        Some(Node::Op(op)) => op,
        Some(Node::Scripts(scripts)) => match &mut scripts.base {
            Node::Op(op) => op,
            _ => return,
        },
        _ => return,
    };
    op.force = Some(limits);
}

fn big_op_node(text: &str, integral: bool) -> Node {
    Node::Op(OpNode {
        text: text.to_owned(),
        big: true,
        integral,
        display_limits: !integral,
        force: None,
    })
}

fn negate(node: Node) -> Node {
    let Node::Glyph(mut glyph) = node else {
        return node;
    };
    let negated = match glyph.text.as_str() {
        "=" => "≠",
        "<" => "≮",
        ">" => "≯",
        "≤" => "≰",
        "≥" => "≱",
        "∈" => "∉",
        "∋" => "∌",
        "⊂" => "⊄",
        "⊃" => "⊅",
        "⊆" => "⊈",
        "⊇" => "⊉",
        "≡" => "≢",
        "∼" => "≁",
        "≈" => "≉",
        "≅" => "≇",
        "∣" => "∤",
        "∥" => "∦",
        "∃" => "∄",
        _ => {
            glyph.text.push('\u{338}');
            return Node::Glyph(glyph);
        }
    };
    glyph.text = negated.to_owned();
    Node::Glyph(glyph)
}

fn styled_char(c: char, font: Font) -> Node {
    let letter = c.is_ascii_alphabetic();
    let (text, italic, bold) = match font {
        Font::Math => (c, letter, false),
        Font::Roman => (c, false, false),
        Font::Bold => (c, false, true),
        Font::Italic => (c, true, false),
        Font::BoldItalic => (c, letter, true),
        Font::DoubleStruck | Font::Script | Font::Fraktur => {
            (math_alphanumeric(c, font).unwrap_or(c), false, false)
        }
    };
    Node::Glyph(Glyph {
        text: text.to_string(),
        class: Class::Ord,
        italic,
        bold,
    })
}

/// Maps ASCII letters and digits into the Unicode mathematical alphanumeric blocks,
/// honouring the letterlike-symbol holes (ℝ, ℬ, ℭ, ...) those blocks leave.
fn math_alphanumeric(c: char, font: Font) -> Option<char> {
    let hole = match (font, c) {
        (Font::DoubleStruck, 'C') => Some('ℂ'),
        (Font::DoubleStruck, 'H') => Some('ℍ'),
        (Font::DoubleStruck, 'N') => Some('ℕ'),
        (Font::DoubleStruck, 'P') => Some('ℙ'),
        (Font::DoubleStruck, 'Q') => Some('ℚ'),
        (Font::DoubleStruck, 'R') => Some('ℝ'),
        (Font::DoubleStruck, 'Z') => Some('ℤ'),
        (Font::Script, 'B') => Some('ℬ'),
        (Font::Script, 'E') => Some('ℰ'),
        (Font::Script, 'F') => Some('ℱ'),
        (Font::Script, 'H') => Some('ℋ'),
        (Font::Script, 'I') => Some('ℐ'),
        (Font::Script, 'L') => Some('ℒ'),
        (Font::Script, 'M') => Some('ℳ'),
        (Font::Script, 'R') => Some('ℛ'),
        (Font::Script, 'e') => Some('ℯ'),
        (Font::Script, 'g') => Some('ℊ'),
        (Font::Script, 'o') => Some('ℴ'),
        (Font::Fraktur, 'C') => Some('ℭ'),
        (Font::Fraktur, 'H') => Some('ℌ'),
        (Font::Fraktur, 'I') => Some('ℑ'),
        (Font::Fraktur, 'R') => Some('ℜ'),
        (Font::Fraktur, 'Z') => Some('ℨ'),
        _ => None,
    };
    if hole.is_some() {
        return hole;
    }
    let (upper, lower, digit) = match font {
        Font::DoubleStruck => (0x1D538, 0x1D552, Some(0x1D7D8)),
        Font::Script => (0x1D49C, 0x1D4B6, None),
        Font::Fraktur => (0x1D504, 0x1D51E, None),
        _ => return None,
    };
    let code = match c {
        'A'..='Z' => upper + (c as u32 - 'A' as u32),
        'a'..='z' => lower + (c as u32 - 'a' as u32),
        '0'..='9' => digit? + (c as u32 - '0' as u32),
        _ => return None,
    };
    char::from_u32(code)
}

/// Keeps `\text` content literal: resolves escapes, drops braces and font commands,
/// and collapses whitespace like TeX does.
fn text_content(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let Some(next) = chars.next() else { break };
                if !next.is_ascii_alphabetic() {
                    out.push(match next {
                        '\\' | ',' | ';' | ':' | '!' | ' ' => ' ',
                        other => other,
                    });
                    continue;
                }
                let mut name = String::from(next);
                while let Some(c) = chars.next_if(char::is_ascii_alphabetic) {
                    name.push(c);
                }
                match name.as_str() {
                    "ldots" | "dots" => out.push('…'),
                    "quad" | "qquad" | "space" => out.push(' '),
                    "textbackslash" => out.push('\\'),
                    "text" | "textbf" | "textit" | "textrm" | "emph" | "mathrm" | "textsf"
                    | "texttt" | "mbox" => {}
                    _ => out.push_str(&name),
                }
            }
            '{' | '}' | '$' => {}
            '~' => out.push('\u{a0}'),
            c if c.is_whitespace() => {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            c => out.push(c),
        }
    }
    out
}

fn parse_dimension(raw: &str) -> Option<f32> {
    let raw = raw.trim();
    let split = raw.find(|c: char| c.is_ascii_alphabetic())?;
    let (number, unit) = raw.split_at(split);
    let number = number.replace(' ', "");
    if number.len() > 16 {
        return None;
    }
    let value: f32 = number.parse().ok()?;
    let em = match unit.trim() {
        "em" => 1.0,
        "ex" => 0.43,
        "mu" => 1.0 / 18.0,
        "pt" | "bp" | "px" => 0.1,
        "pc" => 1.2,
        "mm" => 0.2845,
        "cm" => 2.845,
        "in" => 7.227,
        _ => return None,
    };
    Some((value * em).clamp(-5.0, 20.0))
}

fn column_spec(raw: &str) -> Vec<Align> {
    let mut cols = Vec::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match c {
            'l' => cols.push(Align::Left),
            'c' => cols.push(Align::Center),
            'r' => cols.push(Align::Right),
            'p' | 'm' | 'b' => cols.push(Align::Left),
            // `@{...}` and `p{...}` arguments are skipped.
            '{' => {
                let mut depth = 1usize;
                for c in chars.by_ref() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    cols
}

fn env_kind(name: &str) -> Option<EnvKind> {
    Some(match name {
        "matrix" | "matrix*" => EnvKind::Matrix,
        "pmatrix" | "pmatrix*" => EnvKind::Paren,
        "bmatrix" | "bmatrix*" => EnvKind::Bracket,
        "Bmatrix" | "Bmatrix*" => EnvKind::Brace,
        "vmatrix" | "vmatrix*" => EnvKind::Vert,
        "Vmatrix" | "Vmatrix*" => EnvKind::DoubleVert,
        "smallmatrix" => EnvKind::Small,
        "cases" => EnvKind::Cases,
        "dcases" => EnvKind::DisplayCases,
        "rcases" => EnvKind::RightCases,
        "aligned" | "align" | "align*" | "split" | "alignat" | "alignat*" | "alignedat"
        | "eqnarray" | "eqnarray*" | "flalign" | "flalign*" => EnvKind::Aligned,
        "gathered" | "gather" | "gather*" | "equation" | "equation*" | "multline" | "multline*" => {
            EnvKind::Gathered
        }
        "array" | "darray" | "subarray" => EnvKind::Array,
        _ => return None,
    })
}

fn named_space(name: &str) -> Option<f32> {
    Some(match name {
        "," | "thinspace" => THIN,
        ":" | ">" | "medspace" => MEDIUM,
        ";" | "thickspace" => THICK,
        "!" | "negthinspace" => -THIN,
        "negmedspace" => -MEDIUM,
        "negthickspace" => -THICK,
        " " | "space" => 0.28,
        "enspace" => 0.5,
        "quad" => 1.0,
        "qquad" => 2.0,
        _ => return None,
    })
}

fn greek(name: &str) -> Option<char> {
    Some(match name {
        "alpha" => 'α',
        "beta" => 'β',
        "gamma" => 'γ',
        "delta" => 'δ',
        "epsilon" => 'ϵ',
        "varepsilon" => 'ε',
        "zeta" => 'ζ',
        "eta" => 'η',
        "theta" => 'θ',
        "vartheta" => 'ϑ',
        "iota" => 'ι',
        "kappa" => 'κ',
        "varkappa" => 'ϰ',
        "lambda" => 'λ',
        "mu" => 'μ',
        "nu" => 'ν',
        "xi" => 'ξ',
        "omicron" => 'ο',
        "pi" => 'π',
        "varpi" => 'ϖ',
        "rho" => 'ρ',
        "varrho" => 'ϱ',
        "sigma" => 'σ',
        "varsigma" => 'ς',
        "tau" => 'τ',
        "upsilon" => 'υ',
        "phi" => 'ϕ',
        "varphi" => 'φ',
        "chi" => 'χ',
        "psi" => 'ψ',
        "omega" => 'ω',
        "digamma" => 'ϝ',
        "Gamma" => 'Γ',
        "Delta" => 'Δ',
        "Theta" => 'Θ',
        "Lambda" => 'Λ',
        "Xi" => 'Ξ',
        "Pi" => 'Π',
        "Sigma" => 'Σ',
        "Upsilon" => 'Υ',
        "Phi" => 'Φ',
        "Psi" => 'Ψ',
        "Omega" => 'Ω',
        _ => return None,
    })
}

fn symbol(name: &str) -> Option<(&'static str, Class)> {
    use Class::{Bin, Close, Inner, Open, Ord, Punct, Rel};
    Some(match name {
        "pm" => ("±", Bin),
        "mp" => ("∓", Bin),
        "times" => ("×", Bin),
        "div" => ("÷", Bin),
        "cdot" | "centerdot" => ("·", Bin),
        "ast" => ("∗", Bin),
        "star" => ("⋆", Bin),
        "circ" => ("∘", Bin),
        "bullet" => ("•", Bin),
        "cap" => ("∩", Bin),
        "cup" => ("∪", Bin),
        "wedge" | "land" => ("∧", Bin),
        "vee" | "lor" => ("∨", Bin),
        "oplus" => ("⊕", Bin),
        "ominus" => ("⊖", Bin),
        "otimes" => ("⊗", Bin),
        "oslash" => ("⊘", Bin),
        "odot" => ("⊙", Bin),
        "setminus" | "smallsetminus" => ("∖", Bin),
        "sqcup" => ("⊔", Bin),
        "sqcap" => ("⊓", Bin),
        "uplus" => ("⊎", Bin),
        "amalg" => ("⨿", Bin),
        "wr" => ("≀", Bin),
        "diamond" => ("⋄", Bin),
        "dagger" => ("†", Bin),
        "ddagger" => ("‡", Bin),
        "triangleleft" => ("◁", Bin),
        "triangleright" => ("▷", Bin),
        "bigtriangleup" => ("△", Bin),
        "bigtriangledown" => ("▽", Bin),
        "ne" | "neq" => ("≠", Rel),
        "le" | "leq" => ("≤", Rel),
        "ge" | "geq" => ("≥", Rel),
        "leqslant" => ("⩽", Rel),
        "geqslant" => ("⩾", Rel),
        "nleq" => ("≰", Rel),
        "ngeq" => ("≱", Rel),
        "lt" => ("<", Rel),
        "gt" => (">", Rel),
        "ll" => ("≪", Rel),
        "gg" => ("≫", Rel),
        "lll" => ("⋘", Rel),
        "ggg" => ("⋙", Rel),
        "approx" => ("≈", Rel),
        "approxeq" => ("≊", Rel),
        "equiv" => ("≡", Rel),
        "sim" => ("∼", Rel),
        "nsim" => ("≁", Rel),
        "simeq" => ("≃", Rel),
        "cong" => ("≅", Rel),
        "ncong" => ("≇", Rel),
        "propto" | "varpropto" => ("∝", Rel),
        "asymp" => ("≍", Rel),
        "doteq" => ("≐", Rel),
        "triangleq" => ("≜", Rel),
        "coloneqq" | "coloneq" => ("≔", Rel),
        "eqqcolon" | "eqcolon" => ("≕", Rel),
        "prec" => ("≺", Rel),
        "succ" => ("≻", Rel),
        "preceq" => ("⪯", Rel),
        "succeq" => ("⪰", Rel),
        "lesssim" => ("≲", Rel),
        "gtrsim" => ("≳", Rel),
        "in" | "isin" => ("∈", Rel),
        "notin" => ("∉", Rel),
        "ni" | "owns" => ("∋", Rel),
        "subset" => ("⊂", Rel),
        "supset" => ("⊃", Rel),
        "subseteq" => ("⊆", Rel),
        "supseteq" => ("⊇", Rel),
        "subsetneq" => ("⊊", Rel),
        "supsetneq" => ("⊋", Rel),
        "nsubseteq" => ("⊈", Rel),
        "nsupseteq" => ("⊉", Rel),
        "sqsubseteq" => ("⊑", Rel),
        "sqsupseteq" => ("⊒", Rel),
        "mid" => ("∣", Rel),
        "nmid" => ("∤", Rel),
        "parallel" => ("∥", Rel),
        "nparallel" => ("∦", Rel),
        "perp" => ("⊥", Rel),
        "vdash" => ("⊢", Rel),
        "dashv" => ("⊣", Rel),
        "models" | "vDash" => ("⊨", Rel),
        "smile" => ("⌣", Rel),
        "frown" => ("⌢", Rel),
        "bowtie" | "Join" => ("⋈", Rel),
        "therefore" => ("∴", Rel),
        "because" => ("∵", Rel),
        "to" | "rightarrow" => ("→", Rel),
        "gets" | "leftarrow" => ("←", Rel),
        "leftrightarrow" => ("↔", Rel),
        "Rightarrow" => ("⇒", Rel),
        "Leftarrow" => ("⇐", Rel),
        "Leftrightarrow" => ("⇔", Rel),
        "implies" | "Longrightarrow" => ("⟹", Rel),
        "impliedby" | "Longleftarrow" => ("⟸", Rel),
        "iff" | "Longleftrightarrow" => ("⟺", Rel),
        "longrightarrow" => ("⟶", Rel),
        "longleftarrow" => ("⟵", Rel),
        "longleftrightarrow" => ("⟷", Rel),
        "mapsto" => ("↦", Rel),
        "longmapsto" => ("⟼", Rel),
        "uparrow" => ("↑", Rel),
        "downarrow" => ("↓", Rel),
        "updownarrow" => ("↕", Rel),
        "Uparrow" => ("⇑", Rel),
        "Downarrow" => ("⇓", Rel),
        "nearrow" => ("↗", Rel),
        "searrow" => ("↘", Rel),
        "swarrow" => ("↙", Rel),
        "nwarrow" => ("↖", Rel),
        "hookrightarrow" => ("↪", Rel),
        "hookleftarrow" => ("↩", Rel),
        "rightleftharpoons" => ("⇌", Rel),
        "leftrightharpoons" => ("⇋", Rel),
        "rightharpoonup" => ("⇀", Rel),
        "leftharpoonup" => ("↼", Rel),
        "twoheadrightarrow" => ("↠", Rel),
        "rightarrowtail" => ("↣", Rel),
        "leadsto" | "rightsquigarrow" => ("⇝", Rel),
        "colon" => (":", Punct),
        "cdotp" => ("·", Punct),
        "ldotp" => (".", Punct),
        "ldots" | "dots" | "dotsc" | "dotso" | "mathellipsis" => ("…", Inner),
        "cdots" | "dotsb" | "dotsm" | "dotsi" | "midcdots" => ("⋯", Inner),
        "ddots" => ("⋱", Inner),
        "vdots" => ("⋮", Ord),
        "infty" => ("∞", Ord),
        "partial" => ("∂", Ord),
        "nabla" => ("∇", Ord),
        "ell" => ("ℓ", Ord),
        "hbar" | "hslash" => ("ℏ", Ord),
        "emptyset" | "varnothing" => ("∅", Ord),
        "forall" => ("∀", Ord),
        "exists" => ("∃", Ord),
        "nexists" => ("∄", Ord),
        "neg" | "lnot" => ("¬", Ord),
        "angle" => ("∠", Ord),
        "measuredangle" => ("∡", Ord),
        "triangle" => ("△", Ord),
        "prime" => ("′", Ord),
        "aleph" => ("ℵ", Ord),
        "beth" => ("ℶ", Ord),
        "Re" => ("ℜ", Ord),
        "Im" => ("ℑ", Ord),
        "wp" => ("℘", Ord),
        "mho" => ("℧", Ord),
        "complement" => ("∁", Ord),
        "top" => ("⊤", Ord),
        "bot" => ("⊥", Ord),
        "checkmark" => ("✓", Ord),
        "Box" | "square" => ("□", Ord),
        "blacksquare" => ("■", Ord),
        "Diamond" => ("◇", Ord),
        "lozenge" => ("◊", Ord),
        "clubsuit" => ("♣", Ord),
        "diamondsuit" => ("♢", Ord),
        "heartsuit" => ("♡", Ord),
        "spadesuit" => ("♠", Ord),
        "flat" => ("♭", Ord),
        "natural" => ("♮", Ord),
        "sharp" => ("♯", Ord),
        "imath" => ("ı", Ord),
        "jmath" => ("ȷ", Ord),
        "degree" => ("°", Ord),
        "S" | "sect" => ("§", Ord),
        "P" => ("¶", Ord),
        "dag" => ("†", Ord),
        "ddag" => ("‡", Ord),
        "copyright" => ("©", Ord),
        "pounds" => ("£", Ord),
        "surd" => ("√", Ord),
        "backslash" => ("\\", Ord),
        "vert" => ("|", Ord),
        "|" | "Vert" => ("‖", Ord),
        "{" | "lbrace" => ("{", Open),
        "}" | "rbrace" => ("}", Close),
        "langle" => ("⟨", Open),
        "rangle" => ("⟩", Close),
        "lfloor" => ("⌊", Open),
        "rfloor" => ("⌋", Close),
        "lceil" => ("⌈", Open),
        "rceil" => ("⌉", Close),
        "lbrack" => ("[", Open),
        "rbrack" => ("]", Close),
        "lvert" => ("|", Open),
        "rvert" => ("|", Close),
        "lVert" => ("‖", Open),
        "rVert" => ("‖", Close),
        "%" => ("%", Ord),
        "#" => ("#", Ord),
        "&" => ("&", Ord),
        "_" => ("_", Ord),
        "$" => ("$", Ord),
        _ => return None,
    })
}

fn big_op(name: &str) -> Option<(&'static str, bool)> {
    Some(match name {
        "sum" => ("∑", false),
        "prod" => ("∏", false),
        "coprod" => ("∐", false),
        "bigcup" => ("⋃", false),
        "bigcap" => ("⋂", false),
        "bigoplus" => ("⨁", false),
        "bigotimes" => ("⨂", false),
        "bigodot" => ("⨀", false),
        "bigvee" => ("⋁", false),
        "bigwedge" => ("⋀", false),
        "bigsqcup" => ("⨆", false),
        "biguplus" => ("⨄", false),
        "int" | "intop" => ("∫", true),
        "iint" => ("∬", true),
        "iiint" => ("∭", true),
        "oint" => ("∮", true),
        "oiint" => ("∯", true),
        _ => return None,
    })
}

fn function(name: &str) -> Option<(&'static str, bool)> {
    Some(match name {
        "sin" => ("sin", false),
        "cos" => ("cos", false),
        "tan" => ("tan", false),
        "cot" => ("cot", false),
        "sec" => ("sec", false),
        "csc" => ("csc", false),
        "sinh" => ("sinh", false),
        "cosh" => ("cosh", false),
        "tanh" => ("tanh", false),
        "coth" => ("coth", false),
        "arcsin" => ("arcsin", false),
        "arccos" => ("arccos", false),
        "arctan" => ("arctan", false),
        "arg" => ("arg", false),
        "deg" => ("deg", false),
        "dim" => ("dim", false),
        "exp" => ("exp", false),
        "hom" => ("hom", false),
        "ker" => ("ker", false),
        "lg" => ("lg", false),
        "ln" => ("ln", false),
        "log" => ("log", false),
        "sgn" => ("sgn", false),
        "tr" => ("tr", false),
        "lim" => ("lim", true),
        "liminf" => ("lim inf", true),
        "limsup" => ("lim sup", true),
        "max" => ("max", true),
        "min" => ("min", true),
        "sup" => ("sup", true),
        "inf" => ("inf", true),
        "det" => ("det", true),
        "gcd" => ("gcd", true),
        "lcm" => ("lcm", true),
        "Pr" => ("Pr", true),
        "argmax" => ("arg max", true),
        "argmin" => ("arg min", true),
        _ => return None,
    })
}

/// Big operators typed directly as Unicode.
fn unicode_op(c: char) -> Option<Node> {
    let integral = match c {
        '∑' | '∏' | '∐' | '⋃' | '⋂' | '⨁' | '⨂' | '⨀' | '⋁' | '⋀' | '⨆' => {
            false
        }
        '∫' | '∬' | '∭' | '∮' | '∯' => true,
        _ => return None,
    };
    Some(big_op_node(&c.to_string(), integral))
}

/// Class of a literal character, and whether a math-italic font slants it.
fn unicode_class(c: char) -> (Class, bool) {
    let class = match c {
        '+' | '-' | '*' | '±' | '∓' | '×' | '÷' | '·' | '∗' | '∘' | '∩' | '∪' | '∧' | '∨' | '⊕'
        | '⊗' | '⊙' | '∖' => Class::Bin,
        '=' | '<' | '>' | ':' | '≠' | '≤' | '≥' | '≈' | '≡' | '∼' | '≃' | '≅' | '∝' | '→' | '←'
        | '↔' | '⇒' | '⇐' | '⇔' | '↦' | '∈' | '∉' | '∋' | '⊂' | '⊃' | '⊆' | '⊇' | '≪' | '≫'
        | '⟹' | '⟺' | '⟶' | '∣' | '∥' | '⊥' => Class::Rel,
        ',' | ';' => Class::Punct,
        '(' | '[' | '⟨' | '⌊' | '⌈' => Class::Open,
        ')' | ']' | '⟩' | '⌋' | '⌉' | '!' | '?' => Class::Close,
        _ => Class::Ord,
    };
    let italic = matches!(c, 'α'..='ω' | 'ϑ' | 'ϕ' | 'ϖ' | 'ϵ' | 'ϱ');
    (class, italic)
}

// ---------------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------------

/// A math style: TeX level, absolute size in px, and crampedness (which lowers
/// superscripts under bars and in subscripts).
#[derive(Clone, Copy, Debug)]
struct St {
    level: Level,
    size: f32,
    cramped: bool,
}

impl St {
    fn is_script(self) -> bool {
        matches!(self.level, Level::Script | Level::ScriptScript)
    }

    fn cramp(self) -> St {
        St {
            cramped: true,
            ..self
        }
    }

    /// Scripts shrink to 0.7 and then about 0.5 of the root size, never below half.
    fn script(self, root: f32, cramped: bool) -> St {
        let (level, factor) = match self.level {
            Level::Display | Level::Text => (Level::Script, 0.7),
            Level::Script | Level::ScriptScript => (Level::ScriptScript, 0.72),
        };
        St {
            level,
            size: (self.size * factor).max(root * 0.5).min(self.size),
            cramped: cramped || self.cramped,
        }
    }

    /// Numerator and denominator styles. Inline fractions keep text spacing at 0.8
    /// size rather than TeX's script size, which is too small on screen.
    fn fraction_parts(self, root: f32) -> (St, St) {
        let (level, size) = match self.level {
            Level::Display => (Level::Text, self.size),
            Level::Text => (Level::Text, self.size * 0.8),
            Level::Script | Level::ScriptScript => (Level::ScriptScript, self.size * 0.75),
        };
        let size = size.max(root * 0.45).min(self.size);
        let num = St {
            level,
            size,
            cramped: self.cramped,
        };
        (num, num.cramp())
    }

    fn switch(self, level: Level, root: f32) -> St {
        let size = match level {
            Level::Display | Level::Text => root,
            Level::Script => root * 0.7,
            Level::ScriptScript => root * 0.5,
        };
        St {
            level,
            size,
            cramped: self.cramped,
        }
    }
}

#[derive(Clone, Debug)]
enum Placed {
    Glyphs {
        text: String,
        x: f32,
        baseline: f32,
        size: f32,
        italic: bool,
        bold: bool,
        advance: f32,
    },
    Rule {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    },
    Radical {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    },
}

impl Placed {
    fn shift(&mut self, dx: f32, dy: f32) {
        match self {
            Placed::Glyphs { x, baseline, .. } => {
                *x += dx;
                *baseline += dy;
            }
            Placed::Rule { x, y, .. } | Placed::Radical { x, y, .. } => {
                *x += dx;
                *y += dy;
            }
        }
    }
}

/// A TeX box: extent around its baseline origin and its content.
#[derive(Clone, Debug, Default)]
struct MBox {
    width: f32,
    height: f32,
    depth: f32,
    /// Italic correction already included in `width` (subscripts tuck under it).
    italic: f32,
    items: Vec<Placed>,
}

impl MBox {
    fn empty(width: f32) -> MBox {
        MBox {
            width,
            ..MBox::default()
        }
    }

    /// Moves `other`'s content into this box at `(dx, dy)`; extents are the caller's.
    fn place(&mut self, other: MBox, dx: f32, dy: f32) {
        self.items.extend(other.items.into_iter().map(|mut item| {
            item.shift(dx, dy);
            item
        }));
    }

    fn rule(&mut self, x: f32, y: f32, width: f32, height: f32) {
        if width > 0.0 && height > 0.0 {
            self.items.push(Placed::Rule {
                x,
                y,
                width,
                height,
            });
        }
    }

    /// Appends `other` on the baseline at the current width.
    fn append(&mut self, other: MBox) {
        let x = self.width;
        self.width += other.width;
        self.height = self.height.max(other.height);
        self.depth = self.depth.max(other.depth);
        self.place(other, x, 0.0);
    }
}

struct Engine<'a, M> {
    measure: &'a M,
    ascent: f32,
    descent: f32,
    root: f32,
}

/// Inter-atom spacing (TeX's table): 1 thin, 2 medium, 3 thick; negative entries
/// apply only outside script styles.
const SPACING: [[i8; 8]; 8] = [
    // Ord Op  Bin Rel Open Close Punct Inner
    [0, 1, -2, -3, 0, 0, 0, -1],     // Ord
    [1, 1, 0, -3, 0, 0, 0, -1],      // Op
    [-2, -2, 0, 0, -2, 0, 0, -2],    // Bin
    [-3, -3, 0, 0, -3, 0, 0, -3],    // Rel
    [0, 0, 0, 0, 0, 0, 0, 0],        // Open
    [0, 1, -2, -3, 0, 0, 0, -1],     // Close
    [-1, -1, 0, -1, -1, -1, -1, -1], // Punct
    [-1, 1, -2, -3, -1, 0, -1, -1],  // Inner
];

fn class_index(class: Class) -> usize {
    match class {
        Class::Ord => 0,
        Class::Op => 1,
        Class::Bin => 2,
        Class::Rel => 3,
        Class::Open => 4,
        Class::Close => 5,
        Class::Punct => 6,
        Class::Inner => 7,
    }
}

fn spacing(left: Class, right: Class, st: St) -> f32 {
    let entry = SPACING[class_index(left)][class_index(right)];
    if entry < 0 && st.is_script() {
        return 0.0;
    }
    let em = match entry.abs() {
        1 => THIN,
        2 => MEDIUM,
        3 => THICK,
        _ => 0.0,
    };
    em * st.size
}

impl<M: Fn(&str, f32, bool) -> f32> Engine<'_, M> {
    fn rule_thickness(&self, size: f32) -> f32 {
        // At least a device pixel so bars stay visible at chat sizes.
        (RULE * size).max(1.0_f32.min(size * 0.08))
    }

    fn advance(&self, text: &str, size: f32, italic: bool, bold: bool) -> f32 {
        let width = (self.measure)(text, size, italic);
        let width = if width.is_finite() {
            width.max(0.0)
        } else {
            0.0
        };
        if bold { width * BOLD_WIDTH } else { width }
    }

    fn glyph(&self, text: &str, size: f32, italic: bool, bold: bool) -> MBox {
        if text.is_empty() {
            return MBox::default();
        }
        let advance = self.advance(text, size, italic, bold);
        let (height, depth) = text.chars().fold((0.0_f32, 0.0_f32), |(h, d), c| {
            let (ch, cd) = ink(c, italic);
            (h.max(ch), d.max(cd))
        });
        let mut chars = text.chars();
        let (lead, correction) = match (chars.next(), chars.next()) {
            (Some(c), None) if italic => (left_overhang(c) * size, italic_correction(c) * size),
            _ => (0.0, 0.0),
        };
        MBox {
            width: lead + advance + correction,
            height: height.min(self.ascent) * size,
            depth: depth.min(self.descent) * size,
            italic: correction,
            items: vec![Placed::Glyphs {
                text: text.to_owned(),
                x: lead,
                baseline: 0.0,
                size,
                italic,
                bold,
                advance,
            }],
        }
    }

    /// Lays out a horizontal list with TeX spacing. `leading` is the class of an
    /// implicit atom before the list (an `aligned` cell's `{}` or a `\left`).
    fn list(&self, nodes: &[Node], st: St, leading: Option<Class>) -> MBox {
        let mut st = st;
        let mut parts: Vec<(MBox, Option<Class>, St)> = Vec::with_capacity(nodes.len());
        for node in nodes {
            if let Node::Style(level) = node {
                st = st.switch(*level, self.root);
                continue;
            }
            let (content, class) = self.node(node, st);
            parts.push((content, class, st));
        }
        // A binary operator with no left operand, or followed by a relation,
        // closing, or punctuation, is an ordinary symbol (unary minus).
        let mut previous = leading;
        let mut previous_index: Option<usize> = None;
        for index in 0..parts.len() {
            let Some(class) = parts[index].1 else {
                continue;
            };
            let mut class = class;
            if class == Class::Bin
                && previous.is_none_or(|p| {
                    matches!(
                        p,
                        Class::Bin | Class::Op | Class::Rel | Class::Open | Class::Punct
                    )
                })
            {
                class = Class::Ord;
            }
            if matches!(class, Class::Rel | Class::Close | Class::Punct)
                && previous == Some(Class::Bin)
                && let Some(p) = previous_index
            {
                parts[p].1 = Some(Class::Ord);
            }
            parts[index].1 = Some(class);
            previous = Some(class);
            previous_index = Some(index);
        }
        if let Some(p) = previous_index
            && parts[p].1 == Some(Class::Bin)
        {
            parts[p].1 = Some(Class::Ord);
        }

        let mut out = MBox::default();
        let mut previous = leading;
        for (content, class, st) in parts {
            if let Some(class) = class {
                if let Some(left) = previous {
                    out.width += spacing(left, class, st);
                }
                previous = Some(class);
            }
            out.italic = content.italic;
            out.append(content);
        }
        out
    }

    fn node(&self, node: &Node, st: St) -> (MBox, Option<Class>) {
        let class = Some(node.class());
        let content = match node {
            Node::Glyph(glyph) => self.glyph(&glyph.text, st.size, glyph.italic, glyph.bold),
            Node::Group(list) => {
                let mut content = self.list(list, st, None);
                if list.len() != 1 {
                    content.italic = 0.0;
                }
                content
            }
            Node::Classed(_, list) => self.list(list, st, None),
            Node::Op(op) => self.op(op, st),
            Node::Scripts(scripts) => self.scripts(scripts, st),
            Node::Frac(frac) => self.frac(frac, st),
            Node::Sqrt(body, index) => self.sqrt(body, index.as_deref(), st),
            Node::Accent(body, accent) => self.accent(body, *accent, st),
            Node::Fenced(fenced) => self.fenced(fenced, st),
            Node::Middle(delim) => self.delimiter(*delim, 0.0, st.size),
            Node::BigDelim(delim, em, _) => self.delimiter(*delim, em * st.size, st.size),
            Node::Space(em) => return (MBox::empty(em * st.size), None),
            Node::Array(array) => self.array(array, st),
            Node::Stack(stack) => {
                let base = self.node(&stack.base, st).0;
                let over = stack
                    .over
                    .as_ref()
                    .map(|n| self.node(n, st.script(self.root, false)).0);
                let under = stack
                    .under
                    .as_ref()
                    .map(|n| self.node(n, st.script(self.root, true)).0);
                self.stack(base, over, under, st.size, 0.6)
            }
            Node::Brace(body, over) => self.brace(body, *over, st),
            Node::XArrow(left, over, under) => {
                self.xarrow(*left, over.as_deref(), under.as_deref(), st)
            }
            Node::Phantom(body, kind) => {
                let body = self.node(body, st).0;
                match kind {
                    Phantom::Full => MBox {
                        width: body.width,
                        height: body.height,
                        depth: body.depth,
                        ..MBox::default()
                    },
                    Phantom::Horizontal => MBox::empty(body.width),
                    Phantom::Vertical => MBox {
                        height: body.height,
                        depth: body.depth,
                        ..MBox::default()
                    },
                }
            }
            Node::Style(_) => return (MBox::default(), None),
            Node::Boxed(body) => self.boxed(body, st),
        };
        (content, class)
    }

    fn op(&self, op: &OpNode, st: St) -> MBox {
        if !op.big {
            return self.glyph(&op.text, st.size, false, false);
        }
        let mut content = self.glyph(&op.text, st.size * op_scale(op, st), false, false);
        center_on_axis(&mut content, st.size);
        content
    }

    fn scripts(&self, scripts: &Scripts, st: St) -> MBox {
        let limits = match &scripts.base {
            Node::Op(op) => op
                .force
                .unwrap_or(op.display_limits && st.level == Level::Display),
            Node::Brace(..) => true,
            _ => false,
        };
        let sup_st = st.script(self.root, false);
        let sub_st = st.script(self.root, true);
        if limits && scripts.primes == 0 {
            let base = self.node(&scripts.base, st).0;
            let over = scripts.sup.as_ref().map(|n| self.node(n, sup_st).0);
            let under = scripts.sub.as_ref().map(|n| self.node(n, sub_st).0);
            return self.stack(base, over, under, st.size, 0.6);
        }

        let size = st.size;
        let base = self.node(&scripts.base, st).0;
        let simple = matches!(scripts.base, Node::Glyph(_));
        let (base_height, base_depth) = if simple {
            (0.0, 0.0)
        } else {
            (base.height, base.depth)
        };
        // An integral's upper hook leans right: push its superscript clear of it.
        let hook = match &scripts.base {
            Node::Op(op) if op.integral => 0.12 * op_scale(op, st) * size,
            _ => 0.0,
        };
        let sub_x = base.width - base.italic;
        let mut out = MBox {
            width: base.width,
            height: base.height,
            depth: base.depth,
            ..MBox::default()
        };
        out.place(base, 0.0, 0.0);
        let mut sup_x = out.width + hook;
        if scripts.primes > 0 {
            let primes = self.glyph(&"′".repeat(scripts.primes.min(4)), size, false, false);
            out.height = out.height.max(primes.height);
            sup_x += primes.width;
            out.place(primes, out.width, 0.0);
            out.width = sup_x;
        }
        let sup = scripts.sup.as_ref().map(|n| self.node(n, sup_st).0);
        let sub = scripts.sub.as_ref().map(|n| self.node(n, sub_st).0);
        if sup.is_none() && sub.is_none() {
            return out;
        }

        let x_height = X_HEIGHT * size;
        let theta = RULE * size;
        let sup1 = size
            * if st.cramped {
                0.289
            } else if st.level == Level::Display {
                0.413
            } else {
                0.363
            };
        let sup_shift = |sup: &MBox| {
            (base_height - 0.386 * sup_st.size)
                .max(sup1)
                .max(sup.depth + 0.25 * x_height)
        };
        let mut right = out.width;
        match (sup, sub) {
            (Some(sup), None) => {
                let u = sup_shift(&sup);
                self.put_script(&mut out, sup, sup_x, -u, &mut right);
            }
            (None, Some(sub)) => {
                let v = (base_depth + 0.05 * sub_st.size)
                    .max(0.15 * size)
                    .max(sub.height - 0.8 * x_height);
                self.put_script(&mut out, sub, sub_x, v, &mut right);
            }
            (Some(sup), Some(sub)) => {
                let mut u = sup_shift(&sup);
                let mut v = (base_depth + 0.05 * sub_st.size).max(0.247 * size);
                let gap = (u - sup.depth) - (sub.height - v);
                if gap < 4.0 * theta {
                    v += 4.0 * theta - gap;
                    let psi = 0.8 * x_height - (u - sup.depth);
                    if psi > 0.0 {
                        u += psi;
                        v -= psi;
                    }
                }
                self.put_script(&mut out, sup, sup_x, -u, &mut right);
                self.put_script(&mut out, sub, sub_x, v, &mut right);
            }
            (None, None) => {}
        }
        out.width = right + 0.05 * size;
        out
    }

    fn put_script(&self, out: &mut MBox, script: MBox, x: f32, y: f32, right: &mut f32) {
        out.height = out.height.max(script.height - y);
        out.depth = out.depth.max(script.depth + y);
        *right = right.max(x + script.width);
        out.place(script, x, y);
    }

    /// Centres `over` and `under` on `base` with TeX's limit clearances.
    /// `min_under` is the minimum drop of the lower baseline below the base, in em.
    fn stack(
        &self,
        base: MBox,
        over: Option<MBox>,
        under: Option<MBox>,
        size: f32,
        min_under: f32,
    ) -> MBox {
        let width = base
            .width
            .max(over.as_ref().map_or(0.0, |b| b.width))
            .max(under.as_ref().map_or(0.0, |b| b.width));
        let mut out = MBox {
            width,
            height: base.height,
            depth: base.depth,
            ..MBox::default()
        };
        let (base_height, base_depth) = (base.height, base.depth);
        let base_x = (width - base.width) / 2.0;
        out.place(base, base_x, 0.0);
        if let Some(over) = over {
            let shift = base_height + (0.111 * size + over.depth).max(0.2 * size);
            out.height = shift + over.height + 0.1 * size;
            let x = (width - over.width) / 2.0;
            out.place(over, x, -shift);
        }
        if let Some(under) = under {
            let shift = base_depth + (0.167 * size + under.height).max(min_under * size);
            out.depth = shift + under.depth + 0.1 * size;
            let x = (width - under.width) / 2.0;
            out.place(under, x, shift);
        }
        out
    }

    fn frac(&self, frac: &Frac, st: St) -> MBox {
        let st = match frac.level {
            Some(level) => St {
                level,
                size: st
                    .size
                    .max(if st.is_script() { st.size } else { self.root }),
                cramped: st.cramped,
            },
            None => st,
        };
        let size = st.size;
        let (num_st, den_st) = st.fraction_parts(self.root);
        let num = self.node(&frac.num, num_st).0;
        let den = self.node(&frac.den, den_st).0;
        let display = st.level == Level::Display;
        let axis = AXIS * size;
        let theta = self.rule_thickness(size);
        let (mut u, mut v) = match (display, frac.bar) {
            (true, _) => (0.677 * size, 0.686 * size),
            (false, true) => (0.394 * size, 0.345 * size),
            (false, false) => (0.444 * size, 0.345 * size),
        };
        if frac.bar {
            let phi = if display { 3.0 * theta } else { 1.5 * theta };
            let above = (u - num.depth) - (axis + theta / 2.0);
            if above < phi {
                u += phi - above;
            }
            let below = (axis - theta / 2.0) - (den.height - v);
            if below < phi {
                v += phi - below;
            }
        } else {
            let phi = if display { 7.0 } else { 3.0 } * RULE * size;
            let gap = (u - num.depth) - (den.height - v);
            if gap < phi {
                u += (phi - gap) / 2.0;
                v += (phi - gap) / 2.0;
            }
        }
        let inner = num.width.max(den.width);
        // Null delimiter space keeps adjacent fractions and bars apart.
        let pad = if frac.parens { 0.0 } else { 0.12 * size };
        let overhang = if frac.bar { 0.06 * size } else { 0.0 };
        let bar_width = inner + 2.0 * overhang;
        let mut out = MBox {
            width: bar_width + 2.0 * pad,
            height: u + num.height,
            depth: v + den.depth,
            ..MBox::default()
        };
        let num_x = pad + (bar_width - num.width) / 2.0;
        let den_x = pad + (bar_width - den.width) / 2.0;
        out.place(num, num_x, -u);
        out.place(den, den_x, v);
        if frac.bar {
            out.height = out.height.max(axis + theta / 2.0);
            out.depth = out.depth.max(theta / 2.0 - axis);
            out.rule(pad, -(axis + theta / 2.0), bar_width, theta);
        }
        if frac.parens {
            return self.fence(out, Delim::Glyph('('), Delim::Glyph(')'), size);
        }
        out
    }

    fn sqrt(&self, body: &Node, index: Option<&Node>, st: St) -> MBox {
        let size = st.size;
        let body = self.node(body, st.cramp()).0;
        let t = self.rule_thickness(size);
        let mut gap = t + if st.level == Level::Display {
            0.25 * X_HEIGHT * size
        } else {
            0.04 * size
        };
        // A radical is never shorter than about an em; spare height is split evenly
        // above and below the radicand, as TeX does with its smallest surd.
        let mut bottom = body.depth;
        let natural = body.height + body.depth + gap + t;
        let minimum = 1.02 * size;
        if natural < minimum {
            let extra = (minimum - natural) / 2.0;
            gap += extra;
            bottom += extra;
        }
        let top = body.height + gap + t;
        let height = top - t / 2.0 + bottom;
        let width = (0.5 * size + 0.06 * height).min(0.9 * size);

        let mut out = MBox::default();
        // Inset by half a stroke so the check mark's round cap stays in the box.
        let mut radical_x = t / 2.0;
        if let Some(index) = index {
            let index_st = St {
                level: Level::ScriptScript,
                size: (size * 0.5).max(self.root * 0.4).min(size),
                cramped: false,
            };
            let index = self.node(index, index_st).0;
            // The index's right edge sits over the middle of the check mark, with its
            // baseline 60% of the way up the radical.
            radical_x = (0.1 * size + index.width - 0.5 * width).max(t / 2.0);
            let index_x = radical_x + 0.5 * width - index.width;
            let index_y = bottom - 0.6 * height;
            out.height = out.height.max(index.height - index_y);
            out.place(index, index_x, index_y);
        }
        let body_x = radical_x + width + 0.08 * size;
        let bar_end = body_x + body.width + 0.04 * size;
        out.items.push(Placed::Radical {
            x: radical_x,
            y: -top + t / 2.0,
            width,
            height,
        });
        out.rule(radical_x + width, -top, bar_end - radical_x - width, t);
        out.place(body, body_x, 0.0);
        out.width = bar_end + 0.04 * size;
        out.height = out.height.max(top);
        out.depth = bottom + t;
        out
    }

    fn accent(&self, body: &Node, accent: Accent, st: St) -> MBox {
        let size = st.size;
        let skew = match body {
            Node::Glyph(glyph) if glyph.italic && glyph.text.chars().count() == 1 => 0.06 * size,
            _ => 0.0,
        };
        let content = self.node(body, st.cramp()).0;
        let t = self.rule_thickness(size);
        let mut out = MBox {
            width: content.width,
            height: content.height,
            depth: content.depth,
            ..MBox::default()
        };
        let (body_width, body_height, body_depth) = (content.width, content.height, content.depth);
        out.place(content, 0.0, 0.0);
        match accent {
            Accent::Over => {
                out.rule(0.0, -(body_height + 3.0 * t + t), body_width, t);
                out.height = body_height + 5.0 * t;
            }
            Accent::Under => {
                out.rule(0.0, body_depth + 3.0 * t, body_width, t);
                out.depth = body_depth + 5.0 * t;
            }
            Accent::Bar => {
                let width = (body_width - 0.1 * size).max(0.3 * size);
                let bottom = body_height.max(X_HEIGHT * size) + 0.1 * size;
                let x = (body_width - width) / 2.0 + skew;
                out.rule(x, -(bottom + t), width, t);
                out.height = out.height.max(bottom + t);
            }
            Accent::Mark(mark) => {
                let glyph = self.glyph(mark, size, false, false);
                let raise = (body_height - X_HEIGHT * size).max(0.0);
                let x = body_width / 2.0 + skew - glyph.width / 2.0;
                out.height = out.height.max(glyph.height + raise);
                out.place(glyph, x, -raise);
            }
            Accent::Vector => {
                let arrow_size = size * 0.7;
                let glyph = self.glyph("→", arrow_size, false, false);
                // The arrow's ink starts about 0.08 em above its baseline.
                let bottom = body_height + 0.08 * size;
                let y = -(bottom - 0.08 * arrow_size);
                let x = body_width / 2.0 + skew - glyph.width / 2.0;
                out.height = out.height.max(glyph.height - y);
                out.place(glyph, x, y);
            }
        }
        normalize_left(&mut out);
        out
    }

    fn fenced(&self, fenced: &Fenced, st: St) -> MBox {
        // `\middle` splits the body into independently laid-out segments so each
        // node is laid out once and nesting cannot multiply work.
        let mut segments: Vec<MBox> = Vec::new();
        let mut middles = Vec::new();
        let mut start = 0;
        for (index, node) in fenced.body.iter().enumerate() {
            if let Node::Middle(delim) = node {
                segments.push(self.list(&fenced.body[start..index], st, Some(Class::Open)));
                middles.push(*delim);
                start = index + 1;
            }
        }
        segments.push(self.list(&fenced.body[start..], st, Some(Class::Open)));
        let height = segments.iter().map(|b| b.height).fold(0.0, f32::max);
        let depth = segments.iter().map(|b| b.depth).fold(0.0, f32::max);
        let target = delimiter_target(height, depth, st.size);
        let mut inner = MBox::default();
        let mut segments = segments.into_iter();
        if let Some(first) = segments.next() {
            inner.append(first);
        }
        for (segment, delim) in segments.zip(middles) {
            inner.append(self.delimiter(delim, target, st.size));
            inner.append(segment);
        }
        self.fence(inner, fenced.left, fenced.right, st.size)
    }

    fn fence(&self, inner: MBox, left: Delim, right: Delim, size: f32) -> MBox {
        let target = delimiter_target(inner.height, inner.depth, size);
        let mut out = self.delimiter(left, target, size);
        out.append(inner);
        out.append(self.delimiter(right, target, size));
        out.italic = 0.0;
        out
    }

    /// A delimiter at least `total` px tall, centred on the math axis.
    fn delimiter(&self, delim: Delim, total: f32, size: f32) -> MBox {
        let Delim::Glyph(c) = delim else {
            return MBox::empty(0.12 * size);
        };
        let text = c.to_string();
        let (height, depth) = ink(c, false);
        let natural = (height + depth) * size;
        let scale = (total / natural).max(1.0);
        let drawable = matches!(
            c,
            '(' | ')' | '[' | ']' | '{' | '}' | '|' | '‖' | '⟨' | '⟩' | '⌊' | '⌋' | '⌈' | '⌉'
        );
        if scale <= MAX_DELIM_GLYPH_SCALE || !drawable {
            let mut content = self.glyph(&text, size * scale.min(6.0), false, false);
            center_on_axis(&mut content, size);
            return content;
        }
        self.drawn_delimiter(c, total, size)
    }

    fn drawn_delimiter(&self, c: char, total: f32, size: f32) -> MBox {
        let top = -(AXIS * size + total / 2.0);
        let bottom = top + total;
        let stem = (0.06 * size).max(self.rule_thickness(size));
        let pad = 0.08 * size;
        let mut out = MBox {
            height: -top,
            depth: bottom,
            ..MBox::default()
        };
        match c {
            '[' | ']' | '⌊' | '⌋' | '⌈' | '⌉' => {
                let arm = 0.26 * size;
                out.width = 2.0 * pad + arm;
                let left = matches!(c, '[' | '⌊' | '⌈');
                let stem_x = if left { pad } else { pad + arm - stem };
                let arm_x = pad;
                out.rule(stem_x, top, stem, total);
                let thin = stem * 0.8;
                if matches!(c, '[' | ']' | '⌈' | '⌉') {
                    out.rule(arm_x, top, arm, thin);
                }
                if matches!(c, '[' | ']' | '⌊' | '⌋') {
                    out.rule(arm_x, bottom - thin, arm, thin);
                }
            }
            '|' => {
                out.width = 2.0 * pad + stem;
                out.rule(pad, top, stem, total);
            }
            '‖' => {
                let gap = 0.12 * size;
                out.width = 2.0 * pad + 2.0 * stem + gap;
                out.rule(pad, top, stem, total);
                out.rule(pad + stem + gap, top, stem, total);
            }
            '(' | ')' => {
                let bulge = ((0.16 + 0.04 * total / size) * size).min(0.42 * size);
                let thick = stem * 1.35;
                let thin = stem * 0.5;
                out.width = 2.0 * pad + bulge + thick;
                let mirror = c == ')';
                let band = Band {
                    top,
                    total,
                    x0: pad,
                    span: bulge + thick,
                    mirror,
                };
                curve(&mut out, band, |v| {
                    // Near-flat middle with increasingly tight ends, like a text paren.
                    let offset = bulge * (0.55 * v * v + 0.45 * (1.0 - (1.0 - v * v).sqrt()));
                    (offset, thin + (thick - thin) * (1.0 - v * v))
                });
            }
            '{' | '}' => {
                let reach = 0.2 * size;
                let thick = stem * 1.2;
                let thin = stem * 0.55;
                let half = total / 2.0;
                let cusp = (0.2 * size / half).min(0.3);
                let hook = (0.22 * size / half).min(0.35);
                out.width = 2.0 * pad + 2.0 * reach + thick;
                let mirror = c == '}';
                let band = Band {
                    top,
                    total,
                    x0: pad,
                    span: 2.0 * reach + thick,
                    mirror,
                };
                curve(&mut out, band, |v| {
                    let a = v.abs();
                    if a < cusp {
                        let t = a / cusp;
                        let x = reach * (1.0 - (1.0 - t) * (1.0 - t));
                        (x, thin + (thick - thin) * t)
                    } else if a > 1.0 - hook {
                        let t = (a - (1.0 - hook)) / hook;
                        (reach + reach * t * t, thick + (thin - thick) * t)
                    } else {
                        (reach, thick)
                    }
                });
            }
            _ => {
                // Angle brackets.
                let reach = ((0.22 + 0.03 * total / size) * size).min(0.5 * size);
                let thick = stem;
                out.width = 2.0 * pad + reach + thick;
                let mirror = c == '⟩';
                let band = Band {
                    top,
                    total,
                    x0: pad,
                    span: reach + thick,
                    mirror,
                };
                curve(&mut out, band, |v| (reach * v.abs(), thick));
            }
        }
        out
    }

    fn array(&self, array: &Array, st: St) -> MBox {
        let kind = array.kind;
        let cell_st = match kind {
            EnvKind::Aligned | EnvKind::Gathered | EnvKind::DisplayCases => St {
                level: Level::Display,
                ..st
            },
            EnvKind::Small => st.script(self.root, false),
            _ => St {
                level: if st.level == Level::Display {
                    Level::Text
                } else {
                    st.level
                },
                ..st
            },
        };
        let size = cell_st.size;
        let columns = array.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return MBox::default();
        }
        let aligned = kind == EnvKind::Aligned;
        let cells: Vec<Vec<MBox>> = array
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .enumerate()
                    .map(|(col, cell)| {
                        // `&=` gets relation spacing on both sides, as if `{}` preceded it.
                        let leading = (aligned && col % 2 == 1).then_some(Class::Ord);
                        self.list(cell, cell_st, leading)
                    })
                    .collect()
            })
            .collect();

        let mut widths = vec![0.0_f32; columns];
        for row in &cells {
            for (col, cell) in row.iter().enumerate() {
                widths[col] = widths[col].max(cell.width);
            }
        }
        let align = |col: usize| match kind {
            EnvKind::Aligned => {
                if col.is_multiple_of(2) {
                    Align::Right
                } else {
                    Align::Left
                }
            }
            EnvKind::Cases | EnvKind::DisplayCases | EnvKind::RightCases => Align::Left,
            EnvKind::Array => array.cols.get(col).copied().unwrap_or(Align::Center),
            _ => Align::Center,
        };
        let gap_before = |col: usize| match kind {
            _ if col == 0 => 0.0,
            EnvKind::Aligned if col % 2 == 1 => 0.0,
            EnvKind::Aligned => 1.5 * size,
            EnvKind::Small => 0.4 * size,
            _ => size,
        };
        let outer = if kind == EnvKind::Array {
            0.5 * size
        } else {
            0.0
        };
        let mut xs = Vec::with_capacity(columns);
        let mut x = outer;
        for (col, width) in widths.iter().enumerate() {
            x += gap_before(col);
            xs.push(x);
            x += width;
        }
        let total_width = x + outer;

        let stretch = match kind {
            EnvKind::Cases | EnvKind::DisplayCases | EnvKind::RightCases => 1.2,
            _ => 1.0,
        };
        let skip = 1.2 * size * stretch;
        let jot = if aligned { 0.25 * size } else { 0.0 };
        let metrics: Vec<(f32, f32)> = cells
            .iter()
            .map(|row| {
                row.iter().fold((0.7 * skip, 0.3 * skip), |(h, d), cell| {
                    (h.max(cell.height), d.max(cell.depth))
                })
            })
            .collect();
        let total_height: f32 = metrics.iter().map(|(h, d)| h + d).sum::<f32>()
            + jot * metrics.len().saturating_sub(1) as f32;
        let axis = AXIS * st.size;
        let mut out = MBox {
            width: total_width,
            height: total_height / 2.0 + axis,
            depth: total_height / 2.0 - axis,
            ..MBox::default()
        };
        let mut y = -out.height;
        for (row, (height, depth)) in cells.into_iter().zip(metrics) {
            let baseline = y + height;
            for (col, cell) in row.into_iter().enumerate() {
                let slack = widths[col] - cell.width;
                let offset = match align(col) {
                    Align::Left => 0.0,
                    Align::Center => slack / 2.0,
                    Align::Right => slack,
                };
                out.place(cell, xs[col] + offset, baseline);
            }
            y = baseline + depth + jot;
        }
        let (left, right) = match kind {
            EnvKind::Paren => (Delim::Glyph('('), Delim::Glyph(')')),
            EnvKind::Bracket => (Delim::Glyph('['), Delim::Glyph(']')),
            EnvKind::Brace => (Delim::Glyph('{'), Delim::Glyph('}')),
            EnvKind::Vert => (Delim::Glyph('|'), Delim::Glyph('|')),
            EnvKind::DoubleVert => (Delim::Glyph('‖'), Delim::Glyph('‖')),
            EnvKind::Cases | EnvKind::DisplayCases => (Delim::Glyph('{'), Delim::Null),
            EnvKind::RightCases => (Delim::Null, Delim::Glyph('}')),
            _ => return out,
        };
        self.fence(out, left, right, st.size)
    }

    fn brace(&self, body: &Node, over: bool, st: St) -> MBox {
        let size = st.size;
        let content = self.node(body, st).0;
        let t = self.rule_thickness(size);
        let tick = 0.14 * size;
        let gap = 0.1 * size;
        let width = content.width;
        let mut out = MBox {
            width,
            height: content.height,
            depth: content.depth,
            ..MBox::default()
        };
        // A squared brace: a bar with turned-in ends and a centre point.
        let (bar_y, end_y, mid_y) = if over {
            let bar = -(content.height + gap + tick);
            out.height = content.height + gap + tick + tick;
            (bar, bar, bar - tick)
        } else {
            let bar = content.depth + gap + tick;
            out.depth = bar + t + tick;
            (bar, bar - tick + t, bar + t)
        };
        out.place(content, 0.0, 0.0);
        out.rule(0.0, bar_y, width, t);
        out.rule(0.0, end_y, t, tick);
        out.rule(width - t, end_y, t, tick);
        out.rule((width - t) / 2.0, mid_y, t, tick);
        out
    }

    fn xarrow(&self, left: bool, over: Option<&Node>, under: Option<&Node>, st: St) -> MBox {
        let size = st.size;
        let over = over.map(|n| self.node(n, st.script(self.root, false)).0);
        let under = under.map(|n| self.node(n, st.script(self.root, true)).0);
        let label = over
            .as_ref()
            .map_or(0.0, |b| b.width)
            .max(under.as_ref().map_or(0.0, |b| b.width));
        let head_text = if left { "←" } else { "→" };
        let mut head = self.glyph(head_text, size, false, false);
        let width = (label + 0.6 * size).max(head.width);
        let mut arrow = MBox {
            width,
            height: head.height,
            depth: head.depth,
            ..MBox::default()
        };
        let shaft = width - head.width * 0.8;
        let t = self.rule_thickness(size) * 0.9;
        let (shaft_x, head_x) = if left {
            (head.width * 0.8, 0.0)
        } else {
            (0.0, width - head.width)
        };
        arrow.rule(shaft_x, -(AXIS * size + t / 2.0), shaft, t);
        head.italic = 0.0;
        arrow.place(head, head_x, 0.0);
        let mut out = self.stack(arrow, over, under, size, 0.3);
        out.width += 0.1 * size;
        out
    }

    fn boxed(&self, body: &Node, st: St) -> MBox {
        let size = st.size;
        let content = self.node(body, st).0;
        let t = self.rule_thickness(size);
        let sep = 0.3 * size;
        let width = content.width + 2.0 * (sep + t);
        let height = content.height + sep + t;
        let depth = content.depth + sep + t;
        let mut out = MBox {
            width,
            height,
            depth,
            ..MBox::default()
        };
        out.place(content, sep + t, 0.0);
        out.rule(0.0, -height, width, t);
        out.rule(0.0, depth - t, width, t);
        out.rule(0.0, -height, t, height + depth);
        out.rule(width - t, -height, t, height + depth);
        out
    }

    /// Converts the root box, shifting content right of any negative overhang and
    /// widening the extents to cover every item's full box.
    fn finish(&self, root: MBox) -> Layout {
        let min_x = root
            .items
            .iter()
            .map(|item| match item {
                Placed::Glyphs { x, .. } | Placed::Rule { x, .. } | Placed::Radical { x, .. } => *x,
            })
            .fold(0.0_f32, f32::min);
        let shift = -min_x;
        let mut width = root.width.max(0.0) + shift;
        let mut ascent = root.height.max(0.0);
        let mut descent = root.depth.max(0.0);
        let mut items = Vec::with_capacity(root.items.len());
        for mut placed in root.items {
            placed.shift(shift, 0.0);
            let item = match placed {
                Placed::Glyphs {
                    text,
                    x,
                    baseline,
                    size,
                    italic,
                    bold,
                    advance,
                } => {
                    width = width.max(x + advance);
                    ascent = ascent.max(size * self.ascent - baseline);
                    descent = descent.max(baseline + size * self.descent);
                    Item::Glyphs {
                        text,
                        x,
                        baseline,
                        size,
                        italic,
                        bold,
                    }
                }
                Placed::Rule {
                    x,
                    y,
                    width: w,
                    height: h,
                } => {
                    width = width.max(x + w);
                    ascent = ascent.max(-y);
                    descent = descent.max(y + h);
                    Item::Rule {
                        x,
                        y,
                        width: w,
                        height: h,
                    }
                }
                Placed::Radical {
                    x,
                    y,
                    width: w,
                    height: h,
                } => {
                    width = width.max(x + w);
                    ascent = ascent.max(-y);
                    descent = descent.max(y + h);
                    Item::Radical {
                        x,
                        y,
                        width: w,
                        height: h,
                    }
                }
            };
            items.push(item);
        }
        Layout {
            width,
            ascent,
            descent,
            items,
        }
    }
}

/// The vertical band a drawn delimiter stroke occupies, in px.
#[derive(Clone, Copy)]
struct Band {
    top: f32,
    total: f32,
    x0: f32,
    span: f32,
    /// Flip horizontally within `[x0, x0 + span]` for closing delimiters.
    mirror: bool,
}

/// Approximates a curved stroke by stacked rules. For `v` in `[-1, 1]` from top to
/// bottom, `shape(v)` gives the stroke's offset from the band's outer edge and its
/// thickness.
fn curve(out: &mut MBox, band: Band, shape: impl Fn(f32) -> (f32, f32)) {
    // About one rule per pixel, bounded so huge delimiters stay cheap.
    let steps = band.total.clamp(16.0, 240.0) as usize;
    let step = band.total / steps as f32;
    let bottom = band.top + band.total;
    for i in 0..steps {
        let v0 = -1.0 + 2.0 * i as f32 / steps as f32;
        let v1 = -1.0 + 2.0 * (i + 1) as f32 / steps as f32;
        let (a, thick_a) = shape(v0);
        let (b, thick_b) = shape(v1);
        let left = a.min(b);
        let width = (a - b).abs() + thick_a.max(thick_b);
        let x = if band.mirror {
            band.x0 + band.span - left - width
        } else {
            band.x0 + left
        };
        let y = band.top + step * i as f32;
        // Overlap neighbours slightly so antialiasing leaves no seams.
        let height = (step * 1.5).min(bottom - y);
        out.rule(x, y, width, height);
    }
}

/// Big operators grow in display style; integrals more, as their text glyph is slim.
fn op_scale(op: &OpNode, st: St) -> f32 {
    match (st.level == Level::Display, op.integral) {
        (true, true) => 1.8,
        (true, false) => 1.4,
        (false, true) => 1.15,
        (false, false) => 1.0,
    }
}

/// TeX's delimiter size for content of the given extent: cover at least 90% of it
/// and fall at most 0.5 em short.
fn delimiter_target(height: f32, depth: f32, size: f32) -> f32 {
    let axis = AXIS * size;
    let delta = (height - axis).max(depth + axis);
    (2.0 * delta * 0.901).max(2.0 * delta - 0.5 * size)
}

/// Shifts a single-glyph box vertically so its ink is centred on the math axis.
fn center_on_axis(content: &mut MBox, size: f32) {
    let shift = (content.height - content.depth) / 2.0 - AXIS * size;
    for item in &mut content.items {
        item.shift(0.0, shift);
    }
    content.height -= shift;
    content.depth += shift;
}

/// Moves content that starts left of the origin back to it, widening the box.
fn normalize_left(content: &mut MBox) {
    let min_x = content
        .items
        .iter()
        .map(|item| match item {
            Placed::Glyphs { x, .. } | Placed::Rule { x, .. } | Placed::Radical { x, .. } => *x,
        })
        .fold(0.0_f32, f32::min);
    let mut right = content.width;
    for item in &mut content.items {
        item.shift(-min_x, 0.0);
        if let Placed::Glyphs { x, advance, .. } = item {
            right = right.max(*x + *advance);
        }
    }
    content.width = right.max(content.width - min_x);
}

/// Approximate ink height and depth in em of a character in a typical text face.
/// Placement follows ink, not line boxes, as TeX's does.
fn ink(c: char, italic: bool) -> (f32, f32) {
    match c {
        'b' | 'd' | 'h' | 'k' | 'l' => (0.69, 0.0),
        'f' => (0.69, if italic { 0.21 } else { 0.0 }),
        'i' => (0.66, 0.0),
        'j' => (0.66, 0.21),
        't' => (0.59, 0.0),
        'g' | 'p' | 'q' | 'y' => (0.45, 0.21),
        'a'..='z' => (0.45, 0.0),
        'Q' => (0.68, 0.2),
        'A'..='Z' | '0'..='9' => (0.68, 0.0),
        'β' | 'ζ' | 'ξ' | 'ϕ' => (0.69, 0.21),
        'δ' | 'θ' | 'λ' | 'ϑ' | 'ℓ' | '∂' => (0.69, 0.0),
        'γ' | 'η' | 'μ' | 'ρ' | 'φ' | 'χ' | 'ψ' | 'ϱ' | 'ς' => (0.45, 0.21),
        'α'..='ω' | 'ϵ' | 'ϖ' | 'ϰ' | 'ı' | 'ȷ' => (0.45, 0.0),
        'Α'..='Ω' => (0.68, 0.0),
        '(' | ')' | '[' | ']' | '{' | '}' | '|' | '‖' | '⟨' | '⟩' | '⌊' | '⌋' | '⌈' | '⌉' | '/'
        | '\\' => (0.72, 0.22),
        '∑' | '∏' | '∐' | '⋃' | '⋂' | '⨁' | '⨂' | '⨀' | '⋁' | '⋀' | '⨆' | '⨄' => {
            (0.7, 0.22)
        }
        '∫' | '∬' | '∭' | '∮' | '∯' => (0.9, 0.12),
        '+' | '±' | '∓' | '÷' | '×' => (0.58, 0.0),
        '−' | '·' | '∗' | '∘' | '•' | '⋆' | '⋯' => (0.4, 0.0),
        '=' | '≡' | '≈' | '∼' | '≃' | '≅' | '∝' | '∞' | '≔' => (0.5, 0.0),
        '<' | '>' | '≤' | '≥' | '≠' | '⊂' | '⊃' | '⊆' | '⊇' | '∈' | '∉' | '∋' | '∪' | '∩' | '≪'
        | '≫' | '∧' | '∨' | '⊕' | '⊗' => (0.6, 0.1),
        '→' | '←' | '↔' | '⇒' | '⇐' | '⇔' | '↦' | '⟶' | '⟹' | '⟺' | '⟵' | '⟸' | '⟷' | '⟼' => {
            (0.47, 0.0)
        }
        ',' => (0.1, 0.17),
        ';' => (0.45, 0.17),
        ':' => (0.45, 0.0),
        '.' | '…' => (0.1, 0.0),
        '′' | '″' | '‴' | '\'' | '"' | 'ˆ' | '˜' | '˙' | '¨' | 'ˇ' | '˘' | '´' | '`' | '˚' => {
            (0.69, 0.0)
        }
        '⋮' | '⋱' => (0.72, 0.05),
        ' ' | '\u{a0}' => (0.0, 0.0),
        _ => (0.7, 0.05),
    }
}

/// Space before italic letters whose descender reaches left of their origin, so
/// `\sum f` or `(j` do not collide.
fn left_overhang(c: char) -> f32 {
    match c {
        'f' => 0.08,
        'j' => 0.05,
        _ => 0.0,
    }
}

/// Extra advance for italic letters whose top overhangs their advance width.
fn italic_correction(c: char) -> f32 {
    match c {
        'f' => 0.1,
        'F' | 'H' | 'I' | 'J' | 'K' | 'M' | 'N' | 'T' | 'U' | 'V' | 'W' | 'X' | 'Y' | 'Z' => 0.05,
        'd' | 'l' | 'β' | 'ζ' | 'ξ' | 'δ' | 'θ' => 0.02,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const EPS: f32 = 1e-3;
    const SIZE: f32 = 20.0;
    /// Times New Roman line metrics.
    const METRICS: Metrics = Metrics {
        ascent: 0.891,
        descent: 0.216,
    };

    /// Times New Roman advances for printable ASCII, regular and italic, in em.
    const TIMES: [[f32; 95]; 2] = [
        [
            0.250, 0.333, 0.408, 0.500, 0.500, 0.833, 0.778, 0.180, 0.333, 0.333, 0.500, 0.564,
            0.250, 0.333, 0.250, 0.278, 0.500, 0.500, 0.500, 0.500, 0.500, 0.500, 0.500, 0.500,
            0.500, 0.500, 0.278, 0.278, 0.564, 0.564, 0.564, 0.444, 0.921, 0.722, 0.667, 0.667,
            0.722, 0.611, 0.556, 0.722, 0.722, 0.333, 0.389, 0.722, 0.611, 0.889, 0.722, 0.722,
            0.556, 0.722, 0.667, 0.556, 0.611, 0.722, 0.722, 0.944, 0.722, 0.722, 0.611, 0.333,
            0.278, 0.333, 0.469, 0.500, 0.333, 0.444, 0.500, 0.444, 0.500, 0.444, 0.333, 0.500,
            0.500, 0.278, 0.278, 0.500, 0.278, 0.778, 0.500, 0.500, 0.500, 0.500, 0.333, 0.389,
            0.278, 0.500, 0.500, 0.722, 0.500, 0.500, 0.444, 0.480, 0.200, 0.480, 0.541,
        ],
        [
            0.250, 0.333, 0.420, 0.500, 0.500, 0.833, 0.778, 0.214, 0.333, 0.333, 0.500, 0.675,
            0.250, 0.333, 0.250, 0.278, 0.500, 0.500, 0.500, 0.500, 0.500, 0.500, 0.500, 0.500,
            0.500, 0.500, 0.333, 0.333, 0.675, 0.675, 0.675, 0.500, 0.920, 0.611, 0.611, 0.667,
            0.722, 0.611, 0.611, 0.722, 0.722, 0.333, 0.444, 0.667, 0.556, 0.833, 0.667, 0.722,
            0.611, 0.722, 0.611, 0.500, 0.556, 0.722, 0.611, 0.833, 0.611, 0.556, 0.556, 0.389,
            0.278, 0.389, 0.422, 0.500, 0.333, 0.500, 0.500, 0.444, 0.500, 0.444, 0.278, 0.500,
            0.500, 0.278, 0.278, 0.444, 0.278, 0.722, 0.500, 0.500, 0.500, 0.500, 0.389, 0.389,
            0.278, 0.500, 0.444, 0.667, 0.444, 0.444, 0.389, 0.400, 0.275, 0.400, 0.541,
        ],
    ];

    fn char_width(c: char, italic: bool) -> f32 {
        if (' '..='~').contains(&c) {
            return TIMES[usize::from(italic)][c as usize - 32];
        }
        symbol_width(c, italic)
    }

    fn symbol_width(c: char, italic: bool) -> f32 {
        if italic {
            let width = match c {
                'α' => 0.525,
                'β' => 0.498,
                'γ' => 0.394,
                'δ' => 0.465,
                'ε' => 0.394,
                'ζ' => 0.405,
                'η' => 0.496,
                'θ' => 0.491,
                'ι' => 0.278,
                'κ' => 0.480,
                'λ' => 0.433,
                'μ' => 0.502,
                'ν' => 0.444,
                'ξ' => 0.428,
                'ο' => 0.500,
                'π' => 0.501,
                'ρ' => 0.480,
                'σ' => 0.493,
                'τ' => 0.358,
                'υ' => 0.459,
                'φ' => 0.553,
                'χ' => 0.440,
                'ψ' => 0.621,
                'ω' => 0.706,
                'ϵ' => 0.403,
                'ϕ' => 0.518,
                'ϑ' => 0.491,
                _ => 0.0,
            };
            if width > 0.0 {
                return width;
            }
        }
        match c {
            'α' => 0.524,
            'β' => 0.509,
            'γ' => 0.442,
            'δ' => 0.471,
            'ε' => 0.420,
            'ζ' => 0.414,
            'η' => 0.523,
            'θ' => 0.479,
            'ι' => 0.269,
            'κ' => 0.504,
            'λ' => 0.485,
            'μ' => 0.536,
            'ν' => 0.452,
            'ξ' => 0.446,
            'ο' => 0.500,
            'π' => 0.505,
            'ρ' => 0.499,
            'σ' => 0.539,
            'τ' => 0.402,
            'υ' => 0.495,
            'φ' => 0.577,
            'χ' => 0.444,
            'ψ' => 0.626,
            'ω' => 0.658,
            'ϵ' => 0.405,
            'ϕ' => 0.526,
            'ϑ' => 0.495,
            'Γ' => 0.578,
            'Δ' => 0.643,
            'Θ' => 0.722,
            'Λ' => 0.725,
            'Ξ' => 0.643,
            'Π' => 0.722,
            'Σ' => 0.582,
            'Υ' => 0.722,
            'Φ' => 0.731,
            'Ψ' => 0.738,
            'Ω' => 0.743,
            '∑' => 0.713,
            '∏' => 0.823,
            '∫' => 0.274,
            '∬' => 0.915,
            '∮' => 0.678,
            '⋃' => 0.641,
            '⋂' => 0.641,
            '∞' => 0.713,
            '∂' => 0.494,
            '∇' => 0.619,
            '∀' => 0.630,
            '∃' => 0.526,
            '∈' => 0.576,
            '∉' => 0.612,
            '⊂' => 0.632,
            '⊆' => 0.663,
            '∪' => 0.531,
            '∩' => 0.722,
            '∧' => 0.594,
            '∨' => 0.594,
            '¬' => 0.564,
            '×' => 0.564,
            '÷' => 0.549,
            '±' => 0.549,
            '∓' => 0.563,
            '·' => 0.333,
            '∗' => 0.523,
            '∘' => 0.355,
            '≤' => 0.549,
            '≥' => 0.549,
            '≠' => 0.549,
            '≈' => 0.549,
            '≡' => 0.564,
            '∼' => 0.550,
            '∝' => 0.519,
            '→' => 1.000,
            '←' => 1.000,
            '↔' => 1.000,
            '⇒' => 0.867,
            '⇔' => 0.867,
            '⟹' => 1.066,
            '⟺' => 1.130,
            '↦' => 0.602,
            '′' => 0.219,
            'ˆ' => 0.333,
            '˜' => 0.333,
            '˙' => 0.333,
            '¨' => 0.333,
            'ˇ' => 0.333,
            '˘' => 0.333,
            '´' => 0.333,
            '˚' => 0.333,
            '…' => 1.000,
            '⋯' => 1.000,
            '⋮' => 0.411,
            '⋱' => 0.939,
            '‖' => 0.281,
            '⟨' => 0.366,
            '⟩' => 0.366,
            '⌊' => 0.602,
            '⌋' => 0.602,
            '⌈' => 0.602,
            '⌉' => 0.602,
            'ℝ' => 0.602,
            'ℤ' => 0.602,
            'ℕ' => 0.602,
            'ℚ' => 0.778,
            'ℂ' => 0.722,
            'ℒ' => 0.691,
            '𝔸' => 0.647,
            '⊤' => 0.750,
            'ℓ' => 0.479,
            'ℏ' => 0.604,
            '∅' => 0.717,
            '−' => 0.564,
            '(' | ')' | '[' | ']' => 0.333,
            '{' | '}' => 0.48,
            _ => 0.7,
        }
    }

    fn times(text: &str, size: f32, italic: bool) -> f32 {
        text.chars().map(|c| char_width(c, italic)).sum::<f32>() * size
    }

    fn lay(tex: &str, style: Style) -> Layout {
        lay_at(tex, style, SIZE)
    }

    fn lay_at(tex: &str, style: Style, size: f32) -> Layout {
        let formula = parse(tex).unwrap_or_else(|| panic!("failed to parse {tex:?}"));
        formula.layout(style, size, METRICS, times)
    }

    struct Run<'a> {
        text: &'a str,
        x: f32,
        baseline: f32,
        size: f32,
        italic: bool,
        bold: bool,
    }

    fn runs(layout: &Layout) -> Vec<Run<'_>> {
        layout
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Glyphs {
                    text,
                    x,
                    baseline,
                    size,
                    italic,
                    bold,
                } => Some(Run {
                    text,
                    x: *x,
                    baseline: *baseline,
                    size: *size,
                    italic: *italic,
                    bold: *bold,
                }),
                _ => None,
            })
            .collect()
    }

    fn run<'a>(layout: &'a Layout, text: &str) -> Run<'a> {
        runs(layout)
            .into_iter()
            .find(|run| run.text == text)
            .unwrap_or_else(|| panic!("no run {text:?} in {layout:#?}"))
    }

    fn rules(layout: &Layout) -> Vec<(f32, f32, f32, f32)> {
        layout
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Rule {
                    x,
                    y,
                    width,
                    height,
                } => Some((*x, *y, *width, *height)),
                _ => None,
            })
            .collect()
    }

    fn assert_bounded(tex: &str, layout: &Layout) {
        assert!(layout.width.is_finite() && layout.width >= 0.0, "{tex}");
        assert!(
            layout.ascent.is_finite() && layout.descent.is_finite(),
            "{tex}"
        );
        let within = |left: f32, right: f32, top: f32, bottom: f32| {
            left >= -EPS
                && right <= layout.width + EPS
                && top >= -layout.ascent - EPS
                && bottom <= layout.descent + EPS
        };
        for item in &layout.items {
            let ok = match item {
                Item::Glyphs {
                    text,
                    x,
                    baseline,
                    size,
                    italic,
                    ..
                } => {
                    let advance = times(text, *size, *italic);
                    !text.is_empty()
                        && *size > 0.0
                        && within(
                            *x,
                            x + advance,
                            baseline - size * METRICS.ascent,
                            baseline + size * METRICS.descent,
                        )
                }
                Item::Rule {
                    x,
                    y,
                    width,
                    height,
                }
                | Item::Radical {
                    x,
                    y,
                    width,
                    height,
                } => *width > 0.0 && *height > 0.0 && within(*x, x + width, *y, y + height),
            };
            assert!(ok, "{tex}: {item:?} outside {layout:?}");
        }
    }

    const SAMPLES: &[&str] = &[
        r"\sum_{i=1}^{n} f(x_i)\,\Delta x = \int_a^b f(x)\,dx",
        r"x = \frac{-b \pm \sqrt{b^2 - 4ac}}{2a}",
        r"\left( \frac{a}{b} \right)^2 + \binom{n}{k} \ne \sqrt[3]{x^2+y^2}",
        r"f(x) = \begin{cases} x^2 & x \ge 0 \\ -x & \text{otherwise} \end{cases}",
        r"A = \begin{pmatrix} 1 & 2 \\ 3 & 4 \end{pmatrix}, \quad \begin{vmatrix} a & b \\ c & d \end{vmatrix} = ad - bc",
        r"\begin{aligned} (a+b)^2 &= a^2 + 2ab + b^2 \\ &\le 2\left(a^2+b^2\right) \end{aligned}",
        r"\lim_{x \to 0} \frac{\sin x}{x} = 1, \quad \hat{\theta}, \bar{x}, \vec{v}, \overline{AB}, \mathbb{R}^n, \mathcal{L}, f'(x), \dot x",
        r"e^{i\pi} + 1 = 0, \; \nabla \cdot \mathbf{E} = \frac{\rho}{\varepsilon_0}, \; \forall \epsilon > 0\ \exists \delta",
        r"\left\{ x \in \mathbb{Z} \;\middle|\; x^2 < 10 \right\} \cup \left[ \int_0^1 \frac{dx}{\sqrt{1-x^2}} \right] \cdot \prod_{p} \frac{1}{1-p^{-s}}",
        r"\mathbf{x}^\top A \mathbf{x} \geq 0,\ \|v\|_2 = \sqrt{\sum_k v_k^2},\ \operatorname{rank}(A) \le \min(m, n), \ \underbrace{a+\cdots+a}_{n}",
        r"\begin{bmatrix} \cos\theta & -\sin\theta \\ \sin\theta & \cos\theta \end{bmatrix} \begin{bmatrix} x \\ y \end{bmatrix}, \quad \Pr\left[ X \ge t \right] \le \frac{\mathbb{E}[X]}{t}",
    ];

    const INLINE_SAMPLES: &[&str] = &[
        r"\frac{a}{b} + x_i^2 \le \sum_{k=1}^n k",
        r"\int_0^\infty e^{-x^2}\,dx = \frac{\sqrt{\pi}}{2}",
        r"\lim_{n\to\infty} \left(1 + \frac{1}{n}\right)^n = e",
    ];

    #[test]
    fn fraction_places_numerator_above_and_denominator_below_the_bar() {
        for style in [Style::Display, Style::Inline] {
            let layout = lay(r"\frac{a}{b}", style);
            let a = run(&layout, "a");
            let b = run(&layout, "b");
            let bars = rules(&layout);
            assert_eq!(bars.len(), 1);
            let (bar_x, bar_y, bar_width, bar_height) = bars[0];
            assert!(a.baseline < bar_y, "numerator baseline above bar");
            assert!(b.baseline > bar_y + bar_height, "denominator below bar");
            // Ink of the numerator clears the bar; denominator's top clears it too.
            assert!(b.baseline - 0.45 * b.size > bar_y + bar_height);
            assert!(bar_x >= 0.0 && bar_width >= times("a", a.size, true));
            // The bar sits on the math axis.
            let centre = -(bar_y + bar_height / 2.0);
            assert!((centre - AXIS * SIZE).abs() < 0.5);
        }
        let inline = lay(r"\frac{a}{b}", Style::Inline);
        let display = lay(r"\frac{a}{b}", Style::Display);
        assert!((run(&display, "a").size - SIZE).abs() < EPS);
        assert!((run(&inline, "a").size - 0.8 * SIZE).abs() < EPS);
    }

    #[test]
    fn scripts_are_raised_lowered_and_shrunk() {
        let layout = lay("x^2", Style::Inline);
        let two = run(&layout, "2");
        assert!(two.baseline < -0.3 * SIZE);
        assert!((two.size - 0.7 * SIZE).abs() < EPS);
        assert!(two.x >= times("x", SIZE, true) - EPS);

        let layout = lay("x_i", Style::Inline);
        assert!(run(&layout, "i").baseline > 0.1 * SIZE);

        let layout = lay("x_i^2", Style::Inline);
        let (i, two) = (run(&layout, "i"), run(&layout, "2"));
        assert!(i.baseline > 0.0 && two.baseline < 0.0);
        assert!((i.x - two.x).abs() < 0.2 * SIZE);

        // Nested scripts keep shrinking but never below half size.
        let layout = lay("e^{x^{y^{z}}}", Style::Inline);
        let (x, y, z) = (run(&layout, "x"), run(&layout, "y"), run(&layout, "z"));
        assert!(x.size > y.size - EPS && y.size >= z.size - EPS);
        assert!(z.size >= 0.5 * SIZE - EPS);
        assert!(z.baseline < y.baseline && y.baseline < x.baseline);
    }

    #[test]
    fn primes_follow_the_base_at_full_size() {
        let layout = lay("f'(x)", Style::Inline);
        let prime = run(&layout, "′");
        assert!((prime.size - SIZE).abs() < EPS);
        assert!(prime.baseline.abs() < EPS);
        let layout = lay("x''^2", Style::Inline);
        let (primes, two) = (run(&layout, "′′"), run(&layout, "2"));
        assert!(two.x >= primes.x + times("′′", SIZE, false) - EPS);
    }

    #[test]
    fn display_sum_places_limits_above_and_below_centred() {
        let layout = lay(r"\sum_{i=1}^{n} i", Style::Display);
        let sum = run(&layout, "∑");
        let n = run(&layout, "n");
        let lower = run(&layout, "1");
        assert!((sum.size - 1.4 * SIZE).abs() < EPS);
        let sum_width = times("∑", sum.size, false);
        let sum_centre = sum.x + sum_width / 2.0;
        let n_centre = n.x + times("n", n.size, true) / 2.0;
        assert!((n_centre - sum_centre).abs() < 0.1 * SIZE);
        assert!(n.baseline < sum.baseline - 0.6 * sum.size);
        assert!(lower.baseline > sum.baseline + 0.2 * sum.size);

        let inline = lay(r"\sum_{i=1}^{n} i", Style::Inline);
        let sum = run(&inline, "∑");
        let n = run(&inline, "n");
        assert!(n.x > sum.x + times("∑", sum.size, false) - EPS);

        // Integrals keep scripts at the side even in display style.
        let layout = lay(r"\int_a^b x", Style::Display);
        let int = run(&layout, "∫");
        assert!(run(&layout, "b").x > int.x);
        assert!(run(&layout, "a").x > int.x);
        // `\limits` forces limits back.
        let layout = lay(r"\int\limits_a^b x", Style::Display);
        let int = run(&layout, "∫");
        assert!(run(&layout, "b").x < int.x + times("∫", int.size, false));
    }

    #[test]
    fn function_limits_follow_style() {
        let display = lay(r"\lim_{x \to 0} x", Style::Display);
        let lim = run(&display, "lim");
        assert!(!lim.italic);
        assert!(run(&display, "0").baseline > 0.5 * SIZE);
        let inline = lay(r"\lim_{x \to 0} x", Style::Inline);
        assert!(run(&inline, "0").x > run(&inline, "lim").x + times("lim", SIZE, false) - EPS);
    }

    fn gap_after(layout: &Layout, left: &str, right: &str) -> f32 {
        let l = run(layout, left);
        let r = run(layout, right);
        r.x - (l.x + times(l.text, l.size, l.italic))
    }

    #[test]
    fn spacing_follows_atom_classes() {
        let bin = lay("a+b", Style::Inline);
        let rel = lay("a=b", Style::Inline);
        let ord = lay("ab", Style::Inline);
        assert!((gap_after(&bin, "a", "+") - MEDIUM * SIZE).abs() < 0.01);
        assert!((gap_after(&rel, "a", "=") - THICK * SIZE).abs() < 0.01);
        assert!(gap_after(&ord, "a", "b").abs() < 0.01);

        // Unary minus is ordinary: no space between it and its operand.
        let unary = lay("-a", Style::Inline);
        assert!(gap_after(&unary, "−", "a").abs() < 0.01);
        let unary = lay("(-a)", Style::Inline);
        assert!(gap_after(&unary, "−", "a").abs() < 0.01);
        let binary = lay("b-a", Style::Inline);
        assert!((gap_after(&binary, "−", "a") - MEDIUM * SIZE).abs() < 0.01);

        // Operators get a thin space before an ordinary operand.
        let op = lay(r"\sin x", Style::Inline);
        assert!((gap_after(&op, "sin", "x") - THIN * SIZE).abs() < 0.01);
        // Punctuation is followed by a thin space.
        let punct = lay("a,b", Style::Inline);
        assert!((gap_after(&punct, ",", "b") - THIN * SIZE).abs() < 0.01);

        // Script styles drop the conditional spaces.
        let script = lay("x^{a+b}", Style::Inline);
        assert!(gap_after(&script, "a", "+").abs() < 0.01);

        // Explicit spaces.
        let quad = lay(r"a\quad b", Style::Inline);
        assert!((gap_after(&quad, "a", "b") - SIZE).abs() < 0.01);
        let thin = lay(r"a\,b", Style::Inline);
        assert!((gap_after(&thin, "a", "b") - THIN * SIZE).abs() < 0.01);
    }

    #[test]
    fn symbols_map_to_unicode_and_styles() {
        let layout = lay(
            r"\alpha \Gamma x 2 - \infty \mathbb{R} \mathcal{L} \mathbf{v}",
            Style::Inline,
        );
        assert!(run(&layout, "α").italic);
        assert!(!run(&layout, "Γ").italic);
        assert!(run(&layout, "x").italic);
        assert!(!run(&layout, "2").italic);
        run(&layout, "−");
        run(&layout, "∞");
        run(&layout, "ℝ");
        run(&layout, "ℒ");
        let v = run(&layout, "v");
        assert!(v.bold && !v.italic);
        let layout = lay(r"\mathbb{A}\mathrm{d}x", Style::Inline);
        run(&layout, "𝔸");
        assert!(!run(&layout, "d").italic);
        let layout = lay(r"\text{if } x \operatorname{rank} A", Style::Inline);
        assert!(!run(&layout, "if ").italic);
        assert!(!run(&layout, "rank").italic);
        let layout = lay(r"\{ \} \| \% \# \& \_", Style::Inline);
        for text in ["{", "}", "‖", "%", "#", "&", "_"] {
            run(&layout, text);
        }
        let layout = lay(r"a \not= b \not\in c", Style::Inline);
        run(&layout, "≠");
        run(&layout, "∉");
    }

    #[test]
    fn matrices_align_rows_and_columns() {
        let layout = lay(
            r"\begin{pmatrix} 1 & 22 \\ 333 & 4 \end{pmatrix}",
            Style::Display,
        );
        let (one, two, three, four) = (
            run(&layout, "1"),
            run(&layout, "2"),
            run(&layout, "3"),
            run(&layout, "4"),
        );
        assert!((one.baseline - two.baseline).abs() < EPS);
        assert!((three.baseline - four.baseline).abs() < EPS);
        assert!(three.baseline > one.baseline);
        // Centred columns: the midpoints of `1` and `333` coincide.
        let mid = |r: &Run<'_>, n: f32| r.x + n * times(r.text, r.size, r.italic) / 2.0;
        assert!((mid(&one, 1.0) - mid(&three, 3.0)).abs() < 0.01);
        // The matrix is centred on the axis: each row box spans 0.84 em above and
        // 0.36 em below its baseline, so row centres sit 0.24 em above baselines.
        let centre = (one.baseline + three.baseline) / 2.0 - 0.24 * SIZE;
        assert!((centre + AXIS * SIZE).abs() < 0.5);
        // Parentheses tall enough for two rows are drawn from rules.
        assert!(rules(&layout).len() > 20);

        let aligned = lay(
            r"\begin{aligned} a &= b + c \\ ccc &= d \end{aligned}",
            Style::Display,
        );
        let equals: Vec<_> = runs(&aligned)
            .into_iter()
            .filter(|r| r.text == "=")
            .collect();
        assert_eq!(equals.len(), 2);
        assert!((equals[0].x - equals[1].x).abs() < EPS);
        // Right-aligned first column: `a` and `ccc` end at the same x.
        let a = run(&aligned, "a");
        let end_a = a.x + times("a", a.size, true);
        let last_c = runs(&aligned)
            .into_iter()
            .filter(|r| r.text == "c" && r.baseline > a.baseline)
            .map(|r| r.x + times("c", r.size, true))
            .fold(0.0, f32::max);
        assert!((end_a - last_c).abs() < 0.01);
        // `&=` still gets relation spacing on both sides.
        assert!((gap_after(&aligned, "a", "=") - THICK * SIZE).abs() < 0.01);

        let cases = lay(
            r"\begin{cases} 1 & x > 0 \\ 22 & x \le 0 \end{cases}",
            Style::Display,
        );
        let one = run(&cases, "1");
        let two = runs(&cases).into_iter().find(|r| r.text == "2").unwrap();
        assert!(
            (one.x - two.x).abs() < EPS,
            "cases columns are left-aligned"
        );

        let array = lay(
            r"\begin{array}{r|l} 1 & 2 \\ 333 & 4 \end{array}",
            Style::Display,
        );
        let (one, three) = (run(&array, "1"), run(&array, "3"));
        let end = |r: &Run<'_>, n: usize| r.x + times(r.text, r.size, r.italic) * n as f32;
        assert!((end(&one, 1) - end(&three, 3)).abs() < 0.01);
    }

    #[test]
    fn delimiters_grow_with_content() {
        let plain = lay(r"\left( x \right)", Style::Display);
        let open = run(&plain, "(");
        assert!((open.size - SIZE).abs() < EPS);
        let tall = lay(
            r"\left( \frac{\frac{a}{b}}{\frac{c}{d}} \right)",
            Style::Display,
        );
        let content_height = tall.ascent + tall.descent;
        let paren_rules = rules(&tall).len();
        assert!(paren_rules > 20 && content_height > 2.0 * SIZE);
        let big = lay(r"\big( x \big)", Style::Inline);
        assert!(run(&big, "(").size > SIZE);
        let middle = lay(r"\left\{ \frac{a}{b} \middle| c \right\}", Style::Display);
        assert!(
            !runs(&middle).iter().any(|r| r.text == "|"),
            "middle bar is drawn"
        );
    }

    #[test]
    fn radicals_cover_their_body() {
        let layout = lay(r"\sqrt{x}", Style::Inline);
        let radical = layout
            .items
            .iter()
            .find_map(|item| match item {
                Item::Radical {
                    x,
                    y,
                    width,
                    height,
                } => Some((*x, *y, *width, *height)),
                _ => None,
            })
            .unwrap();
        let x = run(&layout, "x");
        let (bar_x, bar_y, bar_width, _) = rules(&layout)[0];
        assert!(x.x >= radical.0 + radical.2);
        assert!((bar_x - (radical.0 + radical.2)).abs() < EPS);
        assert!(bar_x + bar_width >= x.x + times("x", SIZE, true));
        assert!(bar_y < x.baseline - 0.45 * SIZE);
        assert!(radical.1 + radical.3 >= x.baseline - EPS);

        let indexed = lay(r"\sqrt[3]{x}", Style::Inline);
        let three = run(&indexed, "3");
        assert!(three.size < SIZE && three.baseline < 0.0);
    }

    #[test]
    fn accents_sit_above_their_base() {
        let layout = lay(
            r"\hat{x} \bar{y} \vec{v} \overline{AB} \tilde{n}",
            Style::Inline,
        );
        let x = run(&layout, "x");
        let hat = run(&layout, "ˆ");
        assert!(hat.x > x.x - EPS && hat.x < x.x + times("x", SIZE, true));
        let arrow = run(&layout, "→");
        assert!(arrow.baseline < -0.3 * SIZE);
        assert!(rules(&layout).len() >= 2);
        assert!(runs(&layout).iter().any(|r| r.text == "˜"));
    }

    #[test]
    fn unknown_commands_render_their_name() {
        let layout = lay(r"\foo{x} + y", Style::Inline);
        let foo = run(&layout, r"\foo");
        assert!(!foo.italic);
        assert!(parse(r"\foo").is_none());
        assert!(parse(r"\foo \bar \baz x").is_none());
        assert!(parse(r"\text{plain words}").is_some());
    }

    #[test]
    fn malformed_and_oversized_input_is_rejected() {
        for tex in [
            "",
            "  ",
            r"\frac{",
            "}}}",
            "^^",
            r"\left(",
            r"\right)",
            "x^2^3",
            "x_1_2",
            r"\begin{matrix} a",
            r"\begin{nope} a \end{nope}",
            r"\end{matrix}",
            "{",
            r"\sqrt[3",
            r"\left( x \right",
            r"\",
            r"\left( x",
            "a & b }",
            r"\begin{pmatrix}1\end{bmatrix}",
        ] {
            assert!(parse(tex).is_none(), "{tex:?} should be rejected");
        }
        let deep = format!("{}x{}", "{".repeat(40), "}".repeat(40));
        assert!(parse(&deep).is_none());
        let shallow = format!("{}x{}", "{".repeat(20), "}".repeat(20));
        assert!(parse(&shallow).is_some());
        let long = "x+".repeat(2100);
        assert!(parse(&long).is_none());
        let scripts = format!("x{}", "^{x".repeat(40)) + &"}".repeat(40);
        assert!(parse(&scripts).is_none());
    }

    #[test]
    fn bare_alignment_markers_become_an_aligned_block() {
        let layout = lay(r"a &= b \\ cc &= d", Style::Display);
        let equals: Vec<_> = runs(&layout)
            .into_iter()
            .filter(|r| r.text == "=")
            .collect();
        assert_eq!(equals.len(), 2);
        assert!((equals[0].x - equals[1].x).abs() < EPS);
    }

    #[test]
    fn every_layout_stays_within_its_bounds() {
        let extra = [
            r"\boxed{x^2} \phantom{y} \hphantom{y} \vphantom{\frac{a}{b}}",
            r"\overset{!}{=} \underset{n}{\to} \stackrel{def}{=} \xrightarrow[below]{above}",
            r"\!\!\!\!x",
            r"\hspace{-3em}x",
            r"\overbrace{a+b}^{2}",
            r"\dfrac{1}{2} \tfrac{1}{2}",
            r"\displaystyle\sum_i x \textstyle\sum_i x \scriptstyle x",
            r"\sqrt{\frac{a}{b}}",
            r"\left. \frac{df}{dx} \right|_{x=0}",
            r"\langle v, w \rangle \left\langle \frac{a}{b} \right\rangle",
            r"\lfloor x \rfloor \left\lceil \frac{n}{2} \right\rceil",
            r"\color{red}{x} \textcolor{blue}{y}",
            r"\begin{Vmatrix} a \end{Vmatrix} \begin{Bmatrix} a \\ b \\ c \end{Bmatrix}",
            r"\begin{smallmatrix} a & b \\ c & d \end{smallmatrix} \begin{rcases} a \\ b \end{rcases}",
            r"n \pmod{p} \quad a \bmod b",
            r"\hat{\mathbf{x}}_{i}^{(k)}",
            r"\iint_D \oint_C \bigcup_{i} A_i",
        ];
        for tex in SAMPLES.iter().chain(INLINE_SAMPLES).chain(extra.iter()) {
            for style in [Style::Display, Style::Inline] {
                let layout = lay(tex, style);
                assert!(layout.width > 0.0, "{tex}");
                assert_bounded(tex, &layout);
                assert_eq!(layout, lay(tex, style), "layout is deterministic");
            }
        }
    }

    #[test]
    fn no_panic_on_prefixes_or_noise() {
        let check = |tex: &str| {
            if let Some(formula) = parse(tex) {
                for style in [Style::Display, Style::Inline] {
                    let layout = formula.layout(style, SIZE, METRICS, times);
                    assert_bounded(tex, &layout);
                }
            }
        };
        for tex in SAMPLES.iter().chain(INLINE_SAMPLES) {
            for (end, _) in tex.char_indices() {
                check(&tex[..end]);
                check(&tex[end..]);
            }
        }
        let alphabet: Vec<char> =
            r"\{}[]()^_&'x2+-=|.,! frac sqrt left right begin end matrix cases \\ \alpha"
                .chars()
                .collect();
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        for _ in 0..4000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = (seed % 40) as usize;
            let mut tex = String::new();
            let mut state = seed;
            for _ in 0..len {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                tex.push(alphabet[(state >> 33) as usize % alphabet.len()]);
            }
            check(&tex);
        }
    }

    #[test]
    fn degenerate_sizes_and_metrics_do_not_poison_output() {
        let formula = parse(r"\frac{a}{\sqrt{b}}").unwrap();
        assert_eq!(
            formula.layout(Style::Inline, f32::NAN, METRICS, times),
            Layout::default()
        );
        assert_eq!(
            formula.layout(Style::Inline, 0.0, METRICS, times),
            Layout::default()
        );
        let odd = Metrics {
            ascent: f32::INFINITY,
            descent: -1.0,
        };
        let layout = formula.layout(Style::Display, SIZE, odd, |_, _, _| f32::NAN);
        assert!(
            layout.width.is_finite() && layout.ascent.is_finite() && layout.descent.is_finite()
        );
    }

    // --- Visual check: `MATH_SVG_DIR=/some/dir cargo test render_samples` --------

    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    fn svg_items(out: &mut String, layout: &Layout, dx: f32, dy: f32) {
        use std::fmt::Write as _;
        for item in &layout.items {
            match item {
                Item::Glyphs {
                    text,
                    x,
                    baseline,
                    size,
                    italic,
                    bold,
                } => {
                    let _ = writeln!(
                        out,
                        r#"<text x="{:.2}" y="{:.2}" font-size="{:.2}" font-family="Times New Roman" font-style="{}" font-weight="{}" xml:space="preserve">{}</text>"#,
                        dx + x,
                        dy + baseline,
                        size,
                        if *italic { "italic" } else { "normal" },
                        if *bold { "bold" } else { "normal" },
                        escape(text)
                    );
                }
                Item::Rule {
                    x,
                    y,
                    width,
                    height,
                } => {
                    let _ = writeln!(
                        out,
                        r#"<rect x="{:.2}" y="{:.2}" width="{:.2}" height="{:.2}"/>"#,
                        dx + x,
                        dy + y,
                        width,
                        height
                    );
                }
                Item::Radical {
                    x,
                    y,
                    width,
                    height,
                } => {
                    let (x, y) = (dx + x, dy + y);
                    // Per the contract, stroke as thick as the bar that continues it.
                    let stroke = layout
                        .items
                        .iter()
                        .find_map(|other| match other {
                            Item::Rule {
                                x: bar_x, height, ..
                            } if (bar_x - (x - dx + width)).abs() < 1e-3 => Some(*height),
                            _ => None,
                        })
                        .unwrap();
                    let _ = writeln!(
                        out,
                        r#"<polyline points="{:.2},{:.2} {:.2},{:.2} {:.2},{:.2}" fill="none" stroke="black" stroke-width="{stroke:.2}" stroke-linejoin="round"/>"#,
                        x,
                        y + height - (0.45 * height).min(0.9 * width),
                        x + 0.4 * width,
                        y + height,
                        x + width,
                        y
                    );
                }
            }
        }
    }

    fn svg_page(display: &[&str], inline: &[&str], size: f32) -> String {
        use std::fmt::Write as _;
        let mut body = String::new();
        let mut y = 20.0;
        let mut max_width: f32 = 0.0;
        for tex in display {
            let layout = lay_at(tex, Style::Display, size);
            y += layout.ascent + 6.0;
            let _ = writeln!(
                body,
                r##"<rect x="20" y="{:.2}" width="{:.2}" height="{:.2}" fill="none" stroke="#dde" stroke-width="0.5"/>"##,
                y - layout.ascent,
                layout.width,
                layout.ascent + layout.descent
            );
            svg_items(&mut body, &layout, 20.0, y);
            max_width = max_width.max(layout.width);
            y += layout.descent + 14.0;
        }
        for tex in inline {
            let layout = lay_at(tex, Style::Inline, 18.0);
            y += layout.ascent.max(18.0) + 8.0;
            let lead = "Inline math sits in text like ";
            let lead_width = times(lead, 18.0, false);
            let _ = writeln!(
                body,
                r#"<text x="20" y="{y:.2}" font-size="18" font-family="Times New Roman">{lead}</text>"#
            );
            svg_items(&mut body, &layout, 20.0 + lead_width, y);
            let _ = writeln!(
                body,
                r#"<text x="{:.2}" y="{y:.2}" font-size="18" font-family="Times New Roman"> and continues.</text>"#,
                20.0 + lead_width + layout.width
            );
            max_width = max_width.max(lead_width + layout.width + 150.0);
            y += layout.descent + 6.0;
        }
        let width = max_width + 40.0;
        let height = y + 20.0;
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width:.0}" height="{height:.0}" viewBox="0 0 {width:.0} {height:.0}"><rect width="100%" height="100%" fill="white"/>
{body}</svg>
"#
        )
    }

    #[test]
    fn render_samples() {
        let pages = [
            svg_page(&SAMPLES[..5], &[], 28.0),
            svg_page(&SAMPLES[5..], &[], 28.0),
            svg_page(&[], INLINE_SAMPLES, 28.0),
            svg_page(
                &[
                    r"\begin{cases} a & b \\ c & d \\ e & f \end{cases} \left\{ \frac{a}{b} \right\}",
                    r"\begin{pmatrix} a \\ b \\ c \end{pmatrix} \left\langle \frac{a}{b} \right\rangle \sqrt{\frac{a}{b}} \left\lfloor \frac{a}{b} \right\rfloor",
                ],
                &[],
                60.0,
            ),
        ];
        assert!(pages[0].contains("<polyline"));
        if let Some(dir) = std::env::var_os("MATH_SVG_DIR") {
            for (index, page) in pages.iter().enumerate() {
                let path = std::path::Path::new(&dir).join(format!("math-{index}.svg"));
                std::fs::write(path, page).unwrap();
            }
        }
    }
}
