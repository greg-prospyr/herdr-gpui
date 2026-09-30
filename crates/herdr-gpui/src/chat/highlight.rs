//! Small, fast syntax highlighting for code blocks and edit diffs in the chat view.
//!
//! This is a hand-written, line-oriented lexer rather than a grammar engine. Each
//! language family gets one linear pass per line, with a little state carried
//! across lines for constructs that span them: block comments, raw and
//! triple-quoted strings, template literals, HTML comments and open tags, TOML
//! arrays, YAML block scalars, and Markdown fences. The goal is tasteful colouring
//! of the code agents typically show, not exact parsing; odd input degrades to
//! plain text, never to a panic.
//!
//! Every delimiter the lexers split on is ASCII and every byte >= 0x80 is treated
//! as part of an identifier, string, or comment, so span boundaries land on UTF-8
//! character boundaries. `Out::emit` also rounds up to the next boundary as a
//! backstop, which keeps the "ranges cover the line exactly" contract even if a
//! lexer rule misjudges a position.

use std::ops::Range;

/// Inputs above either bound are returned as plain text so a huge paste cannot
/// stall the UI thread.
const MAX_BYTES: usize = 200_000;
const MAX_LINES: usize = 5_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Token {
    Plain,
    Keyword,
    Type,
    Function,
    String,
    Number,
    Comment,
    /// `true`, `false`, `null`, `None`, `self`, and ALL_CAPS names.
    Constant,
    Operator,
    Punctuation,
    /// `#[..]`, `@decorator`, HTML/JSX attributes, and command-line flags.
    Attribute,
    /// HTML, XML, and JSX tag names.
    Tag,
    /// JSON/YAML/TOML keys, CSS properties, and `obj.field` accesses.
    Property,
    /// Shell `$VAR`, Ruby `@ivar`, SQL bind parameters, YAML anchors.
    Variable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Language {
    Rust,
    /// Also JavaScript, JSX, and TSX.
    TypeScript,
    Python,
    Go,
    /// sh, bash, zsh, and console transcripts.
    Shell,
    Json,
    Toml,
    Yaml,
    /// Also XML, SVG, and Vue-style templates.
    Html,
    Css,
    Sql,
    /// The C family: C, C++, Java, Kotlin, Swift, C#, and friends.
    C,
    Ruby,
    Markdown,
    Diff,
    Plain,
}

impl Language {
    /// From a fence info string ("rust", "ts", "tsx", "py", "sh", "json5", "diff",
    /// "patch", ...) or a file path/extension ("src/a.rs", "Makefile",
    /// "x.test.tsx"). Case-insensitive. Unknown → Plain.
    pub(crate) fn from_hint(hint: &str) -> Self {
        // Fence info strings may carry attributes after the language, as in
        // "rust,ignore", "ts title=a.ts", or "{.python}".
        let first = hint
            .split(|c: char| c.is_whitespace() || matches!(c, ',' | '{' | '}'))
            .find(|part| !part.is_empty())
            .unwrap_or_default()
            .trim_start_matches('.');
        let name = first.rsplit(['/', '\\']).next().unwrap_or_default();
        let lower = name.to_ascii_lowercase();
        if let Some(language) = Self::from_name(&lower) {
            return language;
        }
        lower
            .rsplit_once('.')
            .and_then(|(_, extension)| Self::from_name(extension))
            .unwrap_or(Self::Plain)
    }

    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "rust" | "rs" => Self::Rust,
            "typescript" | "ts" | "tsx" | "mts" | "cts" | "javascript" | "js" | "jsx" | "mjs"
            | "cjs" | "node" => Self::TypeScript,
            "python" | "py" | "pyi" | "pyw" | "python3" | "py3" => Self::Python,
            "go" | "golang" => Self::Go,
            "shell" | "sh" | "bash" | "zsh" | "ksh" | "fish" | "console" | "terminal"
            | "shellsession" | "shell-session" | "bashrc" | "zshrc" | "profile" | "make"
            | "makefile" | "mk" | "dockerfile" | "containerfile" | "justfile" | "env" => {
                Self::Shell
            }
            "json" | "json5" | "jsonc" | "jsonl" | "ndjson" | "geojson" | "webmanifest" => {
                Self::Json
            }
            "toml" | "cargo.lock" | "pipfile" => Self::Toml,
            "yaml" | "yml" => Self::Yaml,
            "html" | "htm" | "xhtml" | "xml" | "svg" | "vue" | "svelte" | "astro" | "plist"
            | "xsd" | "xsl" | "xslt" | "rss" | "atom" => Self::Html,
            "css" | "scss" | "sass" | "less" => Self::Css,
            "sql" | "psql" | "pgsql" | "mysql" | "sqlite" | "plsql" => Self::Sql,
            "c" | "h" | "cpp" | "c++" | "cxx" | "cc" | "hpp" | "hxx" | "hh" | "java" | "kotlin"
            | "kt" | "kts" | "swift" | "cs" | "csharp" | "c#" | "objc" | "objective-c" | "m"
            | "mm" | "scala" | "dart" | "groovy" | "gradle" => Self::C,
            "ruby" | "rb" | "gemfile" | "rakefile" | "gemspec" | "rake" => Self::Ruby,
            "markdown" | "md" | "mdx" | "mkd" => Self::Markdown,
            "diff" | "patch" => Self::Diff,
            "text" | "txt" | "plain" | "plaintext" => Self::Plain,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineKind {
    Context,
    Added,
    Removed,
    /// `@@ -a,b +c,d @@` hunk headers.
    Hunk,
    /// `diff --git`, `index`, `---`, `+++`, mode and rename lines.
    Header,
}

/// Classifies one line of a unified diff.
pub(crate) fn diff_line_kind(line: &str) -> LineKind {
    const HEADERS: &[&str] = &[
        "diff ",
        "index ",
        "--- ",
        "+++ ",
        "new file mode",
        "deleted file mode",
        "old mode",
        "new mode",
        "similarity index",
        "dissimilarity index",
        "rename from",
        "rename to",
        "copy from",
        "copy to",
        "Binary files",
    ];
    if line.starts_with("@@") {
        LineKind::Hunk
    } else if HEADERS.iter().any(|header| line.starts_with(header))
        || line == "---"
        || line == "+++"
    {
        LineKind::Header
    } else if line.starts_with('+') {
        LineKind::Added
    } else if line.starts_with('-') {
        LineKind::Removed
    } else {
        LineKind::Context
    }
}

/// Tokens for each line of `code` as (byte range within that line, token). Lines
/// split on '\n' (a trailing '\r' is excluded from ranges). Ranges are contiguous
/// and cover each line exactly; adjacent same-token ranges are merged.
pub(crate) fn highlight(language: Language, code: &str) -> Vec<Vec<(Range<usize>, Token)>> {
    let oversized =
        code.len() > MAX_BYTES || code.bytes().filter(|&byte| byte == b'\n').count() >= MAX_LINES;
    let mut state = State::default();
    code.split('\n')
        .map(|raw| {
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            let mut out = Out::new(line);
            if !oversized {
                scan_line(language, &mut state, &mut out);
            }
            out.finish()
        })
        .collect()
}

fn scan_line(language: Language, state: &mut State, out: &mut Out<'_>) {
    match language {
        Language::Rust => scan_code(&RUST, state, out),
        Language::TypeScript => scan_code(&TYPESCRIPT, state, out),
        Language::Python => scan_code(&PYTHON, state, out),
        Language::Go => scan_code(&GO, state, out),
        Language::C => scan_code(&C_FAMILY, state, out),
        Language::Ruby => scan_code(&RUBY, state, out),
        Language::Sql => scan_code(&SQL, state, out),
        Language::Shell => scan_shell(state, out),
        Language::Json => scan_json(state, out),
        Language::Toml => scan_toml(state, out),
        Language::Yaml => scan_yaml(state, out),
        Language::Html => scan_html(state, out),
        Language::Css => scan_css(state, out),
        Language::Markdown => scan_markdown(state, out),
        Language::Diff => {
            let token = match diff_line_kind(out.text) {
                LineKind::Hunk => Token::Comment,
                LineKind::Header => Token::Keyword,
                LineKind::Context | LineKind::Added | LineKind::Removed => Token::Plain,
            };
            out.emit(out.text.len(), token);
        }
        Language::Plain => {}
    }
}

/// Span builder for one line. Spans are always contiguous from 0: `emit` extends
/// coverage from the current position to `end`, merging with the previous span
/// when the token matches.
struct Out<'a> {
    text: &'a str,
    spans: Vec<(Range<usize>, Token)>,
    pos: usize,
}

impl<'a> Out<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            spans: Vec::new(),
            pos: 0,
        }
    }

    fn bytes(&self) -> &'a [u8] {
        self.text.as_bytes()
    }

    fn emit(&mut self, end: usize, token: Token) {
        let mut end = end.min(self.text.len());
        // Terminates because `text.len()` is always a boundary.
        while !self.text.is_char_boundary(end) {
            end += 1;
        }
        if end <= self.pos {
            return;
        }
        match self.spans.last_mut() {
            Some((range, last)) if *last == token => range.end = end,
            _ => self.spans.push((self.pos..end, token)),
        }
        self.pos = end;
    }

    /// Extent of the character starting at `at`, for single-character fallbacks.
    fn char_end(&self, at: usize) -> usize {
        self.text
            .get(at..)
            .and_then(|rest| rest.chars().next())
            .map_or(at + 1, |c| at + c.len_utf8())
    }

    fn word(&self, range: Range<usize>) -> &'a str {
        self.text.get(range).unwrap_or_default()
    }

    fn finish(mut self) -> Vec<(Range<usize>, Token)> {
        self.emit(self.text.len(), Token::Plain);
        self.spans
    }
}

/// Construct left open at the end of the previous line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Normal,
    Block {
        depth: u32,
    },
    Str {
        quote: u8,
    },
    Raw {
        hashes: usize,
    },
    Triple {
        quote: u8,
    },
    Template,
    HtmlComment,
    Tag,
    Fence {
        fence: u8,
        len: usize,
    },
    YamlBlock {
        indent: usize,
    },
}

#[derive(Debug, Default)]
struct State {
    mode: Mode,
    /// CSS brace depth, or TOML open-array depth.
    nest: usize,
    /// Brace depth inside each open `${` of nested JS template literals.
    templates: Vec<u32>,
    /// Inside a JSX tag, with the brace depth of any `{expr}` attribute value.
    jsx: Option<u32>,
    /// A shell line ended with `\`, so the next line continues the command.
    continued: bool,
}

fn scan_while(bytes: &[u8], mut at: usize, pred: impl Fn(u8) -> bool) -> usize {
    while at < bytes.len() && pred(bytes[at]) {
        at += 1;
    }
    at
}

fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

fn skip_blanks(bytes: &[u8], at: usize) -> usize {
    scan_while(bytes, at, |c| c == b' ' || c == b'\t')
}

/// End of a quoted string whose body starts at `at`, and whether it closed.
fn quoted_end(bytes: &[u8], mut at: usize, quote: u8, escapes: bool) -> (usize, bool) {
    while at < bytes.len() {
        let byte = bytes[at];
        if escapes && byte == b'\\' {
            at += 2;
            continue;
        }
        if byte == quote {
            return (at + 1, true);
        }
        at += 1;
    }
    (bytes.len(), false)
}

fn triple_end(bytes: &[u8], mut at: usize, quote: u8, escapes: bool) -> (usize, bool) {
    while at < bytes.len() {
        if escapes && bytes[at] == b'\\' {
            at += 2;
        } else if bytes[at..].starts_with(&[quote, quote, quote]) {
            return (at + 3, true);
        } else {
            at += 1;
        }
    }
    (bytes.len(), false)
}

fn raw_end(bytes: &[u8], mut at: usize, hashes: usize) -> (usize, bool) {
    while at < bytes.len() {
        if bytes[at] == b'"'
            && bytes
                .get(at + 1..at + 1 + hashes)
                .is_some_and(|tail| tail.iter().all(|&c| c == b'#'))
        {
            return (at + 1 + hashes, true);
        }
        at += 1;
    }
    (bytes.len(), false)
}

fn block_comment_end(bytes: &[u8], mut at: usize, mut depth: u32, nested: bool) -> (usize, u32) {
    while at < bytes.len() {
        let next = bytes.get(at + 1).copied();
        if bytes[at] == b'*' && next == Some(b'/') {
            at += 2;
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return (at, 0);
            }
        } else if nested && bytes[at] == b'/' && next == Some(b'*') {
            at += 2;
            depth += 1;
        } else {
            at += 1;
        }
    }
    (bytes.len(), depth)
}

