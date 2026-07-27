//! Hand-written lexer. Produces one flat `Vec<Token>` ending in `Eof`.
//!
//! Whitespace and `//` comments are dropped; `///` doc comments are kept as
//! tokens so the parser can attach them to the declaration that follows.

use std::fmt;

use crate::diag::Diagnostic;
use crate::span::Span;

/// Reserved words. Everything else that reads like a keyword (`endian`,
/// `little`, `max`, `uuid`, `read`, `f32`, `bool`, ...) is lexed as an
/// identifier and recognized by position, so schemas stay free to use those
/// words as field or type names. See GRAMMAR.ebnf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kw {
    Alias,
    As,
    Characteristic,
    Else,
    Enum,
    Padding,
    Reserved,
    Scaled,
    Service,
    String,
    Struct,
}

impl Kw {
    pub fn as_str(self) -> &'static str {
        match self {
            Kw::Alias => "alias",
            Kw::As => "as",
            Kw::Characteristic => "characteristic",
            Kw::Else => "else",
            Kw::Enum => "enum",
            Kw::Padding => "padding",
            Kw::Reserved => "reserved",
            Kw::Scaled => "scaled",
            Kw::Service => "service",
            Kw::String => "string",
            Kw::Struct => "struct",
        }
    }

    fn from_str(s: &str) -> Option<Kw> {
        Some(match s {
            "alias" => Kw::Alias,
            "as" => Kw::As,
            "characteristic" => Kw::Characteristic,
            "else" => Kw::Else,
            "enum" => Kw::Enum,
            "padding" => Kw::Padding,
            "reserved" => Kw::Reserved,
            "scaled" => Kw::Scaled,
            "service" => Kw::Service,
            "string" => Kw::String,
            "struct" => Kw::Struct,
            _ => return None,
        })
    }

    /// Words a misspelled declaration keyword might have meant.
    pub const DECL_KEYWORDS: [Kw; 5] = [Kw::Alias, Kw::Scaled, Kw::Enum, Kw::Struct, Kw::Service];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Punct {
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Colon,
    Semi,
    Comma,
    Eq,
    Hash,
    Minus,
    /// The `---` header separator (§1.1).
    Separator,
}

impl Punct {
    pub fn as_str(self) -> &'static str {
        match self {
            Punct::LBrace => "{",
            Punct::RBrace => "}",
            Punct::LParen => "(",
            Punct::RParen => ")",
            Punct::LBracket => "[",
            Punct::RBracket => "]",
            Punct::Colon => ":",
            Punct::Semi => ";",
            Punct::Comma => ",",
            Punct::Eq => "=",
            Punct::Hash => "#",
            Punct::Minus => "-",
            Punct::Separator => "---",
        }
    }

    /// The token wrapped in backticks, for diagnostics.
    pub fn as_quoted(self) -> &'static str {
        match self {
            Punct::LBrace => "`{`",
            Punct::RBrace => "`}`",
            Punct::LParen => "`(`",
            Punct::RParen => "`)`",
            Punct::LBracket => "`[`",
            Punct::RBracket => "`]`",
            Punct::Colon => "`:`",
            Punct::Semi => "`;`",
            Punct::Comma => "`,`",
            Punct::Eq => "`=`",
            Punct::Hash => "`#`",
            Punct::Minus => "`-`",
            Punct::Separator => "`---`",
        }
    }

    fn is_open(self) -> bool {
        matches!(self, Punct::LBrace | Punct::LParen | Punct::LBracket)
    }

    fn is_close(self) -> bool {
        matches!(self, Punct::RBrace | Punct::RParen | Punct::RBracket)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokKind {
    Kw(Kw),
    Ident(String),
    /// Integer literal, decimal or hex. Kept as `u128` — the widest wire type
    /// is 128 bits, so no schema-legal literal can overflow it.
    Int(u128),
    Float(f64),
    Str(String),
    /// A single `///` line, text only (marker and leading space stripped).
    Doc(String),
    Punct(Punct),
    Eof,
}

impl TokKind {
    /// How this token is named in "expected ..., found ..." messages.
    pub fn describe(&self) -> String {
        match self {
            TokKind::Kw(k) => format!("keyword `{}`", k.as_str()),
            TokKind::Ident(name) => format!("identifier `{name}`"),
            TokKind::Int(v) => format!("integer literal `{v}`"),
            TokKind::Float(v) => format!("number literal `{v}`"),
            TokKind::Str(_) => "string literal".to_string(),
            TokKind::Doc(_) => "doc comment".to_string(),
            TokKind::Punct(p) => format!("`{}`", p.as_str()),
            TokKind::Eof => "end of file".to_string(),
        }
    }

    pub fn is_open_delim(&self) -> bool {
        matches!(self, TokKind::Punct(p) if p.is_open())
    }

    pub fn is_close_delim(&self) -> bool {
        matches!(self, TokKind::Punct(p) if p.is_close())
    }
}

impl fmt::Display for TokKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokKind,
    pub span: Span,
}