/// Numeric literal starting with a digit: hex/octal/binary prefixes, `_`
/// separators, fractions, exponents, and alphanumeric suffixes (`u32`, `f64`, `n`).
fn number_end(bytes: &[u8], at: usize) -> usize {
    let digits = |at| scan_while(bytes, at, |c| c.is_ascii_digit() || c == b'_');
    let alnum = |at| scan_while(bytes, at, |c| c.is_ascii_alphanumeric() || c == b'_');
    if bytes.get(at) == Some(&b'0')
        && matches!(
            bytes.get(at + 1),
            Some(b'x' | b'X' | b'o' | b'O' | b'b' | b'B')
        )
    {
        return alnum(at + 2);
    }
    let mut end = digits(at);
    if bytes.get(end) == Some(&b'.') && bytes.get(end + 1).is_some_and(u8::is_ascii_digit) {
        end = digits(end + 1);
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(bytes.get(end + 1), Some(b'+' | b'-')));
        if bytes.get(end + 1 + sign).is_some_and(u8::is_ascii_digit) {
            end = digits(end + 1 + sign);
        }
    }
    alnum(end)
}

fn symbol_token(byte: u8) -> Token {
    match byte {
        b'(' | b')' | b'[' | b']' | b'{' | b'}' | b',' | b';' | b'.' | b':' => Token::Punctuation,
        b'+' | b'-' | b'*' | b'/' | b'%' | b'=' | b'<' | b'>' | b'!' | b'&' | b'|' | b'^'
        | b'~' | b'?' => Token::Operator,
        _ => Token::Plain,
    }
}

// ---------------------------------------------------------------------------
// Programming languages with C-like, Python-like, or SQL-like lexical rules.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Rust,
    Ts,
    Python,
    Go,
    C,
    Ruby,
    Sql,
}

struct Syntax {
    family: Family,
    keywords: &'static [&'static str],
    types: &'static [&'static str],
    constants: &'static [&'static str],
}

#[rustfmt::skip]
static RUST: Syntax = Syntax {
    family: Family::Rust,
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
        "pub", "ref", "return", "static", "struct", "super", "trait", "type", "union", "unsafe",
        "use", "where", "while", "yield", "macro_rules",
    ],
    types: &[
        "Self", "bool", "char", "str", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16",
        "i32", "i64", "i128", "isize", "f32", "f64",
    ],
    constants: &["true", "false", "self", "None", "Some", "Ok", "Err"],
};

#[rustfmt::skip]
static TYPESCRIPT: Syntax = Syntax {
    family: Family::Ts,
    keywords: &[
        "abstract", "as", "async", "await", "break", "case", "catch", "class", "const",
        "constructor", "continue", "debugger", "declare", "default", "delete", "do", "else", "enum",
        "export", "extends", "finally", "for", "from", "function", "get", "if", "implements",
        "import", "in", "instanceof", "interface", "is", "keyof", "let", "namespace", "new", "of",
        "private", "protected", "public", "readonly", "return", "satisfies", "set", "static",
        "super", "switch", "throw", "try", "type", "typeof", "var", "while", "with", "yield",
    ],
    types: &[
        "string", "number", "boolean", "any", "unknown", "never", "void", "object", "bigint",
        "symbol",
    ],
    constants: &[
        "true", "false", "null", "undefined", "this", "NaN", "Infinity",
    ],
};

#[rustfmt::skip]
static PYTHON: Syntax = Syntax {
    family: Family::Python,
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "case", "class", "continue", "def",
        "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in",
        "is", "lambda", "match", "nonlocal", "not", "or", "pass", "raise", "return", "try",
        "while", "with", "yield",
    ],
    types: &[
        "int", "float", "str", "bool", "list", "dict", "set", "tuple", "bytes", "object", "complex",
        "frozenset",
    ],
    constants: &["True", "False", "None", "self", "cls"],
};

#[rustfmt::skip]
static GO: Syntax = Syntax {
    family: Family::Go,
    keywords: &[
        "break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough",
        "for", "func", "go", "goto", "if", "import", "interface", "map", "package", "range",
        "return", "select", "struct", "switch", "type", "var",
    ],
    types: &[
        "any", "bool", "byte", "complex64", "complex128", "error", "float32", "float64", "int",
        "int8", "int16", "int32", "int64", "rune", "string", "uint", "uint8", "uint16", "uint32",
        "uint64", "uintptr",
    ],
    constants: &["true", "false", "nil", "iota"],
};

/// A union of C, C++, Java, Kotlin, Swift, and C# keywords: a few words that are
/// identifiers in one member are keywords in another, which is fine for display.
#[rustfmt::skip]
static C_FAMILY: Syntax = Syntax {
    family: Family::C,
    keywords: &[
        "abstract", "as", "async", "await", "break", "case", "catch", "class", "companion", "const",
        "constexpr", "continue", "data", "default", "defer", "delete", "do", "else", "enum",
        "explicit", "extends", "extension", "extern", "final", "finally", "for", "foreach",
        "friend", "fun", "func", "goto", "guard", "if", "implements", "import", "in", "init",
        "inline", "interface", "internal", "is", "lateinit", "let", "mutable", "namespace", "new",
        "noexcept", "object", "open", "operator", "out", "override", "package", "private",
        "protected", "protocol", "public", "readonly", "ref", "register", "return", "sealed",
        "sizeof", "static", "struct", "suspend", "switch", "synchronized", "template", "throw",
        "throws", "try", "typealias", "typedef", "typename", "union", "using", "val", "var",
        "virtual", "volatile", "when", "where", "while", "yield",
    ],
    types: &[
        "void", "int", "char", "short", "long", "float", "double", "bool", "boolean", "byte",
        "signed", "unsigned", "size_t", "string", "auto", "uint8_t", "uint16_t", "uint32_t",
        "uint64_t", "int8_t", "int16_t", "int32_t", "int64_t",
    ],
    constants: &[
        "true", "false", "null", "nullptr", "NULL", "nil", "this", "self", "super",
    ],
};

#[rustfmt::skip]
static RUBY: Syntax = Syntax {
    family: Family::Ruby,
    keywords: &[
        "alias", "and", "begin", "break", "case", "class", "def", "defined?", "do", "else", "elsif",
        "end", "ensure", "for", "if", "in", "module", "next", "not", "or", "redo", "rescue",
        "retry", "return", "super", "then", "undef", "unless", "until", "when", "while", "yield",
        "require", "require_relative", "include", "extend", "attr_reader", "attr_writer",
        "attr_accessor", "private", "protected", "public",
    ],
    types: &[],
    constants: &["true", "false", "nil", "self"],
};

/// Matched case-insensitively.
#[rustfmt::skip]
static SQL: Syntax = Syntax {
    family: Family::Sql,
    keywords: &[
        "add", "all", "alter", "and", "as", "asc", "begin", "between", "by", "cascade", "case",
        "check", "column", "commit", "constraint", "create", "cross", "database", "declare",
        "default", "delete", "desc", "distinct", "drop", "else", "end", "exists", "foreign", "from",
        "full", "function", "grant", "group", "having", "if", "in", "index", "inner", "insert",
        "into", "is", "join", "key", "language", "left", "like", "limit", "not", "offset", "on",
        "or", "order", "outer", "primary", "procedure", "references", "replace", "returning",
        "returns", "revoke", "right", "rollback", "schema", "select", "set", "table", "then",
        "transaction", "trigger", "union", "unique", "update", "using", "values", "view", "when",
        "where", "with",
    ],
    types: &[
        "int", "integer", "bigint", "smallint", "serial", "bigserial", "text", "varchar", "char",
        "boolean", "bool", "date", "time", "timestamp", "timestamptz", "interval", "uuid", "json",
        "jsonb", "float", "real", "double", "numeric", "decimal", "blob", "bytea",
    ],
    constants: &["true", "false", "null"],
};

impl Syntax {
    fn lookup(&self, word: &str) -> Option<Token> {
        let has = |list: &[&str]| {
            if self.family == Family::Sql {
                list.iter().any(|entry| entry.eq_ignore_ascii_case(word))
            } else {
                list.contains(&word)
            }
        };
        if has(self.constants) {
            Some(Token::Constant)
        } else if has(self.types) {
            Some(Token::Type)
        } else if has(self.keywords) {
            Some(Token::Keyword)
        } else {
            None
        }
    }

    /// What the identifier after a declaring keyword names.
    fn declares(&self, keyword: &str) -> Option<Token> {
        match (self.family, keyword) {
            (Family::Rust, "fn")
            | (Family::Ts, "function")
            | (Family::Python | Family::Ruby, "def")
            | (Family::Go, "func")
            | (Family::C, "fun" | "func") => Some(Token::Function),
            (Family::Rust, "struct" | "enum" | "trait" | "type" | "union")
            | (Family::Ts, "class" | "interface" | "type" | "enum")
            | (Family::Python, "class")
            | (Family::Go, "type")
            | (Family::C, "class" | "struct" | "enum" | "interface" | "union" | "protocol")
            | (Family::Ruby, "class" | "module") => Some(Token::Type),
            _ => None,
        }
    }

    fn line_comment(&self, rest: &[u8]) -> bool {
        match self.family {
            Family::Rust | Family::Ts | Family::Go | Family::C => rest.starts_with(b"//"),
            Family::Python | Family::Ruby => rest.starts_with(b"#"),
            Family::Sql => rest.starts_with(b"--"),
        }
    }

    fn block_comments(&self) -> bool {
        !matches!(self.family, Family::Python | Family::Ruby)
    }

    /// Whether a string opened with `quote` may continue onto the next line.
    fn multiline(&self, quote: u8) -> bool {
        match self.family {
            Family::Rust => quote == b'"',
            Family::Go => quote == b'`',
            Family::Ruby | Family::Sql => true,
            Family::Ts | Family::Python | Family::C => false,
        }
    }

    fn escapes(&self, quote: u8) -> bool {
        match self.family {
            Family::Sql => false,
            Family::Go => quote != b'`',
            _ => true,
        }
    }
}

fn scan_code(syntax: &Syntax, state: &mut State, out: &mut Out<'_>) {
    let bytes = out.bytes();
    let len = bytes.len();
    let family = syntax.family;
    let mut at = 0;
    // Last significant byte before the current token (0 at line start) and whether
    // it ended a keyword; used for `.field`, JSX starts, and declared names.
    let mut prev = 0u8;
    let mut prev_keyword = false;
    let mut declared: Option<Token> = None;
    let line_start = scan_while(bytes, 0, |c| c.is_ascii_whitespace());
    while at < len {
        match state.mode {
            Mode::Block { depth } => {
                let (end, depth) = block_comment_end(bytes, at, depth, family == Family::Rust);
                out.emit(end, Token::Comment);
                state.mode = if depth == 0 {
                    Mode::Normal
                } else {
                    Mode::Block { depth }
                };
                at = end;
                continue;
            }
            Mode::Str { quote } => {
                let (end, closed) = quoted_end(bytes, at, quote, syntax.escapes(quote));
                out.emit(end, Token::String);
                if closed {
                    state.mode = Mode::Normal;
                }
                at = end;
                prev = b'"';
                continue;
            }
            Mode::Raw { hashes } => {
                let (end, closed) = raw_end(bytes, at, hashes);
                out.emit(end, Token::String);
                if closed {
                    state.mode = Mode::Normal;
                }
                at = end;
                prev = b'"';
                continue;
            }
            Mode::Triple { quote } => {
                let (end, closed) = triple_end(bytes, at, quote, true);
                out.emit(end, Token::String);
                if closed {
                    state.mode = Mode::Normal;
                }
                at = end;
                prev = b'"';
                continue;
            }
            Mode::Template => {
                at = template_body(state, out, at);
                prev = b'"';
                continue;
            }
            _ => {}
        }

        let byte = bytes[at];
        if byte.is_ascii_whitespace() {
            at = scan_while(bytes, at, |c| c.is_ascii_whitespace());
            out.emit(at, Token::Plain);
            continue;
        }
        let pending = declared.take();
        if state.jsx == Some(0) {
            at = jsx_tag_token(state, out, at);
            prev = b'>';
            prev_keyword = false;
            continue;
        }
        if syntax.line_comment(&bytes[at..]) {
            out.emit(len, Token::Comment);
            break;
        }
        let next = bytes.get(at + 1).copied();
        let token_start = at;
        let mut was_keyword = false;
        match byte {
            b'/' if next == Some(b'*') && syntax.block_comments() => {
                out.emit(at + 2, Token::Comment);
                state.mode = Mode::Block { depth: 1 };
                at += 2;
            }
            b'#' if family == Family::Rust && matches!(next, Some(b'[' | b'!')) => {
                at = attribute_end(bytes, at);
                out.emit(at, Token::Attribute);
            }
            b'#' if family == Family::C && at == line_start => {
                at = preprocessor(out, at);
            }
            b'@' if matches!(family, Family::Python | Family::Ts | Family::C)
                && next.is_some_and(is_ident_start) =>
            {
                at = scan_while(bytes, at + 1, |c| is_ident(c) || c == b'.');
                out.emit(at, Token::Attribute);
            }
            b'@' | b'$' if family == Family::Ruby => {
                let sigils = scan_while(bytes, at, |c| c == b'@' || c == b'$');
                at = scan_while(bytes, sigils, is_ident);
                let token = if at > sigils {
                    Token::Variable
                } else {
                    Token::Plain
                };
                out.emit(at, token);
            }
            b':' if family == Family::Ruby
                && next.is_some_and(is_ident_start)
                && (at == 0 || bytes[at - 1] != b':') =>
            {
                at = scan_while(bytes, at + 1, is_ident);
                out.emit(at, Token::Constant);
            }
            b'@' | b':' | b'$' | b'?'
                if family == Family::Sql
                    && next.is_some_and(|c| is_ident(c) || c.is_ascii_digit())
                    && (at == 0 || bytes[at - 1] != b':')
                    && next != Some(b':') =>
            {
                at = scan_while(bytes, at + 1, is_ident);
                out.emit(at, Token::Variable);
            }
            b'"' | b'\'' | b'`' => at = open_string(syntax, state, out, at, byte),
            b'0'..=b'9' => {
                at = number_end(bytes, at);
                out.emit(at, Token::Number);
            }
            b'<' if family == Family::Ts && jsx_starts(bytes, at, prev, prev_keyword) => {
                at = jsx_open(state, out, at);
            }
            b'{' => {
                if let Some(depth) = state.jsx.as_mut() {
                    *depth += 1;
                } else if let Some(depth) = state.templates.last_mut() {
                    *depth += 1;
                }
                at += 1;
                out.emit(at, Token::Punctuation);
            }
            b'}' => {
                at += 1;
                out.emit(at, Token::Punctuation);
                if let Some(depth) = state.jsx.as_mut() {
                    *depth = depth.saturating_sub(1);
                } else if let Some(depth) = state.templates.last_mut() {
                    if *depth == 0 {
                        state.templates.pop();
                        state.mode = Mode::Template;
                    } else {
                        *depth -= 1;
                    }
                }
            }
            _ if is_ident_start(byte) || (family == Family::Ts && byte == b'$') => {
                let end = scan_while(bytes, at, |c| {
                    is_ident(c) || (family == Family::Ts && c == b'$')
                });
                let word = out.word(at..end);
                let prefixed = bytes
                    .get(end)
                    .copied()
                    .filter(|&quote| string_prefix(syntax, bytes, word, end, quote));
                if let Some(quote) = prefixed {
                    at = end;
                    if family == Family::Rust && word.ends_with('r') {
                        at = rust_raw_string(state, out, at);
                    } else {
                        out.emit(at, Token::String);
                        at = open_string(syntax, state, out, at, quote);
                    }
                } else {
                    let (token, end) = classify_ident(syntax, out, at, end, prev, pending);
                    was_keyword = token == Token::Keyword;
                    if was_keyword {
                        declared = syntax.declares(word);
                    }
                    out.emit(end, token);
                    at = end;
                }
            }
            _ => {
                at = out.char_end(at);
                out.emit(at, symbol_token(byte));
            }
        }
        prev = if at > token_start {
            bytes[at - 1]
        } else {
            prev
        };
        if is_ident(prev) {
            prev = b'a';
        }
        prev_keyword = was_keyword;
    }
}

/// Classifies an identifier spanning `start..end`, returning the token and its
/// extent (which grows to include a Rust macro's `!`).
fn classify_ident(
    syntax: &Syntax,
    out: &Out<'_>,
    start: usize,
    end: usize,
    prev: u8,
    declared: Option<Token>,
) -> (Token, usize) {
    let bytes = out.bytes();
    let word = out.word(start..end);
    let next = bytes.get(end).copied();
    let call = bytes.get(skip_blanks(bytes, end)) == Some(&b'(');
    if prev == b'.' && !(syntax.family == Family::Rust && word == "await") {
        return (
            if call {
                Token::Function
            } else {
                Token::Property
            },
            end,
        );
    }
    if let Some(token) = declared {
        return (token, end);
    }
    if let Some(token) = syntax.lookup(word) {
        return (token, end);
    }
    if syntax.family == Family::Rust && next == Some(b'!') && bytes.get(end + 1) != Some(&b'=') {
        return (Token::Function, end + 1);
    }
    if call {
        return (Token::Function, end);
    }
    let has_upper = word.bytes().any(|c| c.is_ascii_uppercase());
    let has_lower = word.bytes().any(|c| c.is_ascii_lowercase());
    if word.len() > 1 && has_upper && !has_lower {
        return (Token::Constant, end);
    }
    if word.bytes().next().is_some_and(|c| c.is_ascii_uppercase()) {
        return (Token::Type, end);
    }
    (Token::Plain, end)
}

/// Whether `word` is a string-literal prefix (`r`, `b`, `br`, Python's `f`/`rb`/...)
/// immediately followed by `next`, a quote (or `#` for Rust raw strings) at `end`.
fn string_prefix(syntax: &Syntax, bytes: &[u8], word: &str, end: usize, next: u8) -> bool {
    match syntax.family {
        Family::Rust => match word {
            "r" | "br" | "cr" => next == b'"' || (next == b'#' && raw_hashes_quote(bytes, end)),
            "b" | "c" => next == b'"' || (word == "b" && next == b'\''),
            _ => false,
        },
        Family::Python => {
            (next == b'"' || next == b'\'')
                && word.len() <= 2
                && word.bytes().all(|c| b"rRbBuUfF".contains(&c))
        }
        _ => false,
    }
}

fn raw_hashes_quote(bytes: &[u8], at: usize) -> bool {
    bytes.get(scan_while(bytes, at, |c| c == b'#')) == Some(&b'"')
}

/// Rust raw string whose `#`s or opening quote start at `at`.
fn rust_raw_string(state: &mut State, out: &mut Out<'_>, at: usize) -> usize {
    let bytes = out.bytes();
    let quote = scan_while(bytes, at, |c| c == b'#');
    let hashes = quote - at;
    let (end, closed) = raw_end(bytes, quote + 1, hashes);
    out.emit(end, Token::String);
    if !closed {
        state.mode = Mode::Raw { hashes };
    }
    end
}

/// Opens a string, char, template, or quoted identifier at the quote at `at`.
fn open_string(
    syntax: &Syntax,
    state: &mut State,
    out: &mut Out<'_>,
    at: usize,
    quote: u8,
) -> usize {
    let bytes = out.bytes();
    let family = syntax.family;
    match (family, quote) {
        (Family::Rust, b'\'') => return rust_quote(out, at),
        (Family::Ts, b'`') => {
            out.emit(at + 1, Token::String);
            state.mode = Mode::Template;
            return at + 1;
        }
        (Family::Rust | Family::Python | Family::C, b'`') => {
            out.emit(at + 1, Token::Punctuation);
            return at + 1;
        }
        _ => {}
    }
    let triple = matches!(family, Family::Python) || (family == Family::C && quote == b'"');
    if triple && bytes[at..].starts_with(&[quote, quote, quote]) {
        out.emit(at + 3, Token::String);
        state.mode = Mode::Triple { quote };
        return at + 3;
    }
    let (end, closed) = quoted_end(bytes, at + 1, quote, syntax.escapes(quote));
    // SQL double quotes and backticks delimit identifiers, not strings.
    let token = if family == Family::Sql && quote != b'\'' {
        Token::Property
    } else {
        Token::String
    };
    out.emit(end, token);
    if !closed && syntax.multiline(quote) {
        state.mode = Mode::Str { quote };
    }
    end
}

/// Rust `'`: a char literal (`'a'`, `'\n'`, `'é'`) or a lifetime/label (`'a`).
fn rust_quote(out: &mut Out<'_>, at: usize) -> usize {
    let bytes = out.bytes();
    let next = bytes.get(at + 1).copied();
    if next == Some(b'\\') {
        let (end, _) = quoted_end(bytes, at + 1, b'\'', true);
        out.emit(end, Token::String);
        return end;
    }
    if next.is_some() {
        let after = out.char_end(at + 1);
        if bytes.get(after) == Some(&b'\'') {
            out.emit(after + 1, Token::String);
            return after + 1;
        }
    }
    if next.is_some_and(is_ident_start) {
        let end = scan_while(bytes, at + 1, is_ident);
        out.emit(end, Token::Type);
        return end;
    }
    out.emit(at + 1, Token::Punctuation);
    at + 1
}

/// `#[...]` or `#![...]`, including nested brackets and quoted `]`.
fn attribute_end(bytes: &[u8], at: usize) -> usize {
    let mut depth = 0u32;
    let mut index = at;
    while index < bytes.len() {
        match bytes[index] {
            b'[' => depth += 1,
            b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return index + 1;
                }
            }
            b'"' => {
                index = quoted_end(bytes, index + 1, b'"', true).0;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    bytes.len()
}

/// A C preprocessor directive (`#include <x.h>`, `#define`), from `#` at `at`.
fn preprocessor(out: &mut Out<'_>, at: usize) -> usize {
    let bytes = out.bytes();
    let word_start = skip_blanks(bytes, at + 1);
    let end = scan_while(bytes, word_start, is_ident);
    out.emit(end, Token::Attribute);
    let word = out.word(word_start..end);
    if matches!(word, "include" | "import") {
        let open = skip_blanks(bytes, end);
        if bytes.get(open) == Some(&b'<') {
            let close = quoted_end(bytes, open + 1, b'>', false).0;
            out.emit(open, Token::Plain);
            out.emit(close, Token::String);
            return close;
        }
    }
    end
}

/// Body of a JS template literal from `at`, stopping after the closing backtick or
/// at a `${` interpolation (which the code scanner then lexes as code).
fn template_body(state: &mut State, out: &mut Out<'_>, at: usize) -> usize {
    let bytes = out.bytes();
    let mut index = at;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'`' => {
                out.emit(index + 1, Token::String);
                state.mode = Mode::Normal;
                return index + 1;
            }
            b'$' if bytes.get(index + 1) == Some(&b'{') => {
                out.emit(index, Token::String);
                out.emit(index + 2, Token::Punctuation);
                state.templates.push(0);
                state.mode = Mode::Normal;
                return index + 2;
            }
            _ => index += 1,
        }
    }
    out.emit(bytes.len(), Token::String);
    bytes.len()
}

/// Whether `<` at `at` opens a JSX element rather than a comparison or generic.
/// Closing tags are unambiguous; opening tags need an expression position.
fn jsx_starts(bytes: &[u8], at: usize, prev: u8, prev_keyword: bool) -> bool {
    let next = bytes.get(at + 1).copied();
    if next == Some(b'/') {
        return bytes.get(at + 2).is_some_and(u8::is_ascii_alphabetic)
            || bytes.get(at + 2) == Some(&b'>');
    }
    let expression_position = prev_keyword
        || matches!(
            prev,
            0 | b'('
                | b','
                | b'='
                | b'{'
                | b'}'
                | b'['
                | b'?'
                | b':'
                | b'&'
                | b'|'
                | b'!'
                | b';'
                | b'>'
        );
    expression_position && (next.is_some_and(|c| c.is_ascii_alphabetic()) || next == Some(b'>'))
}

fn jsx_open(state: &mut State, out: &mut Out<'_>, at: usize) -> usize {
    let bytes = out.bytes();
    let mut index = at + 1;
    if bytes.get(index) == Some(&b'/') {
        index += 1;
    }
    out.emit(index, Token::Punctuation);
    let name_end = scan_while(bytes, index, |c| {
        c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b':' | b'_')
    });
    out.emit(name_end, Token::Tag);
    if name_end == index {
        // Fragment `<>` or `</>`.
        out.emit(name_end + 1, Token::Punctuation);
        return name_end + 1;
    }
    state.jsx = Some(0);
    name_end
}

/// One token inside a JSX tag, between its name and `>` or `/>`.
fn jsx_tag_token(state: &mut State, out: &mut Out<'_>, at: usize) -> usize {
    let bytes = out.bytes();
    let byte = bytes[at];
    let next = bytes.get(at + 1).copied();
    let (end, token) = match byte {
        b'/' if next == Some(b'>') => {
            state.jsx = None;
            (at + 2, Token::Punctuation)
        }
        b'>' => {
            state.jsx = None;
            (at + 1, Token::Punctuation)
        }
        b'{' => {
            state.jsx = Some(1);
            (at + 1, Token::Punctuation)
        }
        b'"' | b'\'' => (quoted_end(bytes, at + 1, byte, false).0, Token::String),
        b'=' => (at + 1, Token::Operator),
        _ if is_ident_start(byte) => (
            scan_while(bytes, at, |c| {
                is_ident(c) || matches!(c, b'-' | b':' | b'.')
            }),
            Token::Attribute,
        ),
        _ => (out.char_end(at), Token::Punctuation),
    };
    out.emit(end, token);
    end
}

// ---------------------------------------------------------------------------
// Shell

fn shell_var_end(bytes: &[u8], at: usize) -> Option<usize> {
    match *bytes.get(at + 1)? {
        b'{' => Some(
            bytes[at + 2..]
                .iter()
                .position(|&c| c == b'}')
                .map_or(bytes.len(), |offset| at + 3 + offset),
        ),
        c if c.is_ascii_alphabetic() || c == b'_' => Some(scan_while(bytes, at + 1, |c| {
            c.is_ascii_alphanumeric() || c == b'_'
        })),
        c if c.is_ascii_digit() || matches!(c, b'@' | b'#' | b'?' | b'$' | b'!' | b'*' | b'-') => {
            Some(at + 2)
        }
        _ => None,
    }
}