/// Tokenizes `src`. Lexical errors are collected rather than thrown: the
/// caller reports all of them at once and does not proceed to parsing (a
/// broken literal would only cause cascading nonsense downstream).
pub fn lex(src: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    Lexer { src, bytes: src.as_bytes(), pos: 0, tokens: Vec::new(), errors: Vec::new() }.run()
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    tokens: Vec<Token>,
    errors: Vec<Diagnostic>,
}

impl<'a> Lexer<'a> {
    fn run(mut self) -> (Vec<Token>, Vec<Diagnostic>) {
        loop {
            self.skip_trivia();
            let start = self.pos;
            let Some(c) = self.peek() else { break };

            match c {
                b'/' if self.starts_with("///") && !self.starts_with("////") => self.doc_comment(),
                b'A'..=b'Z' | b'a'..=b'z' | b'_' => self.word(),
                b'0'..=b'9' => self.number(),
                b'"' => self.string(),
                b'-' => self.dashes(),
                _ => match self.punct(c) {
                    Some(p) => {
                        self.pos += 1;
                        self.push(TokKind::Punct(p), start);
                    }
                    None => self.unexpected_char(),
                },
            }
        }
        let end = Span::new(self.src.len(), self.src.len());
        self.tokens.push(Token { kind: TokKind::Eof, span: end });
        (self.tokens, self.errors)
    }