/// Double-quoted shell string body from `at`, highlighting `$VAR` inside it.
fn shell_double_quoted(state: &mut State, out: &mut Out<'_>, at: usize) -> usize {
    let bytes = out.bytes();
    let mut index = at;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => {
                out.emit(index + 1, Token::String);
                state.mode = Mode::Normal;
                return index + 1;
            }
            b'$' => match shell_var_end(bytes, index) {
                Some(end) => {
                    out.emit(index, Token::String);
                    out.emit(end, Token::Variable);
                    index = end;
                }
                None => index += 1,
            },
            _ => index += 1,
        }
    }
    out.emit(bytes.len(), Token::String);
    state.mode = Mode::Str { quote: b'"' };
    bytes.len()
}

fn is_shell_meta(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(
            byte,
            b'|' | b'&' | b';' | b'<' | b'>' | b'(' | b')' | b'`' | b'"' | b'\'' | b'$' | b'\\'
        )
}

/// What the next bare word on a shell line means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellWord {
    Command,
    Argument,
    /// After `for`/`case`/`select`, where `in` is a keyword.
    AwaitingIn,
    /// After `export`/`local`/..., where words name variables.
    Declaration,
}

impl ShellWord {
    /// A quoted string or expansion fills the command slot if it was open.
    fn after_argument(self) -> Self {
        if self == Self::Command {
            Self::Argument
        } else {
            self
        }
    }
}

fn scan_shell(state: &mut State, out: &mut Out<'_>) {
    let bytes = out.bytes();
    let len = bytes.len();
    let mut at = 0;
    let mut expect = if state.continued {
        ShellWord::Argument
    } else {
        ShellWord::Command
    };
    state.continued = false;
    match state.mode {
        Mode::Str { quote: b'"' } => {
            at = shell_double_quoted(state, out, 0);
            expect = ShellWord::Argument;
        }
        Mode::Str { quote } => {
            let (end, closed) = quoted_end(bytes, 0, quote, false);
            out.emit(end, Token::String);
            if closed {
                state.mode = Mode::Normal;
            }
            at = end;
            expect = ShellWord::Argument;
        }
        _ => {
            state.mode = Mode::Normal;
            // Console transcripts prefix commands with a `$ ` prompt.
            if expect == ShellWord::Command && bytes.starts_with(b"$ ") {
                out.emit(1, Token::Punctuation);
                at = 1;
            }
        }
    }
    while at < len {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        match byte {
            _ if byte.is_ascii_whitespace() => {
                at = scan_while(bytes, at, |c| c.is_ascii_whitespace());
                out.emit(at, Token::Plain);
            }
            b'#' => {
                out.emit(len, Token::Comment);
                break;
            }
            b'\'' => {
                let (end, closed) = quoted_end(bytes, at + 1, b'\'', false);
                out.emit(end, Token::String);
                if !closed {
                    state.mode = Mode::Str { quote: b'\'' };
                }
                at = end;
                expect = expect.after_argument();
            }
            b'"' => {
                out.emit(at + 1, Token::String);
                at = shell_double_quoted(state, out, at + 1);
                expect = expect.after_argument();
            }
            b'$' => {
                if next == Some(b'(') {
                    at = scan_while(bytes, at + 1, |c| c == b'(');
                    out.emit(at, Token::Punctuation);
                    expect = ShellWord::Command;
                } else if next == Some(b'\'') {
                    at = quoted_end(bytes, at + 2, b'\'', true).0;
                    out.emit(at, Token::String);
                    expect = expect.after_argument();
                } else if let Some(end) = shell_var_end(bytes, at) {
                    at = end;
                    out.emit(at, Token::Variable);
                    expect = expect.after_argument();
                } else {
                    at += 1;
                    out.emit(at, Token::Plain);
                }
            }
            b'`' | b'(' => {
                at += 1;
                out.emit(at, Token::Punctuation);
                expect = ShellWord::Command;
            }
            b')' => {
                at += 1;
                out.emit(at, Token::Punctuation);
            }
            b'&' if next == Some(b'>') => {
                at = scan_while(bytes, at, |c| matches!(c, b'&' | b'>'));
                out.emit(at, Token::Operator);
            }
            b'|' | b'&' | b';' => {
                at = scan_while(bytes, at, |c| matches!(c, b'|' | b'&' | b';'));
                out.emit(at, Token::Operator);
                expect = ShellWord::Command;
            }
            b'<' | b'>' => {
                at = scan_while(bytes, at, |c| matches!(c, b'<' | b'>' | b'&' | b'|'));
                out.emit(at, Token::Operator);
            }
            b'\\' => {
                if next.is_none() {
                    state.continued = true;
                    at += 1;
                    out.emit(at, Token::Operator);
                } else {
                    at = out.char_end(at + 1);
                    out.emit(at, Token::Plain);
                }
            }
            _ => {
                let end = scan_while(bytes, at, |c| !is_shell_meta(c));
                let word = out.word(at..end);
                let flag = word.len() > 1 && word.starts_with('-');
                let token = match expect {
                    ShellWord::Command => shell_command_word(out, at, word, &mut expect),
                    ShellWord::AwaitingIn if word == "in" => {
                        expect = ShellWord::Argument;
                        Token::Keyword
                    }
                    ShellWord::Declaration if shell_assignment(out, at, word) => Token::String,
                    ShellWord::Declaration if !flag => Token::Variable,
                    _ if matches!(word, "]" | "]]") => Token::Keyword,
                    _ if flag => Token::Attribute,
                    _ if word.bytes().all(|c| c.is_ascii_digit()) => Token::Number,
                    _ => Token::Plain,
                };
                out.emit(end, token);
                at = end;
            }
        }
    }
}

/// Emits the `NAME=` of a `NAME=value` word and returns whether it was one.
fn shell_assignment(out: &mut Out<'_>, at: usize, word: &str) -> bool {
    let Some(eq) = word.find('=') else {
        return false;
    };
    let name = &word.as_bytes()[..eq];
    if name.is_empty() || !name.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'_') {
        return false;
    }
    out.emit(at + eq, Token::Variable);
    out.emit(at + eq + 1, Token::Operator);
    true
}

fn shell_command_word(out: &mut Out<'_>, at: usize, word: &str, expect: &mut ShellWord) -> Token {
    const KEYWORDS: &[&str] = &[
        "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
        "in", "function", "select", "time", "!", "return", "export", "local", "readonly",
        "declare", "typeset", "unset", "{", "}", "[[", "]]", "[", "]",
    ];
    // `NAME=value cmd`: the command word is still ahead.
    if shell_assignment(out, at, word) {
        return Token::String;
    }
    if !KEYWORDS.contains(&word) {
        *expect = ShellWord::Argument;
        return Token::Function;
    }
    *expect = match word {
        "for" | "case" | "select" => ShellWord::AwaitingIn,
        "export" | "local" | "readonly" | "declare" | "typeset" | "unset" => ShellWord::Declaration,
        "function" | "[[" | "[" | "return" => ShellWord::Argument,
        _ => ShellWord::Command,
    };
    Token::Keyword
}

// ---------------------------------------------------------------------------
// Data formats

fn scan_json(state: &mut State, out: &mut Out<'_>) {
    let bytes = out.bytes();
    let len = bytes.len();
    let mut at = 0;
    while at < len {
        if let Mode::Block { depth } = state.mode {
            let (end, depth) = block_comment_end(bytes, at, depth, false);
            out.emit(end, Token::Comment);
            state.mode = if depth == 0 {
                Mode::Normal
            } else {
                Mode::Block { depth }
            };
            at = end;
            continue;
        }
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        let followed_by_colon = |end: usize| bytes.get(skip_blanks(bytes, end)) == Some(&b':');
        let (end, token) = match byte {
            _ if byte.is_ascii_whitespace() => (
                scan_while(bytes, at, |c| c.is_ascii_whitespace()),
                Token::Plain,
            ),
            b'/' if next == Some(b'/') => (len, Token::Comment),
            b'/' if next == Some(b'*') => {
                state.mode = Mode::Block { depth: 1 };
                (at + 2, Token::Comment)
            }
            b'"' | b'\'' => {
                let end = quoted_end(bytes, at + 1, byte, true).0;
                let token = if followed_by_colon(end) {
                    Token::Property
                } else {
                    Token::String
                };
                (end, token)
            }
            b'-' | b'+' | b'.' if next.is_some_and(|c| c.is_ascii_digit()) => {
                (number_end(bytes, at + 1), Token::Number)
            }
            b'0'..=b'9' => (number_end(bytes, at), Token::Number),
            _ if is_ident_start(byte) || byte == b'$' => {
                let end = scan_while(bytes, at, |c| is_ident(c) || c == b'$');
                let token = match out.word(at..end) {
                    _ if followed_by_colon(end) => Token::Property,
                    "true" | "false" | "null" => Token::Constant,
                    "NaN" | "Infinity" => Token::Number,
                    _ => Token::Plain,
                };
                (end, token)
            }
            _ => (out.char_end(at), symbol_token(byte)),
        };
        out.emit(end, token);
        at = end;
    }
}

fn scan_toml(state: &mut State, out: &mut Out<'_>) {
    let bytes = out.bytes();
    let len = bytes.len();
    let mut at = 0;
    if let Mode::Triple { quote } = state.mode {
        let (end, closed) = triple_end(bytes, 0, quote, quote == b'"');
        out.emit(end, Token::String);
        if !closed {
            return;
        }
        state.mode = Mode::Normal;
        at = end;
    }
    // Openers still unclosed on this line. Inline tables are single-line in TOML,
    // so only arrays (`state.nest`) carry across lines.
    let mut open: Vec<u8> = vec![b'['; state.nest];
    let mut expect_key = open.is_empty() && at == 0;
    let first = skip_blanks(bytes, at);
    if expect_key && bytes.get(first) == Some(&b'[') {
        let name = scan_while(bytes, first, |c| c == b'[');
        out.emit(name, Token::Punctuation);
        let close = bytes[name..]
            .iter()
            .position(|&c| c == b']')
            .map_or(len, |offset| name + offset);
        out.emit(close, Token::Type);
        at = scan_while(bytes, close, |c| c == b']');
        out.emit(at, Token::Punctuation);
        expect_key = false;
    }
    while at < len {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        let (end, token) = match byte {
            _ if byte.is_ascii_whitespace() => (
                scan_while(bytes, at, |c| c.is_ascii_whitespace()),
                Token::Plain,
            ),
            b'#' => (len, Token::Comment),
            b'"' | b'\'' if bytes[at..].starts_with(&[byte, byte, byte]) => {
                let (end, closed) = triple_end(bytes, at + 3, byte, byte == b'"');
                if !closed {
                    state.mode = Mode::Triple { quote: byte };
                }
                (end, Token::String)
            }
            b'"' | b'\'' => {
                let end = quoted_end(bytes, at + 1, byte, byte == b'"').0;
                (
                    end,
                    if expect_key {
                        Token::Property
                    } else {
                        Token::String
                    },
                )
            }
            b'=' => {
                expect_key = false;
                (at + 1, Token::Operator)
            }
            b'[' | b'{' => {
                open.push(byte);
                expect_key = byte == b'{';
                (at + 1, Token::Punctuation)
            }
            b']' | b'}' => {
                open.pop();
                (at + 1, Token::Punctuation)
            }
            b',' => {
                expect_key = open.last() == Some(&b'{');
                (at + 1, Token::Punctuation)
            }
            _ if expect_key && (is_ident(byte) || byte == b'-') => (
                scan_while(bytes, at, |c| is_ident(c) || c == b'-'),
                Token::Property,
            ),
            b'0'..=b'9' => (
                scan_while(bytes, at, |c| {
                    c.is_ascii_alphanumeric() || matches!(c, b'_' | b':' | b'.' | b'-' | b'+')
                }),
                Token::Number,
            ),
            b'+' | b'-' if next.is_some_and(|c| c.is_ascii_alphanumeric()) => (
                scan_while(bytes, at + 1, |c| {
                    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.')
                }),
                Token::Number,
            ),
            _ if is_ident_start(byte) => {
                let end = scan_while(bytes, at, is_ident);
                let token = match out.word(at..end) {
                    "true" | "false" => Token::Constant,
                    "inf" | "nan" => Token::Number,
                    _ => Token::Plain,
                };
                (end, token)
            }
            _ => (out.char_end(at), symbol_token(byte)),
        };
        out.emit(end, token);
        at = end;
    }
    state.nest = open.iter().filter(|&&c| c == b'[').count();
}

fn scan_yaml(state: &mut State, out: &mut Out<'_>) {
    let bytes = out.bytes();
    let len = bytes.len();
    let indent = scan_while(bytes, 0, |c| c == b' ');
    if let Mode::YamlBlock { indent: parent } = state.mode {
        if indent == len {
            return;
        }
        if indent > parent {
            out.emit(len, Token::String);
            return;
        }
        state.mode = Mode::Normal;
    }
    let mut at = indent;
    let rest = &bytes[at..];
    if (rest.starts_with(b"---") || rest.starts_with(b"..."))
        && rest.get(3).is_none_or(u8::is_ascii_whitespace)
    {
        at += 3;
        out.emit(at, Token::Punctuation);
    } else if rest.starts_with(b"%") {
        out.emit(len, Token::Keyword);
        return;
    }
    let mut flow = 0usize;
    let mut value = false;
    // Column of the key (or `-` item) owning a block scalar; its content is the
    // following lines indented deeper than this.
    let mut owner = indent;
    while at < len {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        let flow_delimiter = |c: u8| flow > 0 && matches!(c, b',' | b']' | b'}');
        let separator = |at: usize| {
            bytes.get(at) == Some(&b':')
                && bytes
                    .get(at + 1)
                    .is_none_or(|&c| c.is_ascii_whitespace() || flow_delimiter(c))
        };
        let (end, token) = match byte {
            _ if byte.is_ascii_whitespace() => (
                scan_while(bytes, at, |c| c.is_ascii_whitespace()),
                Token::Plain,
            ),
            b'#' if at == 0 || bytes[at - 1].is_ascii_whitespace() => (len, Token::Comment),
            b'-' if !value && next.is_none_or(|c| c.is_ascii_whitespace()) => {
                owner = at;
                (at + 1, Token::Punctuation)
            }
            b'[' | b'{' => {
                flow += 1;
                value = byte == b'[';
                (at + 1, Token::Punctuation)
            }
            b']' | b'}' => {
                flow = flow.saturating_sub(1);
                (at + 1, Token::Punctuation)
            }
            b',' if flow > 0 => {
                value = false;
                (at + 1, Token::Punctuation)
            }
            _ if separator(at) => {
                value = true;
                (at + 1, Token::Punctuation)
            }
            b'&' | b'*' if next.is_some_and(|c| !c.is_ascii_whitespace()) => (
                scan_while(bytes, at + 1, |c| {
                    !(c.is_ascii_whitespace() || flow_delimiter(c))
                }),
                Token::Variable,
            ),
            b'!' => (
                scan_while(bytes, at + 1, |c| !c.is_ascii_whitespace()),
                Token::Attribute,
            ),
            b'|' | b'>' if value => {
                state.mode = Mode::YamlBlock { indent: owner };
                (
                    scan_while(bytes, at + 1, |c| {
                        matches!(c, b'-' | b'+') || c.is_ascii_digit()
                    }),
                    Token::Operator,
                )
            }
            b'"' | b'\'' => {
                // Single-quoted YAML escapes `'` by doubling it, which `quoted_end`
                // handles as two adjacent strings.
                let end = quoted_end(bytes, at + 1, byte, byte == b'"').0;
                let token = if separator(skip_blanks(bytes, end)) {
                    Token::Property
                } else {
                    Token::String
                };
                value = true;
                (end, token)
            }
            _ => {
                // Plain scalar: up to a `: ` separator, a ` #` comment, or a flow
                // delimiter, with trailing blanks left plain.
                let comment = |end: usize| {
                    end > at && bytes[end] == b'#' && bytes[end - 1].is_ascii_whitespace()
                };
                let mut end = at;
                while end < len && !(separator(end) || comment(end) || flow_delimiter(bytes[end])) {
                    end += 1;
                }
                while end > at && bytes[end - 1].is_ascii_whitespace() {
                    end -= 1;
                }
                let end = end.max(out.char_end(at));
                let token = if separator(end) {
                    Token::Property
                } else {
                    yaml_scalar(out.word(at..end))
                };
                value = true;
                (end, token)
            }
        };
        if token == Token::Property {
            owner = at;
        }
        out.emit(end, token);
        at = end;
    }
}

fn yaml_scalar(word: &str) -> Token {
    const CONSTANTS: &[&str] = &["true", "false", "yes", "no", "on", "off", "null", "~"];
    if CONSTANTS.iter().any(|c| c.eq_ignore_ascii_case(word)) {
        return Token::Constant;
    }
    let digits = word.strip_prefix(['-', '+']).unwrap_or(word);
    let numeric = digits.bytes().next().is_some_and(|c| c.is_ascii_digit())
        && digits.bytes().all(|c| {
            c.is_ascii_hexdigit()
                || matches!(c, b'.' | b'_' | b'x' | b'o' | b'e' | b'E' | b'+' | b'-')
        });
    if numeric || matches!(digits, ".inf" | ".nan" | ".Inf" | ".NaN") {
        Token::Number
    } else {
        Token::String
    }
}

// ---------------------------------------------------------------------------
// Markup and styles

fn scan_html(state: &mut State, out: &mut Out<'_>) {
    let bytes = out.bytes();
    let len = bytes.len();
    let mut at = 0;
    while at < len {
        let byte = bytes[at];
        let (end, token) = match state.mode {
            Mode::HtmlComment => match find(bytes, at, b"-->") {
                Some(close) => {
                    state.mode = Mode::Normal;
                    (close + 3, Token::Comment)
                }
                None => (len, Token::Comment),
            },
            Mode::Tag => match byte {
                _ if byte.is_ascii_whitespace() => (
                    scan_while(bytes, at, |c| c.is_ascii_whitespace()),
                    Token::Plain,
                ),
                b'>' => {
                    state.mode = Mode::Normal;
                    (at + 1, Token::Punctuation)
                }
                b'/' | b'?' if bytes.get(at + 1) == Some(&b'>') => {
                    state.mode = Mode::Normal;
                    (at + 2, Token::Punctuation)
                }
                b'=' => (at + 1, Token::Operator),
                b'"' | b'\'' => (quoted_end(bytes, at + 1, byte, false).0, Token::String),
                _ => {
                    let end = scan_while(bytes, at, |c| {
                        !c.is_ascii_whitespace() && !matches!(c, b'=' | b'>' | b'/' | b'"' | b'\'')
                    });
                    if end == at {
                        (out.char_end(at), Token::Punctuation)
                    } else if bytes.get(at.wrapping_sub(1)) == Some(&b'=') {
                        (end, Token::String)
                    } else {
                        (end, Token::Attribute)
                    }
                }
            },
            _ => {
                let next = bytes.get(at + 1).copied();
                if bytes[at..].starts_with(b"<!--") {
                    state.mode = Mode::HtmlComment;
                    (at + 4, Token::Comment)
                } else if byte == b'<'
                    && next
                        .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, b'/' | b'!' | b'?'))
                {
                    // `<name`, or `</name`, `<!DOCTYPE`, `<?xml`.
                    let name = if next.is_some_and(|c| c.is_ascii_alphabetic()) {
                        at + 1
                    } else {
                        at + 2
                    };
                    out.emit(name, Token::Punctuation);
                    state.mode = Mode::Tag;
                    (
                        scan_while(bytes, name, |c| {
                            c.is_ascii_alphanumeric() || matches!(c, b'-' | b':' | b'.' | b'_')
                        }),
                        Token::Tag,
                    )
                } else if byte == b'&' {
                    let end = scan_while(bytes, at + 1, |c| c.is_ascii_alphanumeric() || c == b'#');
                    if end > at + 1 && bytes.get(end) == Some(&b';') {
                        (end + 1, Token::Constant)
                    } else {
                        (at + 1, Token::Plain)
                    }
                } else {
                    (
                        scan_while(bytes, at + 1, |c| c != b'<' && c != b'&'),
                        Token::Plain,
                    )
                }
            }
        };
        out.emit(end, token);
        at = end.max(at + 1);
    }
}

fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

fn scan_css(state: &mut State, out: &mut Out<'_>) {
    let bytes = out.bytes();
    let len = bytes.len();
    let mut at = 0;
    let mut in_value = false;
    let mut expect_colon = false;
    let mut next_delimiter = None;
    // Parentheses hold declarations too, as in `@media (max-width: 600px)`.
    let mut parens = 0usize;
    while at < len {
        if let Mode::Block { depth } = state.mode {
            let (end, depth) = block_comment_end(bytes, at, depth, false);
            out.emit(end, Token::Comment);
            state.mode = if depth == 0 {
                Mode::Normal
            } else {
                Mode::Block { depth }
            };
            at = end;
            continue;
        }
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        let name = |from: usize| scan_while(bytes, from, |c| is_ident(c) || c == b'-');
        let (end, token) = match byte {
            _ if byte.is_ascii_whitespace() => (
                scan_while(bytes, at, |c| c.is_ascii_whitespace()),
                Token::Plain,
            ),
            b'/' if next == Some(b'*') => {
                state.mode = Mode::Block { depth: 1 };
                (at + 2, Token::Comment)
            }
            b'/' if next == Some(b'/') && !in_value => (len, Token::Comment),
            b'"' | b'\'' => (quoted_end(bytes, at + 1, byte, true).0, Token::String),
            b'{' => {
                state.nest += 1;
                in_value = false;
                (at + 1, Token::Punctuation)
            }
            b'}' => {
                state.nest = state.nest.saturating_sub(1);
                in_value = false;
                (at + 1, Token::Punctuation)
            }
            b';' => {
                in_value = false;
                (at + 1, Token::Punctuation)
            }
            b'(' => {
                parens += 1;
                (at + 1, Token::Punctuation)
            }
            b')' => {
                parens = parens.saturating_sub(1);
                in_value &= parens > 0;
                (at + 1, Token::Punctuation)
            }
            b':' if expect_colon => {
                expect_colon = false;
                in_value = true;
                (at + 1, Token::Punctuation)
            }
            b':' if !in_value => (name(scan_while(bytes, at, |c| c == b':')), Token::Attribute),
            b'@' => (name(at + 1), Token::Keyword),
            b'$' => (name(at + 1), Token::Variable),
            b'!' if next.is_some_and(|c| c.is_ascii_alphabetic()) => (name(at + 1), Token::Keyword),
            b'#' if in_value => (name(at + 1), Token::Number),
            b'#' => (name(at + 1), Token::Attribute),
            b'.' if in_value && next.is_some_and(|c| c.is_ascii_digit()) => {
                (css_number_end(bytes, at + 1), Token::Number)
            }
            b'.' if next.is_some_and(is_ident_start) => (name(at + 1), Token::Type),
            b'-' if in_value && next.is_some_and(|c| c.is_ascii_digit() || c == b'.') => {
                (css_number_end(bytes, at + 1), Token::Number)
            }
            b'0'..=b'9' => (css_number_end(bytes, at), Token::Number),
            _ if is_ident_start(byte) || byte == b'-' => {
                let end = name(at);
                let custom = out.word(at..end).starts_with("--");
                let call = bytes.get(end) == Some(&b'(');
                let token = if in_value {
                    if call {
                        Token::Function
                    } else if custom {
                        Token::Variable
                    } else {
                        Token::Plain
                    }
                } else if (parens > 0 && bytes.get(skip_blanks(bytes, end)) == Some(&b':'))
                    || (state.nest > 0 && css_declaration(bytes, end, &mut next_delimiter))
                {
                    expect_colon = true;
                    if custom {
                        Token::Variable
                    } else {
                        Token::Property
                    }
                } else if call {
                    Token::Function
                } else {
                    Token::Tag
                };
                (end.max(at + 1), token)
            }
            _ => (out.char_end(at), symbol_token(byte)),
        };
        out.emit(end, token);
        at = end;
    }
}

fn css_number_end(bytes: &[u8], at: usize) -> usize {
    let digits = scan_while(bytes, at, |c| c.is_ascii_digit() || c == b'.');
    scan_while(bytes, digits, |c| c.is_ascii_alphabetic() || c == b'%')
}

/// Whether a name ending at `at` starts a `property: value` declaration rather
/// than a nested selector such as `a:hover {`.
///
/// `next_delimiter` caches the first `{`, `;`, or `}` found by an earlier call on
/// this line. It stays valid while it lies beyond the current colon, because the
/// earlier search started before this colon; reusing it keeps a long minified
/// line linear.
fn css_declaration(bytes: &[u8], at: usize, next_delimiter: &mut Option<usize>) -> bool {
    let colon = skip_blanks(bytes, at);
    if bytes.get(colon) != Some(&b':') {
        return false;
    }
    let delimiter = match *next_delimiter {
        Some(found) if found > colon => found,
        _ => {
            let found = bytes[colon..]
                .iter()
                .position(|&c| matches!(c, b'{' | b';' | b'}'))
                .map_or(bytes.len(), |offset| colon + offset);
            *next_delimiter = Some(found);
            found
        }
    };
    bytes.get(delimiter) != Some(&b'{')
}