    fn punct(&self, c: u8) -> Option<Punct> {
        Some(match c {
            b'{' => Punct::LBrace,
            b'}' => Punct::RBrace,
            b'(' => Punct::LParen,
            b')' => Punct::RParen,
            b'[' => Punct::LBracket,
            b']' => Punct::RBracket,
            b':' => Punct::Colon,
            b';' => Punct::Semi,
            b',' => Punct::Comma,
            b'=' => Punct::Eq,
            b'#' => Punct::Hash,
            _ => return None,
        })
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_ascii_whitespace() => self.pos += 1,
                // `///` is a token, so only non-doc `//` runs are trivia here.
                Some(b'/') if self.starts_with("//") && !self.is_doc_start() => {
                    while !matches!(self.peek(), None | Some(b'\n')) {
                        self.pos += 1;
                    }
                }
                // Not valid syntax, but skipping to `*/` keeps one clear error
                // instead of a cascade over the comment's contents.
                Some(b'/') if self.starts_with("/*") => {
                    let start = self.pos;
                    self.pos += 2;
                    while self.peek().is_some() && !self.starts_with("*/") {
                        self.pos += 1;
                    }
                    if self.starts_with("*/") {
                        self.pos += 2;
                    }
                    self.errors.push(
                        Diagnostic::error("block comments are not part of the defgen language")
                            .primary(Span::new(start, self.pos), "unsupported comment syntax")
                            .help("use `//` line comments, or `///` for doc comments"),
                    );
                }
                _ => return,
            }
        }
    }

    fn is_doc_start(&self) -> bool {
        self.starts_with("///") && !self.starts_with("////")
    }

    fn doc_comment(&mut self) {
        let start = self.pos;
        self.pos += 3;
        let text_start = self.pos;
        while !matches!(self.peek(), None | Some(b'\n')) {
            self.pos += 1;
        }
        let text = self.src[text_start..self.pos].trim_end();
        // Strip the single space authors put after `///`, keeping deeper
        // indentation intact so doc formatting survives into generated code.
        let text = text.strip_prefix(' ').unwrap_or(text);
        self.push(TokKind::Doc(text.to_string()), start);
    }

    fn word(&mut self) {
        let start = self.pos;
        while matches!(self.peek(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_')) {
            self.pos += 1;
        }
        let word = &self.src[start..self.pos];
        let kind = match Kw::from_str(word) {
            Some(kw) => TokKind::Kw(kw),
            None => TokKind::Ident(word.to_string()),
        };
        self.push(kind, start);
    }

    fn number(&mut self) {
        let start = self.pos;

        if self.starts_with("0x") || self.starts_with("0X") {
            self.pos += 2;
            let digits_start = self.pos;
            while matches!(self.peek(), Some(c) if c.is_ascii_hexdigit() || c == b'_') {
                self.pos += 1;
            }
            let raw: String = self.src[digits_start..self.pos].replace('_', "");
            let span = Span::new(start, self.pos);
            if raw.is_empty() {
                self.errors.push(
                    Diagnostic::error("hex literal has no digits")
                        .primary(span, "expected at least one hex digit after `0x`"),
                );
                self.push_span(TokKind::Int(0), span);
                return;
            }
            match u128::from_str_radix(&raw, 16) {
                Ok(v) => self.push_span(TokKind::Int(v), span),
                Err(_) => {
                    self.errors.push(self.too_large(span));
                    self.push_span(TokKind::Int(0), span);
                }
            }
            return;
        }

        while matches!(self.peek(), Some(c) if c.is_ascii_digit() || c == b'_') {
            self.pos += 1;
        }

        // A `.` is part of the number only when a digit follows, so `1.` and
        // ranges stay unambiguous.
        let mut is_float = false;
        if self.peek() == Some(b'.') && matches!(self.peek_at(1), Some(c) if c.is_ascii_digit()) {
            is_float = true;
            self.pos += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit() || c == b'_') {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let after = if matches!(self.peek_at(1), Some(b'+' | b'-')) { 2 } else { 1 };
            if matches!(self.peek_at(after), Some(c) if c.is_ascii_digit()) {
                is_float = true;
                self.pos += after;
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
        }

        // `4u8`, `12abc`: a literal running straight into a word is a typo, not
        // two tokens.
        let mut suffix = None;
        if matches!(self.peek(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_')) {
            let suffix_start = self.pos;
            while matches!(self.peek(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_')) {
                self.pos += 1;
            }
            suffix = Some(Span::new(suffix_start, self.pos));
        }

        let span = Span::new(start, self.pos);
        if let Some(suffix_span) = suffix {
            self.errors.push(
                Diagnostic::error(format!(
                    "invalid suffix `{}` on a number literal",
                    suffix_span.text(self.src)
                ))
                .primary(suffix_span, "unexpected characters after the digits")
                .note("defgen number literals are unsuffixed; a field's width comes from its type"),
            );
            self.push_span(TokKind::Int(0), span);
            return;
        }

        let digits: String = span.text(self.src).replace('_', "");
        if is_float {
            match digits.parse::<f64>() {
                Ok(v) => self.push_span(TokKind::Float(v), span),
                Err(_) => {
                    self.errors.push(
                        Diagnostic::error("malformed number literal")
                            .primary(span, "cannot parse this number"),
                    );
                    self.push_span(TokKind::Float(0.0), span);
                }
            }
        } else {
            match digits.parse::<u128>() {
                Ok(v) => self.push_span(TokKind::Int(v), span),
                Err(_) => {
                    self.errors.push(self.too_large(span));
                    self.push_span(TokKind::Int(0), span);
                }
            }
        }
    }

    fn too_large(&self, span: Span) -> Diagnostic {
        Diagnostic::error("integer literal is too large")
            .primary(span, "does not fit in 128 bits")
            .note("the widest defgen wire type is 128 bits, so no valid literal needs more")
    }

    fn string(&mut self) {
        let start = self.pos;
        self.pos += 1; // opening quote
        let mut value = String::new();
        loop {
            match self.peek() {
                None | Some(b'\n') => {
                    self.errors.push(
                        Diagnostic::error("unterminated string literal")
                            .primary(Span::new(start, self.pos), "string is never closed")
                            .secondary(Span::new(start, start + 1), "opened here")
                            .help("string literals must start and end with `\"` on the same line"),
                    );
                    self.push(TokKind::Str(value), start);
                    return;
                }
                Some(b'"') => {
                    self.pos += 1;
                    self.push(TokKind::Str(value), start);
                    return;
                }
                Some(b'\\') => {
                    let esc_start = self.pos;
                    self.pos += 1;
                    let escaped = match self.peek() {
                        Some(b'"') => Some('"'),
                        Some(b'\\') => Some('\\'),
                        Some(b'n') => Some('\n'),
                        Some(b'r') => Some('\r'),
                        Some(b't') => Some('\t'),
                        Some(b'0') => Some('\0'),
                        _ => None,
                    };
                    match escaped {
                        Some(c) => {
                            value.push(c);
                            self.pos += 1;
                        }
                        None => {
                            let end = (self.pos + 1).min(self.src.len());
                            self.errors.push(
                                Diagnostic::error("unknown escape sequence")
                                    .primary(Span::new(esc_start, end), "not a recognized escape")
                                    .help(r#"valid escapes are \" \\ \n \r \t \0"#),
                            );
                            self.pos = end;
                        }
                    }
                }
                Some(_) => {
                    let c = self.src[self.pos..].chars().next().unwrap();
                    value.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }

    /// A run of `-`: exactly three is the header separator, one is a minus
    /// sign (negative `scale`/`offset`), anything else is a typo.
    fn dashes(&mut self) {
        let start = self.pos;
        while self.peek() == Some(b'-') {
            self.pos += 1;
        }
        let span = Span::new(start, self.pos);
        match self.pos - start {
            1 => self.push_span(TokKind::Punct(Punct::Minus), span),
            3 => self.push_span(TokKind::Punct(Punct::Separator), span),
            n => {
                self.errors.push(
                    Diagnostic::error(format!("expected `---`, found a run of {n} dashes"))
                        .primary(span, "not a valid token")
                        .note("the header separator is exactly three dashes"),
                );
                self.push_span(TokKind::Punct(Punct::Separator), span);
            }
        }
    }

    fn unexpected_char(&mut self) {
        let start = self.pos;
        let c = self.src[start..].chars().next().unwrap();
        self.pos += c.len_utf8();
        let span = Span::new(start, self.pos);
        let mut d = Diagnostic::error(format!("unexpected character `{}`", c.escape_debug()))
            .primary(span, "not valid defgen syntax");
        d = match c {
            '\'' => d.help("string literals use double quotes: \"...\""),
            '.' => d.note("a `.` only appears inside a number literal, such as `0.01`"),
            '<' | '>' => d.note("defgen has no generic type syntax; use `Type[N]` for arrays"),
            '*' | '+' | '/' | '%' => d.note("defgen has no arithmetic expressions; values must be literals"),
            _ => d,
        };
        self.errors.push(d);
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    fn starts_with(&self, s: &str) -> bool {
        self.src[self.pos..].starts_with(s)
    }

    fn push(&mut self, kind: TokKind, start: usize) {
        let span = Span::new(start, self.pos);
        self.push_span(kind, span);
    }

    fn push_span(&mut self, kind: TokKind, span: Span) {
        self.tokens.push(Token { kind, span });
    }
}