fn scan_markdown(state: &mut State, out: &mut Out<'_>) {
    let bytes = out.bytes();
    let len = bytes.len();
    let indent = scan_while(bytes, 0, |c| c == b' ');
    let body = &bytes[indent..];
    if let Mode::Fence {
        fence,
        len: fence_len,
    } = state.mode
    {
        let run = scan_while(body, 0, |c| c == fence);
        if indent <= 3 && run >= fence_len && body[run..].iter().all(u8::is_ascii_whitespace) {
            out.emit(len, Token::Punctuation);
            state.mode = Mode::Normal;
        } else {
            out.emit(len, Token::String);
        }
        return;
    }
    if indent > 3 {
        out.emit(len, Token::String);
        return;
    }
    if let Some(&fence @ (b'`' | b'~')) = body.first() {
        let run = scan_while(body, 0, |c| c == fence);
        if run >= 3 {
            out.emit(indent + run, Token::Punctuation);
            out.emit(len, Token::Attribute);
            state.mode = Mode::Fence { fence, len: run };
            return;
        }
    }
    let hashes = scan_while(body, 0, |c| c == b'#');
    if (1..=6).contains(&hashes) && body.get(hashes).is_none_or(|&c| c == b' ') {
        out.emit(len, Token::Keyword);
        return;
    }
    if body.first() == Some(&b'>') {
        out.emit(indent + 1, Token::Punctuation);
        out.emit(len, Token::Comment);
        return;
    }
    if body.len() >= 3
        && matches!(body[0], b'-' | b'*' | b'_')
        && body.iter().all(|&c| c == body[0] || c == b' ')
        && body.iter().filter(|&&c| c == body[0]).count() >= 3
    {
        out.emit(len, Token::Punctuation);
        return;
    }
    let mut at = indent;
    let marker = match body.first() {
        Some(b'-' | b'*' | b'+') => 1,
        Some(c) if c.is_ascii_digit() => {
            let digits = scan_while(body, 0, |c| c.is_ascii_digit());
            if matches!(body.get(digits), Some(b'.' | b')')) {
                digits + 1
            } else {
                0
            }
        }
        _ => 0,
    };
    if marker > 0 && body.get(marker).is_none_or(|&c| c == b' ') {
        at += marker;
        out.emit(at, Token::Punctuation);
        let task = skip_blanks(bytes, at);
        if matches!(bytes.get(task..task + 3), Some(b"[ ]" | b"[x]" | b"[X]")) {
            out.emit(task + 3, Token::Punctuation);
            at = task + 3;
        }
    }
    markdown_inline(out, at);
}

fn markdown_inline(out: &mut Out<'_>, mut at: usize) {
    let bytes = out.bytes();
    let len = bytes.len();
    // Delimiters already known to have no closer later on the line; remembering
    // them keeps a long line of unmatched `**`, `[`, or backticks linear.
    let mut unmatched_ticks: Vec<usize> = Vec::new();
    let mut unmatched_bold = [false; 2];
    let mut no_links = false;
    while at < len {
        let byte = bytes[at];
        match byte {
            b'`' => {
                let ticks = scan_while(bytes, at, |c| c == b'`');
                let run = &bytes[at..ticks];
                let close = if unmatched_ticks.contains(&run.len()) {
                    None
                } else {
                    find(bytes, ticks, run)
                };
                match close {
                    Some(close) => {
                        at = close + run.len();
                        out.emit(at, Token::String);
                    }
                    None => {
                        unmatched_ticks.push(run.len());
                        at = ticks;
                        out.emit(at, Token::Plain);
                    }
                }
            }
            b'*' | b'_' if bytes.get(at + 1) == Some(&byte) => {
                let slot = usize::from(byte == b'_');
                let close = if unmatched_bold[slot] {
                    None
                } else {
                    find(bytes, at + 2, &[byte, byte])
                };
                unmatched_bold[slot] = close.is_none();
                match close {
                    Some(close) if close > at + 2 => {
                        at = close + 2;
                        out.emit(at, Token::Type);
                    }
                    _ => {
                        at += 2;
                        out.emit(at, Token::Plain);
                    }
                }
            }
            b'[' | b'!' if !no_links => {
                let open = at + usize::from(byte == b'!');
                let link = if bytes.get(open) == Some(&b'[') {
                    markdown_link(bytes, open)
                } else {
                    Link::NotHere
                };
                if let Link::Found { text_end, close } = link {
                    out.emit(text_end + 1, Token::Function);
                    out.emit(close + 1, Token::String);
                    at = close + 1;
                } else {
                    no_links = link == Link::NoneLeft;
                    at += 1;
                    out.emit(at, Token::Plain);
                }
            }
            b'<' if bytes[at..].starts_with(b"<http") => {
                at = quoted_end(bytes, at + 1, b'>', false).0;
                out.emit(at, Token::String);
            }
            _ => {
                at = scan_while(bytes, at + 1, |c| {
                    !matches!(c, b'`' | b'*' | b'_' | b'[' | b'!' | b'<')
                });
                out.emit(at, Token::Plain);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    /// `[text](target)` with the positions of its `]` and `)`.
    Found {
        text_end: usize,
        close: usize,
    },
    NotHere,
    /// No complete link can start anywhere later on the line.
    NoneLeft,
}

fn markdown_link(bytes: &[u8], open: usize) -> Link {
    let Some(text_end) = find(bytes, open, b"]") else {
        return Link::NoneLeft;
    };
    if bytes.get(text_end + 1) != Some(&b'(') {
        return Link::NotHere;
    }
    match find(bytes, text_end + 2, b")") {
        Some(close) => Link::Found { text_end, close },
        None => Link::NoneLeft,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Checks the output contract and returns each span's text with its token.
    fn lex(language: Language, code: &str) -> Vec<Vec<(&str, Token)>> {
        let lines = highlight(language, code);
        let raw: Vec<&str> = code.split('\n').collect();
        assert_eq!(lines.len(), raw.len(), "one entry per line");
        lines
            .iter()
            .zip(&raw)
            .map(|(spans, raw)| {
                let line = raw.strip_suffix('\r').unwrap_or(raw);
                let mut pos = 0;
                for (index, (range, token)) in spans.iter().enumerate() {
                    assert_eq!(range.start, pos, "contiguous in {line:?}");
                    assert!(range.end > range.start, "non-empty in {line:?}");
                    assert!(line.is_char_boundary(range.end), "boundary in {line:?}");
                    if index > 0 {
                        assert_ne!(spans[index - 1].1, *token, "merged in {line:?}");
                    }
                    pos = range.end;
                }
                assert_eq!(pos, line.len(), "covers {line:?}");
                spans
                    .iter()
                    .map(|(range, token)| (&line[range.clone()], *token))
                    .collect()
            })
            .collect()
    }

    /// Token of the span containing the first occurrence of `needle`, which must
    /// fall entirely inside one span.
    fn token_of(language: Language, code: &str, needle: &str) -> Token {
        let lines = highlight(language, code);
        for (spans, raw) in lines.iter().zip(code.split('\n')) {
            if let Some(start) = raw.find(needle) {
                let end = start + needle.len();
                let (range, token) = spans
                    .iter()
                    .find(|(range, _)| range.start <= start && start < range.end)
                    .unwrap();
                assert!(
                    range.end >= end,
                    "{needle:?} split across spans in {raw:?}: {spans:?}"
                );
                return *token;
            }
        }
        panic!("{needle:?} not found");
    }

    const SAMPLES: &[(Language, &str)] = &[
        (
            Language::Rust,
            "#[derive(Debug)]\npub struct Foo<'a> { name: &'a str }\n/* outer /* nested */ still */\nfn main() {\n    let s = r#\"raw \"q\" é\"#;\n    let c = 'x'; let e = '\\n'; let u = 'é';\n    println!(\"{} {}\", 0x1F_u32, 1.5e-3);\n    x.field.call(); // 日本語 🎉\n    let multi = \"line one\nline two\";\n}",
        ),
        (
            Language::TypeScript,
            "import { a } from \"./a\";\nconst t = `hi ${name + `in ${x}`} there\nstill template`;\nfunction App(props: Props): JSX.Element {\n  return <div className=\"x\" onClick={() => go({a: 1})}>\n    <Foo.Bar bar={1} />\n  </div>;\n}\n/* comment\n spans */ let n = 10n; // 🎉",
        ),
        (
            Language::Python,
            "@dataclass\nclass Foo(Base):\n    def bar(self, x: int) -> str:\n        \"\"\"Doc\n        string é\"\"\"\n        return f\"{x}!\" + rb'\\x00' # 注释\n    None, True, 1_000.5j",
        ),
        (
            Language::Go,
            "package main\n\nfunc (s *Server) Run(ctx context.Context) error {\n\tq := `raw\nstring`\n\treturn nil // done\n}",
        ),
        (
            Language::Shell,
            "$ cargo build --release -p herdr\nexport PATH=\"$HOME/bin:${PATH}\" # comment\nif [ -f x ]; then echo 'hi é' | grep -v foo > out.txt; fi\nfor f in *.rs; do\n  echo \"multi\n line $f\"\ndone\nls \\\n  -la",
        ),
        (
            Language::Json,
            "{\n  \"name\": \"herdr\", \"n\": -1.5e3,\n  \"ok\": true, \"none\": null, // jsonc\n  \"list\": [1, \"é🎉\"] /* multi\n line */\n}",
        ),
        (
            Language::Toml,
            "[package]\nname = \"herdr\" # comment\nversion.workspace = true\n[[bin]]\narr = [\n  1, 2.5,\n  \"x\",\n]\ndesc = \"\"\"\nmulti é\n\"\"\"\ninline = { a = 1, b = [1, 2], c = 'lit' }\ndate = 1979-05-27T07:32:00Z",
        ),
        (
            Language::Yaml,
            "---\nname: CI\non: [push, pull_request]\njobs:\n  build: # comment\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: |\n          cargo test\n          echo é\n        env: { A: 1, \"B\": 'two' }\n    anchors: &base\n      x: *base\n      n: 3.5\n      t: !!str yes",
        ),
        (
            Language::Html,
            "<!DOCTYPE html>\n<div class=\"a\" id='b' hidden>\n  Text &amp; more é\n  <!-- multi\n  line --> <img\n    src=x.png\n    alt=\"🎉\" />\n</div>",
        ),
        (
            Language::Css,
            "@media (max-width: 600px) {\n  .card > a:hover, #main {\n    color: #fff !important;\n    margin: -0.5em 10px;\n    --gap: calc(1rem + 2px);\n    width: var(--gap);\n  }\n}\n/* multi\n line é */",
        ),
        (
            Language::Sql,
            "SELECT u.id, count(*) AS n\nFROM \"users\" u -- comment\nWHERE u.name = 'O''Brien' AND u.id = $1 AND x::int > :param\n/* multi\n line */ ORDER BY n DESC;",
        ),
        (
            Language::C,
            "#include <stdio.h>\n@Override\npublic static int main(String[] args) {\n    char c = 'x'; /* multi\n    line */ return 0x1F + NULL_PTR;\n    String s = \"\"\"\n    text block\n    \"\"\";\n}",
        ),
        (
            Language::Ruby,
            "class Foo < Bar\n  def baz(x)\n    @count += 1 # comment\n    puts :sym, $stdout, \"str #{x}\", nil\n  end\nend",
        ),
        (
            Language::Markdown,
            "# Title\n\nSome `code` and **bold** and [link](http://x.y).\n- item\n1. first\n> quote\n```rust\nlet x = 1;\n```\n---\n- [x] done é🎉",
        ),
        (
            Language::Diff,
            "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n context\n-old é\n+new 🎉\n\\ No newline at end of file",
        ),
        (Language::Plain, "just text\r\nwith crlf\r\n"),
    ];

    #[test]
    fn every_language_covers_lines_contiguously() {
        for (language, code) in SAMPLES {
            lex(*language, code);
            lex(*language, &code.replace('\n', "\r\n"));
        }
    }

    #[test]
    fn every_prefix_of_every_sample_is_safe() {
        for (language, code) in SAMPLES {
            for (index, _) in code.char_indices() {
                lex(*language, &code[..index]);
                lex(*language, &code[index..]);
            }
        }
    }

    #[test]
    fn samples_under_other_languages_are_safe() {
        let all: String = SAMPLES
            .iter()
            .map(|(_, code)| *code)
            .collect::<Vec<_>>()
            .join("\n");
        for (language, _) in SAMPLES {
            lex(*language, &all);
        }
    }

    #[test]
    fn multibyte_text_stays_on_char_boundaries() {
        let weird = "é'é\"é`é$é#é/*é*/é<é>é&é;é{é}é:é@é!é\\é'a'é\"🎉\"é日本'本'";
        for (language, _) in SAMPLES {
            lex(*language, weird);
            for (index, _) in weird.char_indices() {
                lex(*language, &weird[index..]);
            }
        }
    }

    #[test]
    fn empty_lines_and_crlf_are_excluded() {
        let lines = highlight(Language::Rust, "let a = 1;\r\n\r\n");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].last().unwrap().0.end, "let a = 1;".len());
        assert!(lines[1].is_empty());
        assert!(lines[2].is_empty());
    }

    #[test]
    fn oversized_input_is_plain() {
        let code = "fn a() {}\n".repeat(MAX_LINES);
        let lines = highlight(Language::Rust, &code);
        assert_eq!(lines.len(), MAX_LINES + 1);
        assert_eq!(lines[0], vec![(0.."fn a() {}".len(), Token::Plain)]);
        let long = format!("\"{}\"", "x".repeat(MAX_BYTES));
        assert_eq!(
            highlight(Language::Json, &long)[0],
            vec![(0..long.len(), Token::Plain)]
        );
    }

    #[test]
    fn rust_tokens() {
        let rust = Language::Rust;
        let code = "pub fn run<'a>(x: &'a str) -> Option<u32> { let c = 'x'; let e = '\\''; 'outer: loop {} }";
        assert_eq!(token_of(rust, code, "pub"), Token::Keyword);
        assert_eq!(token_of(rust, code, "run"), Token::Function);
        assert_eq!(token_of(rust, code, "'a"), Token::Type);
        assert_eq!(token_of(rust, code, "str"), Token::Type);
        assert_eq!(token_of(rust, code, "Option"), Token::Type);
        assert_eq!(token_of(rust, code, "'x'"), Token::String);
        assert_eq!(token_of(rust, code, "'\\''"), Token::String);
        assert_eq!(token_of(rust, code, "'outer"), Token::Type);
        assert_eq!(token_of(rust, "let u = 'é';", "'é'"), Token::String);

        assert_eq!(
            token_of(
                rust,
                "#[derive(Debug, \"]\")] struct A;",
                "#[derive(Debug, \"]\")]"
            ),
            Token::Attribute
        );
        assert_eq!(
            token_of(rust, "#![deny(warnings)]", "#![deny(warnings)]"),
            Token::Attribute
        );
        assert_eq!(
            token_of(rust, "println!(\"{}\", a != b);", "println!"),
            Token::Function
        );
        assert_eq!(token_of(rust, "a != b", "a"), Token::Plain);
        assert_eq!(token_of(rust, "x.len + y.go()", "len"), Token::Property);
        assert_eq!(token_of(rust, "x.len + y.go()", "go"), Token::Function);
        assert_eq!(token_of(rust, "f.await?", "await"), Token::Keyword);
        assert_eq!(token_of(rust, "MAX_LEN", "MAX_LEN"), Token::Constant);
        assert_eq!(token_of(rust, "self.x", "self"), Token::Constant);
        for number in ["0x1F_u32", "1_000", "1.5e-3", "2f64", "0b1010"] {
            assert_eq!(
                token_of(rust, &format!("a = {number};"), number),
                Token::Number,
                "{number}"
            );
        }
        assert_eq!(token_of(rust, "for i in 0..10 {}", "0"), Token::Number);
        assert_eq!(
            token_of(rust, "\"a \\\" b\" c", "\"a \\\" b\""),
            Token::String
        );
        assert_eq!(token_of(rust, "\"a \\\" b\" c", "c"), Token::Plain);
        assert_eq!(
            token_of(rust, "b\"bytes\" b'x'", "b\"bytes\""),
            Token::String
        );
        assert_eq!(token_of(rust, "b\"bytes\" b'x'", "b'x'"), Token::String);
    }

    #[test]
    fn rust_multiline_state() {
        let lines = lex(
            Language::Rust,
            "let s = r##\"one \"# still\ntwo\"## ; x\n/* a /* b */\nc */ y\n\"str\nnext\" z",
        );
        assert_eq!(
            lines[0][lines[0].len() - 1],
            ("r##\"one \"# still", Token::String)
        );
        assert_eq!(lines[1][0], ("two\"##", Token::String));
        assert_eq!(lines[2], vec![("/* a /* b */", Token::Comment)]);
        assert_eq!(lines[3][0], ("c */", Token::Comment));
        assert_eq!(lines[3][1], (" y", Token::Plain));
        assert_eq!(lines[4][0], ("\"str", Token::String));
        assert_eq!(lines[5][0], ("next\"", Token::String));
        assert_eq!(lines[5][1], (" z", Token::Plain));
        assert_eq!(
            token_of(Language::Rust, "let r = r\"a\\\"; x", "r\"a\\\""),
            Token::String
        );
    }

    #[test]
    fn typescript_tokens() {
        let ts = Language::TypeScript;
        let code = "export async function load(id: string): Promise<User | null> { return this.db.get(id); }";
        assert_eq!(token_of(ts, code, "export"), Token::Keyword);
        assert_eq!(token_of(ts, code, "load"), Token::Function);
        assert_eq!(token_of(ts, code, "string"), Token::Type);
        assert_eq!(token_of(ts, code, "Promise"), Token::Type);
        assert_eq!(token_of(ts, code, "User"), Token::Type);
        assert_eq!(token_of(ts, code, "null"), Token::Constant);
        assert_eq!(token_of(ts, code, "this"), Token::Constant);
        assert_eq!(token_of(ts, code, "db"), Token::Property);
        assert_eq!(token_of(ts, "class Foo extends Bar {}", "Foo"), Token::Type);
        assert_eq!(token_of(ts, "const $el = 'it\\'s';", "$el"), Token::Plain);
        assert_eq!(
            token_of(ts, "const $el = 'it\\'s';", "'it\\'s'"),
            Token::String
        );
        assert_eq!(
            token_of(ts, "@Component({})", "@Component"),
            Token::Attribute
        );
    }

    #[test]
    fn typescript_template_literals() {
        let lines = lex(
            Language::TypeScript,
            "const a = `x ${f({ b: `y` })} z\nw ${n}`; done",
        );
        assert_eq!(lines[0][4], ("`x ", Token::String));
        assert_eq!(lines[0][5], ("${", Token::Punctuation));
        assert!(lines[0].contains(&("f", Token::Function)));
        assert!(lines[0].contains(&("`y`", Token::String)));
        assert_eq!(lines[0].last().unwrap(), &(" z", Token::String));
        assert_eq!(lines[1][0], ("w ", Token::String));
        assert_eq!(lines[1][2], ("n", Token::Plain));
        assert!(lines[1].contains(&("`", Token::String)));
        assert_eq!(lines[1].last().unwrap(), &(" done", Token::Plain));
    }

    #[test]
    fn jsx_tags_and_attributes() {
        let ts = Language::TypeScript;
        let code =
            "return <Foo.Bar className=\"x\" onClick={() => go(1)}>\n  <b>hi</b>\n</Foo.Bar>;";
        assert_eq!(token_of(ts, code, "Foo.Bar"), Token::Tag);
        assert_eq!(token_of(ts, code, "className"), Token::Attribute);
        assert_eq!(token_of(ts, code, "\"x\""), Token::String);
        assert_eq!(token_of(ts, code, "go"), Token::Function);
        assert_eq!(token_of(ts, code, "b"), Token::Tag);
        assert_eq!(
            token_of(ts, "  <input\n    disabled\n  />", "disabled"),
            Token::Attribute
        );
        // Comparisons and generics are not tags.
        assert_eq!(token_of(ts, "if (a < b) {}", "b"), Token::Plain);
        assert_eq!(
            token_of(ts, "const m = new Map<string, X>();", "string"),
            Token::Type
        );
    }

    #[test]
    fn python_tokens() {
        let py = Language::Python;
        let code = "@app.route(\"/\")\nclass Handler(Base):\n    async def get(self, n: int = 0x10) -> None:\n        return f\"{n}\" if n else rb'\\x00'  # done";
        assert_eq!(token_of(py, code, "@app.route"), Token::Attribute);
        assert_eq!(token_of(py, code, "Handler"), Token::Type);
        assert_eq!(token_of(py, code, "get"), Token::Function);
        assert_eq!(token_of(py, code, "self"), Token::Constant);
        assert_eq!(token_of(py, code, "int"), Token::Type);
        assert_eq!(token_of(py, code, "0x10"), Token::Number);
        assert_eq!(token_of(py, code, "None"), Token::Constant);
        assert_eq!(token_of(py, code, "f\"{n}\""), Token::String);
        assert_eq!(token_of(py, code, "rb'\\x00'"), Token::String);
        assert_eq!(token_of(py, code, "# done"), Token::Comment);
        assert_eq!(token_of(py, "a @ b", "@"), Token::Plain);

        let lines = lex(
            py,
            "x = '''one\n# not comment\nthree''' + y\n\"\"\"a\"\"\" z",
        );
        assert_eq!(lines[1], vec![("# not comment", Token::String)]);
        assert_eq!(lines[2][0], ("three'''", Token::String));
        assert_eq!(lines[2].last().unwrap(), &(" y", Token::Plain));
        assert_eq!(lines[3][0], ("\"\"\"a\"\"\"", Token::String));
        // Unclosed single-quoted strings end with the line.
        let lines = lex(py, "s = 'open\nx");
        assert_eq!(lines[1], vec![("x", Token::Plain)]);
    }

    #[test]
    fn go_c_ruby_sql_tokens() {
        assert_eq!(
            token_of(Language::Go, "func main() { var x int = nil }", "main"),
            Token::Function
        );
        assert_eq!(
            token_of(Language::Go, "func main() { var x int = nil }", "nil"),
            Token::Constant
        );
        assert_eq!(
            token_of(Language::Go, "type Server struct {}", "Server"),
            Token::Type
        );
        let lines = lex(Language::Go, "s := `raw\n\"line\\\"` + x");
        assert_eq!(lines[1][0], ("\"line\\\"`", Token::String));

        let c = Language::C;
        assert_eq!(
            token_of(c, "#include <stdio.h>", "#include"),
            Token::Attribute
        );
        assert_eq!(
            token_of(c, "#include <stdio.h>", "<stdio.h>"),
            Token::String
        );
        assert_eq!(
            token_of(c, "@Override public void run() {}", "@Override"),
            Token::Attribute
        );
        assert_eq!(
            token_of(c, "int x = a ? 'c' : '\\0';", "'c'"),
            Token::String
        );
        assert_eq!(
            token_of(c, "fun main(args: Array<String>) {}", "main"),
            Token::Function
        );
        assert_eq!(token_of(c, "x = y # z", "#"), Token::Plain);

        let rb = Language::Ruby;
        let code = "def call(x) = @count + $stdin.read + :sym # note";
        assert_eq!(token_of(rb, code, "call"), Token::Function);
        assert_eq!(token_of(rb, code, "@count"), Token::Variable);
        assert_eq!(token_of(rb, code, "$stdin"), Token::Variable);
        assert_eq!(token_of(rb, code, ":sym"), Token::Constant);
        assert_eq!(token_of(rb, code, "# note"), Token::Comment);
        assert_eq!(token_of(rb, "Foo::Bar", "Bar"), Token::Type);

        let sql = Language::Sql;
        let code = "select Count(*) from \"Users\" where name = 'O''Brien' and id = $1 and x::int > :p -- c";
        assert_eq!(token_of(sql, code, "select"), Token::Keyword);
        assert_eq!(token_of(sql, code, "Count"), Token::Function);
        assert_eq!(token_of(sql, code, "\"Users\""), Token::Property);
        assert_eq!(token_of(sql, code, "'O''Brien'"), Token::String);
        assert_eq!(token_of(sql, code, "$1"), Token::Variable);
        assert_eq!(token_of(sql, code, ":p"), Token::Variable);
        assert_eq!(token_of(sql, code, "int"), Token::Type);
        assert_eq!(token_of(sql, code, "-- c"), Token::Comment);
        assert_eq!(token_of(sql, "SELECT NULL", "NULL"), Token::Constant);
    }

    #[test]
    fn shell_tokens() {
        let sh = Language::Shell;
        assert_eq!(
            token_of(sh, "export PATH=\"$HOME\" local", "PATH"),
            Token::Variable
        );
        assert_eq!(
            token_of(sh, "export PATH=\"$HOME\" local", "local"),
            Token::Variable
        );
        assert_eq!(
            token_of(sh, "export PATH=\"$HOME\" local", "="),
            Token::Operator
        );
        let code = "FOO=1 cargo test --workspace -p herdr \"$HOME/${DIR}\" $1 | grep -v x && echo 'lit $X' # done";
        assert_eq!(token_of(sh, code, "FOO"), Token::Variable);
        assert_eq!(token_of(sh, code, "cargo"), Token::Function);
        assert_eq!(token_of(sh, code, "test"), Token::Plain);
        assert_eq!(token_of(sh, code, "--workspace"), Token::Attribute);
        assert_eq!(token_of(sh, code, "-p"), Token::Attribute);
        assert_eq!(token_of(sh, code, "$HOME"), Token::Variable);
        assert_eq!(token_of(sh, code, "${DIR}"), Token::Variable);
        assert_eq!(token_of(sh, code, "/"), Token::String);
        assert_eq!(token_of(sh, code, "$1"), Token::Variable);
        assert_eq!(token_of(sh, code, "|"), Token::Operator);
        assert_eq!(token_of(sh, code, "grep"), Token::Function);
        assert_eq!(token_of(sh, code, "&&"), Token::Operator);
        assert_eq!(token_of(sh, code, "echo"), Token::Function);
        assert_eq!(token_of(sh, code, "'lit $X'"), Token::String);
        assert_eq!(token_of(sh, code, "# done"), Token::Comment);
        assert_eq!(
            token_of(sh, "git log --format=%H#x", "--format=%H#x"),
            Token::Attribute
        );

        let lines = lex(
            sh,
            "$ ls -la\nif true; then make; fi\nfor f in a b; do\n  rm \\\n  -rf $f\ndone",
        );
        assert_eq!(lines[0][0], ("$", Token::Punctuation));
        assert_eq!(lines[0][2], ("ls", Token::Function));
        assert!(lines[1].contains(&("if", Token::Keyword)));
        assert!(lines[1].contains(&("true", Token::Function)));
        assert!(lines[1].contains(&("make", Token::Function)));
        assert!(lines[2].contains(&("in", Token::Keyword)));
        assert!(lines[2].contains(&(" a b", Token::Plain)));
        assert!(lines[3].contains(&("rm", Token::Function)));
        assert!(lines[4].contains(&("-rf", Token::Attribute)));
        assert_eq!(lines[5], vec![("done", Token::Keyword)]);

        let lines = lex(sh, "echo \"one\ntwo $X\" after");
        assert_eq!(lines[1][0], ("two ", Token::String));
        assert_eq!(lines[1][1], ("$X", Token::Variable));
        assert_eq!(lines[1].last().unwrap(), &(" after", Token::Plain));
    }

    #[test]
    fn json_tokens() {
        let json = Language::Json;
        let code =
            "{\"key\" : \"va\\\"l\", \"n\": -1.5e3, \"t\": true, \"z\": null, unquoted: 1} // c";
        assert_eq!(token_of(json, code, "\"key\""), Token::Property);
        assert_eq!(token_of(json, code, "\"va\\\"l\""), Token::String);
        assert_eq!(token_of(json, code, "-1.5e3"), Token::Number);
        assert_eq!(token_of(json, code, "true"), Token::Constant);
        assert_eq!(token_of(json, code, "null"), Token::Constant);
        assert_eq!(token_of(json, code, "unquoted"), Token::Property);
        assert_eq!(token_of(json, code, "// c"), Token::Comment);
        let lines = lex(json, "[1, /* a\nb */ 2]");
        assert_eq!(lines[1][0], ("b */", Token::Comment));
        assert!(lines[1].contains(&("2", Token::Number)));
    }

    #[test]
    fn toml_tokens() {
        let toml = Language::Toml;
        let code = "[workspace.dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\nx.y = 1_000 # c\nt = 1979-05-27T07:32:00Z\nok = false";
        assert_eq!(token_of(toml, code, "workspace.dependencies"), Token::Type);
        assert_eq!(token_of(toml, code, "serde"), Token::Property);
        assert_eq!(token_of(toml, code, "version"), Token::Property);
        assert_eq!(token_of(toml, code, "features"), Token::Property);
        assert_eq!(token_of(toml, code, "\"derive\""), Token::String);
        assert_eq!(token_of(toml, code, "1_000"), Token::Number);
        assert_eq!(token_of(toml, code, "# c"), Token::Comment);
        assert_eq!(token_of(toml, code, "1979-05-27T07:32:00Z"), Token::Number);
        assert_eq!(token_of(toml, code, "false"), Token::Constant);
        let lines = lex(toml, "a = [\n  \"x\", 2,\n]\nb = '''\nlit \\n\n'''\nc = 1");
        assert!(lines[1].contains(&("\"x\"", Token::String)));
        assert_eq!(lines[4], vec![("lit \\n", Token::String)]);
        assert_eq!(lines[6][0], ("c", Token::Property));
    }

    #[test]
    fn yaml_tokens() {
        let yaml = Language::Yaml;
        let code = "key: value # c\n- name: \"quoted\"\n  n: 42\n  on: yes\n  ref: &a *b\n  t: !!str x\n  url: http://x:80/a";
        assert_eq!(token_of(yaml, code, "key"), Token::Property);
        assert_eq!(token_of(yaml, code, "value"), Token::String);
        assert_eq!(token_of(yaml, code, "# c"), Token::Comment);
        assert_eq!(token_of(yaml, code, "-"), Token::Punctuation);
        assert_eq!(token_of(yaml, code, "name"), Token::Property);
        assert_eq!(token_of(yaml, code, "\"quoted\""), Token::String);
        assert_eq!(token_of(yaml, code, "42"), Token::Number);
        assert_eq!(token_of(yaml, code, "yes"), Token::Constant);
        assert_eq!(token_of(yaml, code, "&a"), Token::Variable);
        assert_eq!(token_of(yaml, code, "*b"), Token::Variable);
        assert_eq!(token_of(yaml, code, "!!str"), Token::Attribute);
        assert_eq!(token_of(yaml, code, "http://x:80/a"), Token::String);

        let lines = lex(yaml, "- run: |\n    x: 1\n  env: 2");
        assert_eq!(lines[1], vec![("    x: 1", Token::String)]);
        assert_eq!(lines[2][0], ("  env", Token::Property));
        let lines = lex(
            yaml,
            "run: |\n  cargo test: x\n\n  # not a comment\nnext: 1",
        );
        assert_eq!(lines[0][3], ("|", Token::Operator));
        assert_eq!(lines[1], vec![("  cargo test: x", Token::String)]);
        assert!(lines[2].is_empty());
        assert_eq!(lines[3], vec![("  # not a comment", Token::String)]);
        assert_eq!(lines[4][0], ("next", Token::Property));
    }

    #[test]
    fn html_tokens() {
        let html = Language::Html;
        let code = "<a href=\"/x\" data-id=3 hidden>Hi &amp; bye</a><!-- c -->";
        assert_eq!(token_of(html, code, "a"), Token::Tag);
        assert_eq!(token_of(html, code, "href"), Token::Attribute);
        assert_eq!(token_of(html, code, "\"/x\""), Token::String);
        assert_eq!(token_of(html, code, "data-id"), Token::Attribute);
        assert_eq!(token_of(html, code, "3"), Token::String);
        assert_eq!(token_of(html, code, "Hi"), Token::Plain);
        assert_eq!(token_of(html, code, "&amp;"), Token::Constant);
        assert_eq!(token_of(html, code, "<!-- c -->"), Token::Comment);
        let lines = lex(html, "<img\n  src=\"a\"\n/> <!-- one\ntwo --> text");
        assert_eq!(lines[1][1], ("src", Token::Attribute));
        assert_eq!(lines[3][0], ("two -->", Token::Comment));
        assert_eq!(lines[3].last().unwrap(), &(" text", Token::Plain));
    }

    #[test]
    fn css_tokens() {
        let css = Language::Css;
        let code = "@media screen {\n  .card > a:hover, #main {\n    color: #fff !important;\n    margin: -0.5em 10px;\n    width: var(--gap);\n  }\n}";
        assert_eq!(token_of(css, code, "@media"), Token::Keyword);
        let media = "@media (max-width: 600px) and (hover) {";
        assert_eq!(token_of(css, media, "max-width"), Token::Property);
        assert_eq!(token_of(css, media, "600px"), Token::Number);
        assert_eq!(token_of(css, media, "and"), Token::Tag);
        assert_eq!(token_of(css, code, ".card"), Token::Type);
        assert_eq!(token_of(css, "div > a:hover {}", "a"), Token::Tag);
        assert_eq!(token_of(css, code, ":hover"), Token::Attribute);
        assert_eq!(token_of(css, code, "#main"), Token::Attribute);
        assert_eq!(token_of(css, code, "color"), Token::Property);
        assert_eq!(token_of(css, code, "#fff"), Token::Number);
        assert_eq!(token_of(css, code, "!important"), Token::Keyword);
        assert_eq!(token_of(css, code, "-0.5em"), Token::Number);
        assert_eq!(token_of(css, code, "10px"), Token::Number);
        assert_eq!(token_of(css, code, "var"), Token::Function);
        assert_eq!(token_of(css, code, "--gap"), Token::Variable);
        let lines = lex(css, "a { /* x\ny */ color: red }");
        assert_eq!(lines[1][0], ("y */", Token::Comment));
        assert!(lines[1].contains(&("color", Token::Property)));
    }

    #[test]
    fn markdown_tokens() {
        let md = Language::Markdown;
        let lines = lex(
            md,
            "## Heading\nUse `x` and **bold** [docs](https://a.b).\n- item\n```sh\n# not heading\n```\n> quote",
        );
        assert_eq!(lines[0], vec![("## Heading", Token::Keyword)]);
        assert!(lines[1].contains(&("`x`", Token::String)));
        assert!(lines[1].contains(&("**bold**", Token::Type)));
        assert!(lines[1].contains(&("[docs]", Token::Function)));
        assert!(lines[1].contains(&("(https://a.b)", Token::String)));
        assert_eq!(lines[2][0], ("-", Token::Punctuation));
        assert_eq!(
            lines[3],
            vec![("```", Token::Punctuation), ("sh", Token::Attribute)]
        );
        assert_eq!(lines[4], vec![("# not heading", Token::String)]);
        assert_eq!(lines[5], vec![("```", Token::Punctuation)]);
        assert_eq!(lines[6][1], (" quote", Token::Comment));
    }

    #[test]
    fn diff_kinds_and_tokens() {
        let cases = [
            ("diff --git a/x b/x", LineKind::Header),
            ("index 83db48f..bf269f4 100644", LineKind::Header),
            ("--- a/src/main.rs", LineKind::Header),
            ("+++ b/src/main.rs", LineKind::Header),
            ("new file mode 100644", LineKind::Header),
            ("rename from a", LineKind::Header),
            ("@@ -1,3 +1,4 @@ fn main()", LineKind::Hunk),
            ("+added", LineKind::Added),
            ("+", LineKind::Added),
            ("-removed", LineKind::Removed),
            (" context", LineKind::Context),
            ("", LineKind::Context),
            ("\\ No newline at end of file", LineKind::Context),
        ];
        for (line, kind) in cases {
            assert_eq!(diff_line_kind(line), kind, "{line:?}");
        }
        let lines = lex(Language::Diff, "--- a\n@@ -1 +1 @@\n-x\n+y\n z");
        assert_eq!(lines[0], vec![("--- a", Token::Keyword)]);
        assert_eq!(lines[1], vec![("@@ -1 +1 @@", Token::Comment)]);
        assert_eq!(lines[2], vec![("-x", Token::Plain)]);
        assert_eq!(lines[3], vec![("+y", Token::Plain)]);
        assert_eq!(lines[4], vec![(" z", Token::Plain)]);
    }

    /// Long lines of unmatched delimiters must stay linear: each language gets
    /// ~100 KB of adversarial repetition, which a quadratic rescan would take
    /// many seconds to lex even in release mode.
    #[test]
    fn adversarial_lines_stay_linear() {
        let units = [
            "[", "**", "`", "a:b ", "#", "'\\", "\"", "<a ", "${", "/*", "r#", "{", "&", "- ",
            ": ", "$(", "x.", "@a", "0x", "<!--",
        ];
        let languages = [
            Language::Rust,
            Language::TypeScript,
            Language::Python,
            Language::Go,
            Language::Shell,
            Language::Json,
            Language::Toml,
            Language::Yaml,
            Language::Html,
            Language::Css,
            Language::Sql,
            Language::C,
            Language::Ruby,
            Language::Markdown,
            Language::Diff,
        ];
        let started = std::time::Instant::now();
        for unit in units {
            let line = unit.repeat(100_000 / unit.len());
            let nested = format!("a {{ {line}");
            for language in languages {
                lex(language, &line);
                lex(language, &nested);
            }
        }
        // Generous bound for debug builds on slow CI; quadratic behaviour on these
        // inputs takes orders of magnitude longer.
        assert!(started.elapsed() < std::time::Duration::from_secs(60));
    }

    #[test]
    fn language_from_hint() {
        let cases = [
            ("rust", Language::Rust),
            ("RS", Language::Rust),
            ("rust,ignore", Language::Rust),
            ("src/a.rs", Language::Rust),
            ("ts", Language::TypeScript),
            ("tsx", Language::TypeScript),
            ("JavaScript", Language::TypeScript),
            ("x.test.tsx", Language::TypeScript),
            ("web/app.mjs", Language::TypeScript),
            ("py", Language::Python),
            ("python3", Language::Python),
            ("{.python}", Language::Python),
            ("go", Language::Go),
            ("sh", Language::Shell),
            ("bash", Language::Shell),
            ("zsh", Language::Shell),
            ("console", Language::Shell),
            ("Makefile", Language::Shell),
            ("~/.bashrc", Language::Shell),
            ("Dockerfile", Language::Shell),
            ("json", Language::Json),
            ("json5", Language::Json),
            ("package.json", Language::Json),
            ("toml", Language::Toml),
            ("Cargo.lock", Language::Toml),
            ("crates/x/Cargo.toml", Language::Toml),
            ("yml", Language::Yaml),
            (".github/workflows/ci.yaml", Language::Yaml),
            ("html", Language::Html),
            ("icon.svg", Language::Html),
            ("vue", Language::Html),
            ("scss", Language::Css),
            ("sql", Language::Sql),
            ("cpp", Language::C),
            ("Main.java", Language::C),
            ("kotlin", Language::C),
            ("swift", Language::C),
            ("c#", Language::C),
            ("rb", Language::Ruby),
            ("Gemfile", Language::Ruby),
            ("md", Language::Markdown),
            ("README.md", Language::Markdown),
            ("diff", Language::Diff),
            ("patch", Language::Diff),
            ("ts title=\"a.ts\"", Language::TypeScript),
            ("text", Language::Plain),
            ("", Language::Plain),
            ("brainfuck", Language::Plain),
            ("noextension", Language::Plain),
            ("C:\\src\\main.c", Language::C),
        ];
        for (hint, language) in cases {
            assert_eq!(Language::from_hint(hint), language, "{hint:?}");
        }
    }
}
