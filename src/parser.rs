//! Recursive-descent parser for the defgen IDL (see GRAMMAR.ebnf).
//!
//! Two things shape the implementation:
//!
//! * **Diagnostics over panics.** Every failure produces a [`Diagnostic`] with
//!   a labelled span, and — where the mistake has an obvious intended form —
//!   a `help:` line showing it. The parser tracks the declaration it is inside
//!   so errors deep in a body still say which struct or variant they belong to.
//!
//! * **Recovery.** A failed field does not abort the file: the parser
//!   resynchronizes at the next `,`, the closing delimiter, or the next
//!   top-level declaration, so one run reports as many real errors as it can
//!   without inventing cascading ones.
//!
//! Only syntax and single-literal validity are checked here. Cross-node rules
//! (SPEC.md §11) belong to the semantic pass; see `ast` for the split.

use crate::ast::*;
use crate::diag::{Diagnostic, Severity, suggest};
use crate::lexer::{Kw, Punct, TokKind, Token, lex};
use crate::span::{Span, Spanned};

/// Outcome of parsing one file. `schema` is `Some` exactly when no error-level
/// diagnostic was produced, so downstream stages never see a partial tree.
#[derive(Debug)]
pub struct Parsed {
    pub schema: Option<Schema>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Parsed {
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.severity == Severity::Error)
    }
}

pub fn parse(src: &str) -> Parsed {
    let (tokens, lex_errors) = lex(src);
    // A broken literal or stray character makes the token stream unreliable;
    // parsing on would only produce noise on top of a clear lexical error.
    if !lex_errors.is_empty() {
        return Parsed { schema: None, diagnostics: lex_errors };
    }

    let mut p = Parser { src, tokens, pos: 0, diagnostics: Vec::new(), ctx: Vec::new() };
    let schema = p.parse_file();
    let has_errors = p.diagnostics.iter().any(|d| d.severity == Severity::Error);
    Parsed { schema: if has_errors { None } else { Some(schema) }, diagnostics: p.diagnostics }
}

/// Signals "a diagnostic was already recorded; unwind to the nearest recovery
/// point". Never carries a message — the message is already in `diagnostics`.
#[derive(Debug)]
struct Bail;

type PResult<T> = Result<T, Bail>;

/// What the parser is currently inside, for "in struct `Status`" labels.
struct Ctx {
    what: String,
    span: Span,
}

/// Whether items in a delimited list must be comma-separated (§1: trailing
/// commas are always allowed) or may simply follow one another, as
/// tagged-union variants do (§7).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sep {
    Required,
    Optional,
}

/// Where item-level recovery inside a list landed.
enum Recover {
    /// A separator was consumed; try another item.
    NextItem,
    /// A closing delimiter is next.
    Close,
    /// A new top-level declaration is next — the list is almost certainly
    /// missing its closer, so give up on it.
    DeclStart,
    Eof,
}

struct Parser<'a> {
    src: &'a str,
    tokens: Vec<Token>,
    pos: usize,
    diagnostics: Vec<Diagnostic>,
    ctx: Vec<Ctx>,
}

// ---------------------------------------------------------------------------
// Token access
// ---------------------------------------------------------------------------

impl<'a> Parser<'a> {
    fn peek(&self) -> &TokKind {
        &self.tokens[self.pos].kind
    }

    fn span(&self) -> Span {
        self.tokens[self.pos].span
    }

    fn bump(&mut self) -> Token {
        let tok = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    /// Zero-width span just after the previous token — where a missing `;` or
    /// `,` should have been written.
    fn insertion_point(&self) -> Span {
        let prev = if self.pos == 0 { self.tokens[0].span } else { self.tokens[self.pos - 1].span };
        Span { start: prev.end, end: prev.end }
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek(), TokKind::Eof)
    }

    fn at_punct(&self, p: Punct) -> bool {
        matches!(self.peek(), TokKind::Punct(q) if *q == p)
    }

    fn at_kw(&self, k: Kw) -> bool {
        matches!(self.peek(), TokKind::Kw(q) if *q == k)
    }

    fn at_word(&self, word: &str) -> bool {
        matches!(self.peek(), TokKind::Ident(name) if name == word)
    }

    fn eat_punct(&mut self, p: Punct) -> bool {
        if self.at_punct(p) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn eat_kw(&mut self, k: Kw) -> bool {
        if self.at_kw(k) {
            self.bump();
            true
        } else {
            false
        }
    }

    /// True at a keyword that can only begin a top-level declaration.
    fn at_decl_keyword(&self) -> bool {
        matches!(self.peek(), TokKind::Kw(k) if Kw::DECL_KEYWORDS.contains(k))
    }

    /// True at a token that can begin a top-level declaration — the anchor for
    /// error recovery. Includes `#`, which starts an attribute.
    fn at_decl_start(&self) -> bool {
        self.at_decl_keyword() || self.at_punct(Punct::Hash)
    }
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

impl<'a> Parser<'a> {
    fn diag(&self, span: Span, message: impl Into<String>, label: impl Into<String>) -> Diagnostic {
        let mut d = Diagnostic::error(message).primary(span, label);
        if let Some(ctx) = self.ctx.last() {
            // Skip the context label when it would point at the same place.
            if ctx.span.start != span.start {
                d = d.secondary(ctx.span, format!("in {}", ctx.what));
            }
        }
        d
    }

    fn emit(&mut self, d: Diagnostic) -> Bail {
        self.diagnostics.push(d);
        Bail
    }

    /// `expected <what>, found <token>` at the current token.
    fn expected(&mut self, what: &str) -> Bail {
        let tok = self.tokens[self.pos].clone();
        let d = self.diag(
            tok.span,
            format!("expected {what}, found {}", tok.kind.describe()),
            format!("expected {what}"),
        );
        self.emit(d)
    }

    fn expected_one_of(&mut self, options: &[&str]) -> Bail {
        self.expected(&join_or(options))
    }

    /// Consumes `p`, or reports it as missing. `after` completes the sentence
    /// "expected `;` after the alias declaration" — pass `""` to omit it.
    fn expect(&mut self, p: Punct, after: &str) -> PResult<Span> {
        if self.at_punct(p) {
            return Ok(self.bump().span);
        }
        let tok = self.tokens[self.pos].clone();
        let where_ = if after.is_empty() { String::new() } else { format!(" after {after}") };
        let mut d = self.diag(
            tok.span,
            format!("expected `{}`{where_}, found {}", p.as_str(), tok.kind.describe()),
            format!("expected `{}`", p.as_str()),
        );
        let ins = self.insertion_point();
        if ins.start != tok.span.start {
            d = d.secondary(ins, format!("add `{}` here", p.as_str()));
        }
        Err(self.emit(d))
    }

    fn expect_ident(&mut self, what: &str) -> PResult<Ident> {
        match self.peek().clone() {
            TokKind::Ident(name) => {
                let span = self.bump().span;
                Ok(Ident::new(name, span))
            }
            TokKind::Kw(kw) => {
                let span = self.span();
                let d = self
                    .diag(
                        span,
                        format!("expected {what}, found keyword `{}`", kw.as_str()),
                        format!("`{}` is a reserved word", kw.as_str()),
                    )
                    .help(format!("`{}` cannot be used as a name; pick another one", kw.as_str()));
                Err(self.emit(d))
            }
            _ => Err(self.expected(what)),
        }
    }

    /// A non-negative integer literal.
    fn expect_int(&mut self, what: &str) -> PResult<Spanned<u128>> {
        match self.peek().clone() {
            TokKind::Int(v) => {
                let span = self.bump().span;
                Ok(Spanned::new(v, span))
            }
            TokKind::Punct(Punct::Minus) => {
                let start = self.span();
                self.bump();
                let end = self.span();
                let d = self.diag(start.to(end), format!("{what} cannot be negative"), "negative value");
                Err(self.emit(d))
            }
            TokKind::Float(_) => {
                let span = self.span();
                let d = self
                    .diag(span, format!("{what} must be a whole number"), "found a fractional number")
                    .help("only `scale` and `offset` accept fractional values");
                Err(self.emit(d))
            }
            _ => Err(self.expected(&format!("{what} (an integer literal)"))),
        }
    }

    /// A number for `scale`/`offset`: integer or float, optionally negative.
    fn expect_number(&mut self, what: &str) -> PResult<Spanned<f64>> {
        let start = self.span();
        let negative = self.eat_punct(Punct::Minus);
        let (value, span) = match self.peek().clone() {
            TokKind::Float(v) => (v, self.bump().span),
            TokKind::Int(v) => (v as f64, self.bump().span),
            _ => return Err(self.expected(&format!("a number for `{what}`"))),
        };
        let span = if negative { start.to(span) } else { span };
        Ok(Spanned::new(if negative { -value } else { value }, span))
    }
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

impl<'a> Parser<'a> {
    /// Skips tokens until the next top-level declaration or EOF, keeping track
    /// of nesting so a `}` belonging to the broken declaration is consumed
    /// rather than mistaken for the end of the file's structure.
    fn sync_to_decl(&mut self) {
        self.ctx.clear();
        let mut depth: u32 = 0;
        loop {
            if self.at_eof() {
                return;
            }
            if depth == 0 && self.at_decl_start() {
                return;
            }
            let kind = self.peek().clone();
            if kind.is_open_delim() {
                depth += 1;
            } else if kind.is_close_delim() {
                depth = depth.saturating_sub(1);
            }
            self.bump();
        }
    }

    /// Skips the remains of a broken list item, stopping at the next separator,
    /// the closing delimiter, a new declaration, or EOF.
    fn recover_in_list(&mut self, close: Punct) -> Recover {
        let mut depth: u32 = 0;
        loop {
            if self.at_eof() {
                return Recover::Eof;
            }
            if depth == 0 {
                if self.at_punct(close) {
                    return Recover::Close;
                }
                // `;` also ends an item: it separates characteristic bindings,
                // and elsewhere it is a plausible typo for `,`.
                if self.at_punct(Punct::Comma) || self.at_punct(Punct::Semi) {
                    self.bump();
                    return Recover::NextItem;
                }
                if self.peek().is_close_delim() {
                    // A different closer: let the caller report the mismatch.
                    return Recover::Close;
                }
                if self.at_decl_start() || self.at_punct(Punct::Separator) {
                    return Recover::DeclStart;
                }
            }
            let kind = self.peek().clone();
            if kind.is_open_delim() {
                depth += 1;
            } else if kind.is_close_delim() {
                depth -= 1;
            }
            self.bump();
        }
    }

    fn unclosed(&mut self, open: Punct, open_span: Span, close: Punct) -> Bail {
        let d = self
            .diag(
                self.span(),
                format!(
                    "unclosed `{}`: expected `{}`, found {}",
                    open.as_str(),
                    close.as_str(),
                    self.peek().describe()
                ),
                format!("expected `{}`", close.as_str()),
            )
            .secondary(open_span, format!("unclosed `{}`", open.as_str()));
        self.emit(d)
    }

    /// `open item (sep item)* sep? close`, recovering per item. Returns the
    /// items and the span of the whole group.
    fn delimited<T>(
        &mut self,
        open: Punct,
        close: Punct,
        sep: Sep,
        after_open: &str,
        item: fn(&mut Self) -> PResult<T>,
    ) -> PResult<(Vec<T>, Span)> {
        let open_span = self.expect(open, after_open)?;
        let mut items = Vec::new();
        loop {
            if self.at_punct(close) {
                let close_span = self.bump().span;
                return Ok((items, open_span.to(close_span)));
            }
            if self.at_eof() {
                return Err(self.unclosed(open, open_span, close));
            }
            // A new top-level declaration inside a list body means the closing
            // delimiter was forgotten; say that instead of complaining about
            // `struct` not being a valid list item.
            if self.at_decl_keyword() || self.at_punct(Punct::Separator) {
                return Err(self.unclosed(open, open_span, close));
            }

            let outcome = match item(self) {
                Ok(v) => {
                    items.push(v);
                    if self.eat_punct(Punct::Comma) {
                        continue;
                    }
                    if self.at_punct(close) {
                        let close_span = self.bump().span;
                        return Ok((items, open_span.to(close_span)));
                    }
                    if sep == Sep::Optional && !self.at_eof() && !self.at_decl_start() {
                        continue;
                    }
                    if self.at_eof() {
                        return Err(self.unclosed(open, open_span, close));
                    }
                    // The item is complete but nothing separates it from what
                    // follows — almost always a forgotten `,`.
                    let mut options: Vec<&str> = match sep {
                        Sep::Required => vec!["`,`"],
                        Sep::Optional => vec![],
                    };
                    options.push(close.as_quoted());
                    let tok = self.tokens[self.pos].clone();
                    let mut d = self.diag(
                        tok.span,
                        format!("expected {}, found {}", join_or(&options), tok.kind.describe()),
                        format!("expected {}", join_or(&options)),
                    );
                    if sep == Sep::Required {
                        d = d.secondary(self.insertion_point(), "add `,` here");
                    }
                    self.emit(d);
                    self.recover_in_list(close)
                }
                Err(Bail) => self.recover_in_list(close),
            };

            match outcome {
                Recover::NextItem => continue,
                Recover::Close => {
                    if self.at_punct(close) {
                        let close_span = self.bump().span;
                        return Ok((items, open_span.to(close_span)));
                    }
                    return Err(self.unclosed(open, open_span, close));
                }
                Recover::DeclStart => return Err(self.unclosed(open, open_span, close)),
                Recover::Eof => return Err(self.unclosed(open, open_span, close)),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// File and header (§1.1)
// ---------------------------------------------------------------------------

struct Header {
    endian: Option<Spanned<Endianness>>,
    separator: Option<Span>,
}

impl<'a> Parser<'a> {
    fn parse_file(&mut self) -> Schema {
        let header = self.parse_header();
        let decls = self.parse_decls();
        Schema { endian: header.endian, separator: header.separator, decls }
    }

    /// True if the upcoming tokens open a file header (§1.1) — an `endian`
    /// pragma and/or a bare `---` — rather than declarations starting right
    /// away. The header is entirely optional: a file with neither just
    /// starts with its declarations, and the default byte order (little)
    /// applies (§8).
    fn looks_like_header(&self) -> bool {
        let mut i = self.pos;
        while matches!(self.tokens[i].kind, TokKind::Doc(_)) {
            i += 1;
        }
        matches!(&self.tokens[i].kind, TokKind::Ident(name) if name == "endian")
            || matches!(self.tokens[i].kind, TokKind::Punct(Punct::Separator))
    }

    fn parse_header(&mut self) -> Header {
        let mut header = Header { endian: None, separator: None };
        if !self.looks_like_header() {
            return header;
        }
        loop {
            match self.peek().clone() {
                // `//` comments are trivia; a `///` here documents nothing.
                TokKind::Doc(_) => {
                    let span = self.bump().span;
                    let d = Diagnostic::warning("doc comment in the file header documents nothing")
                        .primary(span, "not attached to a declaration")
                        .help("use `//` for a plain comment, or move this above a declaration below `---`");
                    self.emit(d);
                }
                TokKind::Punct(Punct::Separator) => {
                    header.separator = Some(self.bump().span);
                    return header;
                }
                TokKind::Eof => {
                    let d = Diagnostic::error("missing `---` separator")
                        .primary(self.span(), "expected `---` before the end of the file")
                        .note("a file header (an `endian` pragma) is optional, but if present must be followed by `---` then declarations (§1.1)")
                        .help("add a line containing only `---` after the header");
                    self.emit(d);
                    return header;
                }
                TokKind::Ident(name) if name == "endian" => match self.parse_endian_pragma() {
                    Ok(v) => self.set_once(&mut header.endian, v, "endian"),
                    Err(Bail) => self.sync_in_header(),
                },
                // A declaration above the separator: the header is over, and
                // the author forgot the `---`.
                _ if self.at_decl_start() => {
                    let span = self.span();
                    let d = Diagnostic::error("declaration appears above the `---` separator")
                        .primary(span, "declarations belong below `---`")
                        .note("only the `endian` pragma may appear in the file header (§1.1)")
                        .help("add a line containing only `---` between the header and the declarations");
                    self.emit(d);
                    return header;
                }
                _ => {
                    let _ = self.expected_one_of(&["`endian`", "`---`"]);
                    self.sync_in_header();
                    if self.at_eof() {
                        return header;
                    }
                }
            }
        }
    }

    /// Skips to the next `;`, `---`, or declaration after a bad pragma.
    fn sync_in_header(&mut self) {
        loop {
            match self.peek() {
                TokKind::Eof | TokKind::Punct(Punct::Separator) => return,
                TokKind::Punct(Punct::Semi) => {
                    self.bump();
                    return;
                }
                _ if self.at_decl_start() => return,
                _ => {
                    self.bump();
                }
            }
        }
    }

    fn set_once<T>(&mut self, slot: &mut Option<Spanned<T>>, value: Spanned<T>, name: &str) {
        match slot {
            Some(first) => {
                let d = Diagnostic::error(format!("the `{name}` pragma is declared more than once"))
                    .primary(value.span, format!("second `{name}` declaration"))
                    .secondary(first.span, "first declared here")
                    .note(format!("`{name}` is a file-wide setting and must appear exactly once (§1.1)"));
                self.emit(d);
            }
            None => *slot = Some(value),
        }
    }

    fn parse_endian_pragma(&mut self) -> PResult<Spanned<Endianness>> {
        let kw_span = self.bump().span; // `endian`
        if self.at_punct(Punct::Eq) {
            let span = self.span();
            let d = self
                .diag(span, "the `endian` pragma is written with `:`, not `=`", "expected `:`")
                .help("write `endian: little;`");
            return Err(self.emit(d));
        }
        self.expect(Punct::Colon, "`endian`")?;
        let word = self.expect_ident("`little` or `big`")?;
        let value = match word.name.as_str() {
            "little" => Endianness::Little,
            "big" => Endianness::Big,
            other => {
                let mut d = self.diag(
                    word.span,
                    format!("unknown byte order `{other}`"),
                    "expected `little` or `big`",
                );
                if let Some(s) = suggest(other, &["little", "big"]) {
                    d = d.help(format!("did you mean `{s}`?"));
                }
                return Err(self.emit(d));
            }
        };
        let span = kw_span.to(word.span);
        self.expect(Punct::Semi, "the `endian` pragma")?;
        Ok(Spanned::new(value, span))
    }
}

// ---------------------------------------------------------------------------
// Declarations
// ---------------------------------------------------------------------------

impl<'a> Parser<'a> {
    fn parse_decls(&mut self) -> Vec<Decl> {
        let mut decls = Vec::new();
        while !self.at_eof() {
            self.ctx.clear();
            match self.parse_decl() {
                Ok(Some(decl)) => decls.push(decl),
                Ok(None) => {}
                Err(Bail) => self.sync_to_decl(),
            }
        }
        decls
    }

    fn parse_decl(&mut self) -> PResult<Option<Decl>> {
        // A second `---`: report it and carry on with the declarations.
        if self.at_punct(Punct::Separator) {
            let span = self.bump().span;
            let d = Diagnostic::error("unexpected `---`")
                .primary(span, "the header separator may appear only once")
                .note("everything after the first `---` is a type or service declaration (§1.1)");
            self.emit(d);
            return Ok(None);
        }

        let docs = self.collect_docs();
        let attrs = self.parse_attrs()?;
        // `///` after `#[...]`: the spec fixes doc-then-attribute order.
        let docs = if matches!(self.peek(), TokKind::Doc(_)) {
            let span = self.span();
            let extra = self.collect_docs();
            let d = Diagnostic::error("doc comment must come before the attribute")
                .primary(span, "doc comment after `#[...]`")
                .note("the declaration order is `///` doc comments, then `#[...]` attributes (§1.2)");
            self.emit(d);
            docs.into_iter().chain(extra).collect()
        } else {
            docs
        };

        if self.at_eof() {
            if !docs.is_empty() {
                let span = docs.first().unwrap().span.to(docs.last().unwrap().span);
                let d = Diagnostic::error("doc comment documents nothing")
                    .primary(span, "no declaration follows")
                    .help("use `//` for a plain comment");
                self.emit(d);
            }
            if !attrs.is_empty() {
                let d = Diagnostic::error("attribute is not attached to a declaration")
                    .primary(attrs[0].span, "no declaration follows");
                self.emit(d);
            }
            return Ok(None);
        }

        match self.peek().clone() {
            TokKind::Kw(Kw::Alias) => {
                self.reject_attrs(&attrs, "an `alias`");
                Ok(Some(Decl::Alias(self.parse_alias(docs)?)))
            }
            TokKind::Kw(Kw::Scaled) => {
                self.reject_attrs(&attrs, "a `scaled` declaration");
                Ok(Some(Decl::Scaled(self.parse_scaled(docs)?)))
            }
            TokKind::Kw(Kw::Enum) => self.parse_enum_or_union(docs, attrs).map(Some),
            TokKind::Kw(Kw::Struct) => Ok(Some(Decl::Struct(self.parse_struct(docs, attrs)?))),
            TokKind::Kw(Kw::Const) => {
                self.reject_attrs(&attrs, "a `const`");
                Ok(Some(Decl::Const(self.parse_const(docs)?)))
            }
            TokKind::Kw(Kw::Service) => {
                self.reject_attrs(&attrs, "a `service`");
                Ok(Some(Decl::Service(self.parse_service(docs)?)))
            }
            // Pragma below the separator.
            TokKind::Ident(name) if name == "endian" => {
                let span = self.span();
                let d = Diagnostic::error("the `endian` pragma must appear in the file header".to_string())
                    .primary(span, "found below the `---` separator")
                    .note("`endian` is a file-wide setting and lives above `---` (§1.1)")
                    .help("move this `endian` line above the `---`");
                Err(self.emit(d))
            }
            TokKind::Kw(Kw::Characteristic) => {
                let span = self.span();
                let d = self
                    .diag(
                        span,
                        "`characteristic` may only appear inside a `service` block",
                        "not at file scope",
                    )
                    .note("characteristic bindings are grouped under a service UUID (§10)")
                    .help("wrap it in `service Name(uuid: \"...\") { ... }`");
                Err(self.emit(d))
            }
            TokKind::Ident(name) => {
                let span = self.span();
                let mut d = self.diag(
                    span,
                    format!("expected a declaration, found identifier `{name}`"),
                    "expected `alias`, `scaled`, `enum`, `struct`, `const` or `service`",
                );
                let keywords: Vec<&str> = Kw::DECL_KEYWORDS.iter().map(|k| k.as_str()).collect();
                if let Some(s) = suggest(&name, &keywords) {
                    d = d.help(format!("did you mean `{s}`?"));
                }
                Err(self.emit(d))
            }
            _ => Err(self.expected_one_of(&[
                "`alias`",
                "`scaled`",
                "`enum`",
                "`struct`",
                "`const`",
                "`service`",
            ])),
        }
    }

    fn collect_docs(&mut self) -> Docs {
        let mut docs = Vec::new();
        while let TokKind::Doc(text) = self.peek().clone() {
            let span = self.bump().span;
            docs.push(Doc { text, span });
        }
        docs
    }

    /// Parses any number of `#[...]` attributes, resolving them against the
    /// recognized set (§1.2). Unknown attributes are reported but skipped, so
    /// the declaration itself still gets parsed.
    fn parse_attrs(&mut self) -> PResult<Vec<Attr>> {
        let mut attrs: Vec<Attr> = Vec::new();
        while self.at_punct(Punct::Hash) {
            let hash = self.bump().span;
            self.expect(Punct::LBracket, "`#`")?;
            let name = self.expect_ident("an attribute name")?;

            match name.name.as_str() {
                "endian" => {
                    let value = self.parse_endian_attr_arg(&name)?;
                    let close = self.expect(Punct::RBracket, "the attribute")?;
                    let span = hash.to(close);
                    if let Some(previous) = attrs.iter().find(|a| matches!(a.kind, AttrKind::Endian(_))) {
                        let d = Diagnostic::error("duplicate `endian` attribute")
                            .primary(span, "second `#[endian(...)]` on this declaration")
                            .secondary(previous.span, "first one here")
                            .note("a container has exactly one byte order (§8)");
                        self.emit(d);
                    } else {
                        attrs.push(Attr { kind: AttrKind::Endian(value), span });
                    }
                }
                other => {
                    let mut d = self
                        .diag(name.span, format!("unknown attribute `{other}`"), "not a recognized attribute")
                        .note("v1 recognizes only `endian(little)` / `endian(big)` (§1.2)");
                    if let Some(s) = suggest(other, &["endian"]) {
                        d = d.help(format!("did you mean `{s}`?"));
                    }
                    self.emit(d);
                    // Skip the rest of the attribute so the declaration below
                    // still parses.
                    let mut depth = 1;
                    while depth > 0 && !self.at_eof() {
                        let kind = self.bump().kind;
                        if kind.is_open_delim() {
                            depth += 1;
                        } else if kind.is_close_delim() {
                            depth -= 1;
                        }
                    }
                }
            }
        }
        Ok(attrs)
    }

    fn parse_endian_attr_arg(&mut self, name: &Ident) -> PResult<Spanned<Endianness>> {
        if !self.at_punct(Punct::LParen) {
            let d = self
                .diag(
                    name.span,
                    "`endian` attribute needs a byte order argument",
                    "expected `(little)` or `(big)`",
                )
                .help("write `#[endian(little)]` or `#[endian(big)]`");
            return Err(self.emit(d));
        }
        self.bump(); // `(`
        let word = self.expect_ident("`little` or `big`")?;
        let value = match word.name.as_str() {
            "little" => Endianness::Little,
            "big" => Endianness::Big,
            other => {
                let mut d = self.diag(
                    word.span,
                    format!("unknown byte order `{other}`"),
                    "expected `little` or `big`",
                );
                if let Some(s) = suggest(other, &["little", "big"]) {
                    d = d.help(format!("did you mean `{s}`?"));
                }
                return Err(self.emit(d));
            }
        };
        self.eat_punct(Punct::Comma);
        self.expect(Punct::RParen, "the attribute argument")?;
        Ok(Spanned::new(value, word.span))
    }

    /// Attributes are only legal on `struct` and tagged-union `enum` (§1.2).
    fn reject_attrs(&mut self, attrs: &[Attr], what: &str) {
        for attr in attrs {
            let d = Diagnostic::error(format!("attributes cannot be applied to {what}"))
                .primary(attr.span, "not allowed here")
                .secondary(self.span(), format!("{what} starts here"))
                .note("only a `struct` or a tagged-union `enum` has its own byte order, so only those accept `#[endian(...)]` (§1.2, §8)");
            self.emit(d);
        }
    }
}

// ---------------------------------------------------------------------------
// alias / scaled (§3, §4)
// ---------------------------------------------------------------------------

impl<'a> Parser<'a> {
    fn parse_alias(&mut self, docs: Docs) -> PResult<AliasDecl> {
        let kw_span = self.bump().span; // `alias`
        let name = self.expect_ident("an alias name")?;
        self.ctx.push(Ctx { what: format!("alias `{}`", name.name), span: name.span });

        if self.at_punct(Punct::Colon) {
            let span = self.span();
            let d = self
                .diag(span, "an `alias` is written with `=`, not `:`", "expected `=`")
                .help(format!("write `alias {} = u8;`", name.name));
            return Err(self.emit(d));
        }
        self.expect(Punct::Eq, "the alias name")?;
        let target = self.parse_field_type()?;
        let semi = self.expect(Punct::Semi, "the alias target type")?;
        self.ctx.pop();
        Ok(AliasDecl { docs, name, target, span: kw_span.to(semi) })
    }

    fn parse_scaled(&mut self, docs: Docs) -> PResult<ScaledDecl> {
        let kw_span = self.bump().span; // `scaled`
        let name = self.expect_ident("a scaled-type name")?;
        self.ctx.push(Ctx { what: format!("scaled type `{}`", name.name), span: name.span });

        self.expect(Punct::Colon, "the scaled-type name")?;
        let raw = self.parse_scalar_type()?;
        match raw.kind {
            ScalarKind::UInt(_) | ScalarKind::Int(_) => {}
            _ => {
                let found = raw.span.text(self.src);
                let d = self
                    .diag(
                        raw.span,
                        format!("`scaled` must wrap an integer wire type, found `{found}`"),
                        "expected `uN` or `iN`",
                    )
                    .note("a scaled value is a fixed-point integer on the wire; `bool` and enums have no scale (§4)")
                    .help("use an integer raw type, e.g. `scaled Temperature: i16 as f32 (scale: 0.01);`");
                return Err(self.emit(d));
            }
        }

        if !self.eat_kw(Kw::As) {
            let span = self.span();
            let d = self
                .diag(span, "expected `as` after the raw type of a `scaled` declaration", "expected `as`")
                .note("a `scaled` declaration always names both types: `RawType as PhysicalType` (§4)")
                .help(format!("write `scaled {}: i16 as f32 (scale: 0.01);`", name.name));
            return Err(self.emit(d));
        }

        let phys_ident = self.expect_ident("`f32` or `f64`")?;
        let physical = match phys_ident.name.as_str() {
            "f32" => FloatType::F32,
            "f64" => FloatType::F64,
            other => {
                let mut d = self
                    .diag(
                        phys_ident.span,
                        format!("`{other}` is not a physical type"),
                        "expected `f32` or `f64`",
                    )
                    .note("the decoded representation of a scaled value is always a float (§4)");
                if let Some(s) = suggest(other, &["f32", "f64"]) {
                    d = d.help(format!("did you mean `{s}`?"));
                }
                return Err(self.emit(d));
            }
        };
        let physical = Spanned::new(physical, phys_ident.span);

        // `(scale: <num>[, offset: <num>])`
        let args_span_start = self.span();
        if !self.at_punct(Punct::LParen) {
            let d = self
                .diag(args_span_start, "expected `(scale: ...)` after the physical type", "expected `(`")
                .note("`scale` is mandatory: the whole point of a `scaled` declaration is to state the conversion (§4)")
                .help("write `(scale: 0.01)`, optionally `(scale: 0.01, offset: -40.0)`");
            return Err(self.emit(d));
        }
        let mut scale: Option<Spanned<f64>> = None;
        let mut offset: Option<Spanned<f64>> = None;
        let (_, args_span) = self.delimited(Punct::LParen, Punct::RParen, Sep::Required, "the physical type", |p| {
            let arg = p.expect_ident("`scale` or `offset`")?;
            p.expect(Punct::Colon, &format!("`{}`", arg.name))?;
            match arg.name.as_str() {
                "scale" => {
                    let value = p.expect_number("scale")?;
                    if value.value == 0.0 {
                        let d = p
                            .diag(value.span, "`scale` must not be zero", "zero scale")
                            .note("encoding divides by `scale` (`raw = round((physical - offset) / scale)`), so zero has no meaning (§4)");
                        return Err(p.emit(d));
                    }
                    Ok(ScaledArg::Scale(Spanned::new(value.value, arg.span.to(value.span))))
                }
                "offset" => {
                    let value = p.expect_number("offset")?;
                    Ok(ScaledArg::Offset(Spanned::new(value.value, arg.span.to(value.span))))
                }
                other => {
                    let mut d = p
                        .diag(arg.span, format!("unknown `scaled` argument `{other}`"), "expected `scale` or `offset`")
                        .note("a `scaled` declaration takes only `scale` and `offset`; units belong in a `///` comment (§4)");
                    if let Some(s) = suggest(other, &["scale", "offset"]) {
                        d = d.help(format!("did you mean `{s}`?"));
                    }
                    Err(p.emit(d))
                }
            }
        })
        .map(|(args, span)| {
            for arg in args {
                match arg {
                    ScaledArg::Scale(v) => scale = Some(v),
                    ScaledArg::Offset(v) => offset = Some(v),
                }
            }
            ((), span)
        })?;

        let scale = match scale {
            Some(s) => s,
            None => {
                let d = self
                    .diag(args_span, "`scaled` declaration is missing `scale`", "expected a `scale: <number>` argument")
                    .note("`physical = raw * scale + offset`; `offset` defaults to 0 but `scale` has no default (§4)")
                    .help("add `scale: 0.01` (whatever the device's resolution is)");
                return Err(self.emit(d));
            }
        };

        let semi = self.expect(Punct::Semi, "the `scaled` declaration")?;
        self.ctx.pop();
        Ok(ScaledDecl { docs, name, raw, physical, scale, offset, span: kw_span.to(semi) })
    }
}

enum ScaledArg {
    Scale(Spanned<f64>),
    Offset(Spanned<f64>),
}

// ---------------------------------------------------------------------------
// const (§new)
// ---------------------------------------------------------------------------

impl<'a> Parser<'a> {
    fn parse_const(&mut self, docs: Docs) -> PResult<ConstDecl> {
        let kw_span = self.bump().span; // `const`
        let name = self.expect_ident("a constant name")?;
        self.ctx.push(Ctx { what: format!("constant `{}`", name.name), span: name.span });

        self.expect(Punct::Colon, "the constant name")?;
        let ty = self.parse_scalar_type()?;
        match ty.kind {
            ScalarKind::UInt(_) | ScalarKind::Int(_) => {}
            _ => {
                let found = ty.span.text(self.src).to_string();
                let d = self
                    .diag(
                        ty.span,
                        format!("a `const` must be an integer wire type, found `{found}`"),
                        "expected `uN` or `iN`",
                    )
                    .note("a constant is a plain integer value shared into generated code; `bool` and named types have no meaning here")
                    .help("use an integer type, e.g. `const MaxRetries: u8 = 5;`");
                return Err(self.emit(d));
            }
        }

        self.expect(Punct::Eq, "the constant's type")?;
        let value = self.parse_const_value()?;
        let semi = self.expect(Punct::Semi, "the constant's value")?;
        self.ctx.pop();
        Ok(ConstDecl { docs, name, ty, value, span: kw_span.to(semi) })
    }

    /// A constant's value: an optionally-negative integer literal. Whether the
    /// sign and magnitude actually fit the declared type is a cross-node rule
    /// (it needs the type), so it is left to the semantic pass, the same way
    /// an enum variant's fit against its backing width is (§5, §11).
    fn parse_const_value(&mut self) -> PResult<Spanned<ConstLit>> {
        let start = self.span();
        let negative = self.eat_punct(Punct::Minus);
        match self.peek().clone() {
            TokKind::Int(v) => {
                let end = self.bump().span;
                let span = if negative { start.to(end) } else { end };
                Ok(Spanned::new(ConstLit { magnitude: v, negative }, span))
            }
            TokKind::Float(_) => {
                let span = self.span();
                let d = self
                    .diag(span, "a constant's value must be a whole number", "found a fractional number")
                    .help("defgen constants are plain integers; use `scaled` for a fixed-point physical value (§4)");
                Err(self.emit(d))
            }
            _ => Err(self.expected("a constant value (an integer literal)")),
        }
    }
}

// ---------------------------------------------------------------------------
// Types (§2, §6.1, §6.3)
// ---------------------------------------------------------------------------

impl<'a> Parser<'a> {
    /// A scalar: primitive or declared name. Array suffixes and `string` are
    /// handled by [`Parser::parse_field_type`].
    fn parse_scalar_type(&mut self) -> PResult<ScalarType> {
        match self.peek().clone() {
            TokKind::Ident(name) => {
                let span = self.bump().span;
                self.classify_scalar(&name, span)
            }
            TokKind::Kw(Kw::String) => {
                let span = self.span();
                let d = self
                    .diag(span, "`string` cannot be used here", "expected a fixed-width type")
                    .note("a `string(max: N)` is variable-length, so it may only be a field's own type (always last) or an alias target (§6.3)")
                    .help("for a fixed-width text field, use a `u8[N]` array instead");
                Err(self.emit(d))
            }
            TokKind::Int(v) => {
                let span = self.span();
                let d = self
                    .diag(span, "expected a type, found a number", "types are written like `u8`")
                    .help(format!("did you mean `u{v}`?"));
                Err(self.emit(d))
            }
            _ => Err(self.expected("a type")),
        }
    }

    fn classify_scalar(&mut self, name: &str, span: Span) -> PResult<ScalarType> {
        if name == "bool" {
            return Ok(ScalarType { kind: ScalarKind::Bool, span });
        }
        if name == "f32" || name == "f64" {
            let d = self
                .diag(span, format!("`{name}` is not a wire type"), "floats never appear on the wire")
                .note("defgen has no raw floating-point wire type; a float only exists as the decoded form of a `scaled` declaration (§2, §4)")
                .help("declare `scaled Name: i16 as f32 (scale: 0.01);` and use `Name` as the field type");
            return Err(self.emit(d));
        }
        if let Some(kind) = self.classify_int_type(name, span)? {
            return Ok(ScalarType { kind, span });
        }
        Ok(ScalarType { kind: ScalarKind::Named(Ident::new(name, span)), span })
    }

    /// Recognizes `uN`/`iN` and range-checks `N` (§2). Returns `Ok(None)` for
    /// anything that isn't shaped like an integer type at all.
    fn classify_int_type(&mut self, name: &str, span: Span) -> PResult<Option<ScalarKind>> {
        let (prefix, digits) = name.split_at(1);
        let signed = match prefix {
            "u" => false,
            "i" => true,
            _ => return Ok(None),
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(None);
        }

        let min = if signed { 2 } else { 1 };
        let width: Option<u32> = digits.parse().ok().filter(|w| (min..=128).contains(w));
        let Some(width) = width else {
            let mut d = self
                .diag(
                    span,
                    format!("`{name}` is not a valid integer type"),
                    format!("width must be between {min} and 128"),
                )
                .note(format!("`{prefix}N` is exactly N bits wide, with {min} <= N <= 128 (§2)"));
            if signed && digits == "1" {
                d = d.help("a signed integer needs a sign bit plus at least one value bit; use `u1` or `bool` for a single bit");
            } else if digits == "0" {
                d = d.help(
                    "a zero-width field has no meaning; remove it, or use `padding` if you meant unused bits",
                );
            }
            return Err(self.emit(d));
        };
        Ok(Some(if signed { ScalarKind::Int(width) } else { ScalarKind::UInt(width) }))
    }

    /// A full field type: `string(max: N)`, `Type`, `Type[N]` or
    /// `Type[max: N]`.
    fn parse_field_type(&mut self) -> PResult<FieldType> {
        if self.at_kw(Kw::String) {
            let kw_span = self.bump().span;
            let max = self.parse_string_bound(kw_span)?;
            let span = kw_span.to(max.span);
            if self.at_punct(Punct::LBracket) {
                let bracket = self.span();
                let d = self
                    .diag(bracket, "an array of `string` is not supported", "cannot index a `string`")
                    .note("a container may hold at most one variable-length field, always last (§6.3)")
                    .help("model repeated text as its own characteristic instead");
                return Err(self.emit(d));
            }
            return Ok(FieldType { kind: FieldTypeKind::Str { max }, span });
        }

        let elem = self.parse_scalar_type()?;
        if !self.at_punct(Punct::LBracket) {
            let span = elem.span;
            return Ok(FieldType { kind: FieldTypeKind::Scalar(elem), span });
        }

        self.bump(); // `[`
        // `[max: N]` is the variable-length form, `[N]` the fixed one.
        if self.at_word("max") {
            self.bump(); // `max`
            self.expect(Punct::Colon, "`max`")?;
            let bound = self.parse_positive_bound("max")?;
            let close = self.expect(Punct::RBracket, "the `max` bound")?;
            return Ok(FieldType {
                kind: FieldTypeKind::VarArray { elem: elem.clone(), max: bound },
                span: elem.span.to(close),
            });
        }
        if matches!(self.peek(), TokKind::Ident(_)) {
            let ident_span = self.span();
            let word = self.span().text(self.src).to_string();
            let mut d = self.diag(
                ident_span,
                format!("expected an array length, found `{word}`"),
                "array lengths are literal integers",
            )
            .note("an array is either fixed (`Type[4]`) or variable-length (`Type[max: 4]`); there is no length-from-another-field form (§6.1, §14)");
            if let Some(s) = suggest(&word, &["max"]) {
                d = d.help(format!("did you mean `{s}: N`?"));
            }
            return Err(self.emit(d));
        }

        let count = self.expect_int("an array length")?;
        let count_u64 = u64::try_from(count.value).map_err(|_| {
            let d = self.diag(count.span, "array length is too large", "does not fit in 64 bits");
            self.emit(d)
        })?;
        let close = self.expect(Punct::RBracket, "the array length")?;
        Ok(FieldType {
            kind: FieldTypeKind::FixedArray {
                elem: elem.clone(),
                count: Spanned::new(count_u64, count.span),
            },
            span: elem.span.to(close),
        })
    }

    fn parse_string_bound(&mut self, kw_span: Span) -> PResult<Spanned<u64>> {
        if !self.at_punct(Punct::LParen) {
            let d = self
                .diag(kw_span, "`string` requires a maximum byte length", "expected `(max: N)`")
                .note("there is no fixed-width string type; `string(max: N)` bounds the bytes on the wire (§2, §6.3)")
                .help("write `string(max: 24)`");
            return Err(self.emit(d));
        }
        self.bump(); // `(`
        if !self.at_word("max") {
            let span = self.span();
            let found = self.peek().describe();
            let d = self
                .diag(span, format!("expected `max` in `string(...)`, found {found}"), "expected `max`")
                .help("write `string(max: 24)`");
            return Err(self.emit(d));
        }
        self.bump(); // `max`
        self.expect(Punct::Colon, "`max`")?;
        let bound = self.parse_positive_bound("max")?;
        self.eat_punct(Punct::Comma);
        self.expect(Punct::RParen, "the `max` bound")?;
        Ok(bound)
    }

    /// A `max:` bound: a positive integer that fits in 64 bits (§11).
    fn parse_positive_bound(&mut self, what: &str) -> PResult<Spanned<u64>> {
        let value = self.expect_int(&format!("a `{what}` bound"))?;
        if value.value == 0 {
            let d = self
                .diag(value.span, format!("`{what}` must be a positive integer"), "zero is not allowed")
                .note("a variable-length field with a maximum of zero could never hold anything (§11)");
            return Err(self.emit(d));
        }
        u64::try_from(value.value).map(|v| Spanned::new(v, value.span)).map_err(|_| {
            let d = self.diag(value.span, format!("`{what}` bound is too large"), "does not fit in 64 bits");
            self.emit(d)
        })
    }

    /// A width written as `uN`, range-checked against `max`.
    ///
    /// Two different limits meet here. Where the bits become a *value* — an
    /// enum's backing type, a discriminant, a `reserved` field — the limit is
    /// [`MAX_INT_BITS`], because the value has to land in a native integer.
    /// Where they are only a *count* — a container's width, a run of
    /// `padding` — the limit is [`MAX_CONTAINER_BITS`], since nothing ever
    /// holds those bits as one number (§2, §6, §6.2).
    fn parse_unsigned_width(&mut self, what: &str, max: u32) -> PResult<Spanned<u32>> {
        if let TokKind::Int(v) = self.peek().clone() {
            let span = self.span();
            let d = self
                .diag(
                    span,
                    format!("{what} is written as a type, not a number"),
                    "expected a type such as `u16`",
                )
                .help(format!("write `u{v}`"));
            return Err(self.emit(d));
        }
        // A `uN` wider than a primitive is still a legal bit count, so widths
        // above `MAX_INT_BITS` are read here rather than by `parse_scalar_type`,
        // which would reject them as integer types.
        if max > MAX_INT_BITS
            && let TokKind::Ident(name) = self.peek().clone()
            && let Some(digits) = name.strip_prefix('u')
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
        {
            let span = self.span();
            let Some(bits) = digits.parse::<u32>().ok().filter(|b| (1..=max).contains(b)) else {
                let d = self
                    .diag(span, format!("{what} cannot be `{name}`"), format!("width must be between 1 and {max}"))
                    .note(format!(
                        "{what} is a bit count rather than a value, so it may go past `u{MAX_INT_BITS}` — but only up to {max} bits ({} bytes), the largest value ATT can carry (§6)",
                        max / 8
                    ));
                return Err(self.emit(d));
            };
            self.bump();
            return Ok(Spanned::new(bits, span));
        }
        let ty = self.parse_scalar_type()?;
        match ty.kind {
            ScalarKind::UInt(bits) => Ok(Spanned::new(bits, ty.span)),
            ScalarKind::Int(bits) => {
                let d = self
                    .diag(ty.span, format!("{what} must be unsigned, found `i{bits}`"), format!("expected `u{bits}`"))
                    .note("a container's width is a bit count, and padding has no sign; signedness belongs to individual fields (§6)");
                Err(self.emit(d))
            }
            ScalarKind::Bool => {
                let d = self
                    .diag(
                        ty.span,
                        format!("{what} must be an unsigned integer type, found `bool`"),
                        "expected `uN`",
                    )
                    .help("use `u1` to mean a single bit");
                Err(self.emit(d))
            }
            ScalarKind::Named(name) => {
                let d = self
                    .diag(
                        ty.span,
                        format!("{what} must be an unsigned integer type, found `{}`", name.name),
                        "expected a type such as `u16`",
                    )
                    .note("a container's width is spelled out literally so the layout can be checked at compile time (§6)");
                Err(self.emit(d))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Enums and tagged unions (§5, §7)
// ---------------------------------------------------------------------------

impl<'a> Parser<'a> {
    fn parse_enum_or_union(&mut self, docs: Docs, attrs: Vec<Attr>) -> PResult<Decl> {
        let kw_span = self.bump().span; // `enum`
        let name = self.expect_ident("an enum name")?;
        // `enum Name(tag: uT): uN` is a tagged union; `enum Name: uN` is a
        // plain enum.
        if self.at_punct(Punct::LParen) {
            Ok(Decl::Union(self.parse_union(docs, attrs, kw_span, name)?))
        } else {
            self.reject_attrs(&attrs, "a plain `enum`");
            Ok(Decl::Enum(self.parse_plain_enum(docs, kw_span, name)?))
        }
    }

    fn parse_plain_enum(&mut self, docs: Docs, kw_span: Span, name: Ident) -> PResult<EnumDecl> {
        self.ctx.push(Ctx { what: format!("enum `{}`", name.name), span: name.span });
        self.expect(Punct::Colon, "the enum name")?;
        let backing_bits = self.parse_unsigned_width("an enum's backing type", MAX_INT_BITS)?;

        let (items, body_span) = self.delimited(
            Punct::LBrace,
            Punct::RBrace,
            Sep::Required,
            "the enum's backing type",
            Self::parse_enum_item,
        )?;

        let (variants, else_arm) = self.split_else_arms(items, "enum");
        self.ctx.pop();
        Ok(EnumDecl { docs, name, backing_bits, variants, else_arm, span: kw_span.to(body_span) })
    }

    fn parse_enum_item(&mut self) -> PResult<EnumItem<EnumVariant>> {
        let docs = self.collect_docs();
        if self.at_kw(Kw::Else) {
            return Ok(EnumItem::Else(self.parse_else_arm(docs)?));
        }

        let name = self.expect_ident("a variant name")?;

        // `Name(0)` — tagged-union syntax used in a plain enum.
        if self.at_punct(Punct::LParen) {
            let span = self.span();
            let d = self
                .diag(span, "a plain enum variant takes its value with `=`", "expected `=` or `,`")
                .note("`Name(id)` is tagged-union syntax; a plain enum's variants are just numbers (§5, §7)")
                .help(format!("write `{} = 0,`", name.name));
            return Err(self.emit(d));
        }
        // `Name { field }` — a payload in a plain enum.
        if self.at_punct(Punct::LBrace) {
            let span = self.span();
            let enum_name = self.ctx.last().map_or("Name".to_string(), |c| c.span.text(self.src).to_string());
            let d = self
                .diag(span, "a plain enum variant cannot carry fields", "unexpected payload")
                .note("only a tagged union's variants have payloads (§7)")
                .help(format!(
                    "to give variants payloads, declare `enum {enum_name}(id: u8): u32 {{ ... }}`"
                ));
            return Err(self.emit(d));
        }

        let value = if self.eat_punct(Punct::Eq) { Some(self.expect_int("an enum value")?) } else { None };
        let span = name.span.to(value.map_or(name.span, |v| v.span));
        Ok(EnumItem::Variant(EnumVariant { docs, name, value, span }))
    }

    fn parse_union(
        &mut self,
        docs: Docs,
        attrs: Vec<Attr>,
        kw_span: Span,
        name: Ident,
    ) -> PResult<UnionDecl> {
        self.ctx.push(Ctx { what: format!("tagged union `{}`", name.name), span: name.span });
        self.bump(); // `(`

        let tag_name = self.expect_ident("a name for the discriminant")?;
        if self.at_punct(Punct::RParen) {
            let span = tag_name.span.to(self.span());
            let d = self
                .diag(span, "the discriminant needs a name and a type", "expected `name: uN`")
                .note("the tag is a real field of the container, so it is named like one (§7)")
                .help(format!("write `enum {}(id: {}): u64 {{ ... }}`", name.name, tag_name.name));
            return Err(self.emit(d));
        }
        self.expect(Punct::Colon, "the discriminant name")?;
        let tag_bits = self.parse_unsigned_width("a discriminant's type", MAX_INT_BITS)?;
        self.expect(Punct::RParen, "the discriminant type")?;

        self.expect(Punct::Colon, "the discriminant declaration")?;
        let container_bits =
            self.parse_unsigned_width("a tagged union's container type", MAX_CONTAINER_BITS)?;

        // Variants are usually newline-separated; a comma is accepted too.
        let (items, body_span) = self.delimited(
            Punct::LBrace,
            Punct::RBrace,
            Sep::Optional,
            "the tagged union's container type",
            Self::parse_union_item,
        )?;

        let (variants, else_arm) = self.split_else_arms(items, "tagged union");
        self.ctx.pop();
        Ok(UnionDecl {
            docs,
            attrs,
            name,
            tag_name,
            tag_bits,
            container_bits,
            variants,
            else_arm,
            span: kw_span.to(body_span),
        })
    }

    fn parse_union_item(&mut self) -> PResult<EnumItem<UnionVariant>> {
        let docs = self.collect_docs();
        if self.at_kw(Kw::Else) {
            return Ok(EnumItem::Else(self.parse_else_arm(docs)?));
        }

        let name = self.expect_ident("a variant name")?;

        // `Variant = 1` — plain-enum syntax used in a union.
        if self.at_punct(Punct::Eq) {
            let span = self.span();
            let d = self
                .diag(span, "a tagged-union variant takes its id in parentheses", "expected `(`")
                .note(
                    "`Name = value` is plain-enum syntax (§5); a union variant is `Name(id) { fields }` (§7)",
                )
                .help(format!("write `{}(0x0001)`", name.name));
            return Err(self.emit(d));
        }
        if !self.at_punct(Punct::LParen) {
            let span = self.span();
            let d = self
                .diag(span, format!("variant `{}` is missing its id", name.name), "expected `(<id>)`")
                .note("every tagged-union variant's id is mandatory and explicit: these are cross-firmware wire contracts, so they are never auto-numbered (§7)")
                .help(format!("write `{}(0x0001)`", name.name));
            return Err(self.emit(d));
        }
        self.bump(); // `(`
        let id = self.expect_int("a variant id")?;
        self.expect(Punct::RParen, "the variant id")?;

        let mut fields = Vec::new();
        let mut has_payload_block = false;
        let mut end = id.span;
        if self.at_punct(Punct::LBrace) {
            self.ctx.push(Ctx { what: format!("variant `{}`", name.name), span: name.span });
            let (parsed, body_span) = self.delimited(
                Punct::LBrace,
                Punct::RBrace,
                Sep::Required,
                "the variant id",
                Self::parse_field,
            )?;
            self.ctx.pop();
            fields = parsed;
            has_payload_block = true;
            end = body_span;
        }

        Ok(EnumItem::Variant(UnionVariant {
            docs,
            name: name.clone(),
            id,
            fields,
            has_payload_block,
            span: name.span.to(end),
        }))
    }

    fn parse_else_arm(&mut self, docs: Docs) -> PResult<ElseArm> {
        let kw_span = self.bump().span; // `else`
        if self.at_punct(Punct::Comma) || self.at_punct(Punct::RBrace) {
            let d = self
                .diag(kw_span, "the `else` arm needs a variant name", "expected a name after `else`")
                .note("the fallback is a real variant that carries the unrecognized wire value (§5, §7)")
                .help("write `else Unknown`");
            return Err(self.emit(d));
        }
        let name = self.expect_ident("a name for the fallback variant")?;
        Ok(ElseArm { docs, span: kw_span.to(name.span), name })
    }

    /// Enforces "at most one `else`, and it must come last" (§5, §7).
    fn split_else_arms<T: HasSpan>(
        &mut self,
        items: Vec<EnumItem<T>>,
        what: &str,
    ) -> (Vec<T>, Option<ElseArm>) {
        let mut variants = Vec::new();
        let mut else_arm: Option<ElseArm> = None;
        for item in items {
            match item {
                EnumItem::Variant(v) => {
                    if let Some(existing) = &else_arm {
                        let d = Diagnostic::error("the `else` arm must be the last arm")
                            .primary(v.span(), format!("this variant comes after the `else` arm of the {what}"))
                            .secondary(existing.span, "`else` arm declared here")
                            .note("`else` is the fallback for every unmatched value, so nothing can follow it (§5, §7)")
                            .help("move the `else` arm to the end of the body");
                        self.emit(d);
                    }
                    variants.push(v);
                }
                EnumItem::Else(arm) => match &else_arm {
                    Some(existing) => {
                        let d = Diagnostic::error(format!("a {what} may have only one `else` arm"))
                            .primary(arm.span, "second `else` arm")
                            .secondary(existing.span, "first one here")
                            .note("one fallback covers every unmatched value (§5, §7)");
                        self.emit(d);
                    }
                    None => else_arm = Some(arm),
                },
            }
        }
        (variants, else_arm)
    }
}

enum EnumItem<T> {
    Variant(T),
    Else(ElseArm),
}

/// Lets [`Parser::split_else_arms`] report a span for either variant flavour.
trait HasSpan {
    fn span(&self) -> Span;
}

impl HasSpan for EnumVariant {
    fn span(&self) -> Span {
        self.span
    }
}

impl HasSpan for UnionVariant {
    fn span(&self) -> Span {
        self.span
    }
}

// ---------------------------------------------------------------------------
// Structs and fields (§6, §6.2)
// ---------------------------------------------------------------------------

impl<'a> Parser<'a> {
    fn parse_struct(&mut self, docs: Docs, attrs: Vec<Attr>) -> PResult<StructDecl> {
        let kw_span = self.bump().span; // `struct`
        let name = self.expect_ident("a struct name")?;
        self.ctx.push(Ctx { what: format!("struct `{}`", name.name), span: name.span });

        // `: uN` makes the struct fixed-width; omitting it declares a
        // variable-length struct (§6.3).
        let width_bits = if self.eat_punct(Punct::Colon) {
            Some(self.parse_unsigned_width("a struct's width", MAX_CONTAINER_BITS)?)
        } else {
            if !self.at_punct(Punct::LBrace) {
                let span = self.span();
                let found = self.peek().describe();
                let d = self
                    .diag(span, format!("expected `:` or `{{` after the struct name, found {found}"), "expected `: uN` or `{`")
                    .note("a struct either declares an exact width (`struct S: u32 { ... }`) or omits it and ends in one variable-length field (§6, §6.3)");
                return Err(self.emit(d));
            }
            None
        };

        let (fields, body_span) = self.delimited(
            Punct::LBrace,
            Punct::RBrace,
            Sep::Required,
            if width_bits.is_some() { "the struct's width" } else { "the struct name" },
            Self::parse_field,
        )?;

        self.ctx.pop();
        Ok(StructDecl { docs, attrs, name, width_bits, fields, span: kw_span.to(body_span) })
    }

    fn parse_field(&mut self) -> PResult<Field> {
        let docs = self.collect_docs();

        // Attributes belong on declarations, not fields (§1.2).
        if self.at_punct(Punct::Hash) {
            let start = self.span();
            let attrs = self.parse_attrs()?;
            let span = attrs.first().map_or(start, |a| a.span);
            let d = Diagnostic::error("attributes cannot be applied to a field")
                .primary(span, "not allowed here")
                .note("byte order is a property of a whole root container, never of one field; there is no per-field byte-swap override (§8, §14)");
            self.emit(d);
        }

        match self.peek().clone() {
            TokKind::Kw(Kw::Padding) => self.parse_padding_field(docs),
            TokKind::Kw(Kw::Reserved) => self.parse_reserved_field(docs),
            TokKind::Ident(_) => {
                let name = self.expect_ident("a field name")?;
                self.expect(Punct::Colon, format!("field name `{}`", name.name).as_str())?;
                let ty = self.parse_field_type()?;
                let span = name.span.to(ty.span);
                Ok(Field { docs, kind: FieldKind::Value { name, ty }, span })
            }
            TokKind::Kw(kw) => {
                let span = self.span();
                let d = self
                    .diag(
                        span,
                        format!("expected a field name, found keyword `{}`", kw.as_str()),
                        format!("`{}` is a reserved word", kw.as_str()),
                    )
                    .help(format!("rename the field, or remove `{}`", kw.as_str()));
                Err(self.emit(d))
            }
            _ => Err(self.expected("a field name")),
        }
    }

    fn parse_padding_field(&mut self, docs: Docs) -> PResult<Field> {
        let keyword = self.bump().span; // `padding`
        self.expect(Punct::Colon, "`padding`")?;
        let bits = self.parse_unsigned_width("a padding width", MAX_CONTAINER_BITS)?;
        let mut end = bits.span;
        let mut check_zero = false;

        if self.at_punct(Punct::Eq) {
            self.bump(); // `=`
            let value = self.expect_int("a padding value")?;
            end = value.span;
            if value.value != 0 {
                let d = self
                    .diag(value.span, "padding can only be checked against `0`", "expected `0`")
                    .note("bare `padding: uN` ignores those bits on decode; `padding: uN = 0` additionally requires them to be zero. No other value can be asserted (§6.2)")
                    .help("write `= 0`, or drop the `= ...` to ignore the bits");
                return Err(self.emit(d));
            }
            check_zero = true;
        }

        let span = keyword.to(end);
        Ok(Field { docs, kind: FieldKind::Padding { keyword, bits, check_zero }, span })
    }

    fn parse_reserved_field(&mut self, docs: Docs) -> PResult<Field> {
        let kw_span = self.bump().span; // `reserved`
        if self.at_punct(Punct::Colon) {
            let span = self.span();
            let d = self
                .diag(span, "a `reserved` field must be named", "expected a field name before `:`")
                .note("unnamed bits are `padding`; `reserved` bits are exposed read-only and written back unchanged, so they need a name (§6.2)")
                .help("write `reserved flags: u4`, or `padding: u4` if the bits need not round-trip");
            return Err(self.emit(d));
        }
        let name = self.expect_ident("a name for the reserved field")?;
        self.expect(Punct::Colon, format!("reserved field `{}`", name.name).as_str())?;
        let bits = self.parse_unsigned_width("a reserved field's width", MAX_INT_BITS)?;
        let span = kw_span.to(bits.span);
        Ok(Field { docs, kind: FieldKind::Reserved { name, bits }, span })
    }
}

// ---------------------------------------------------------------------------
// Services and characteristics (§10)
// ---------------------------------------------------------------------------

impl<'a> Parser<'a> {
    fn parse_service(&mut self, docs: Docs) -> PResult<ServiceDecl> {
        let kw_span = self.bump().span; // `service`
        let name = self.expect_ident("a service name")?;
        self.ctx.push(Ctx { what: format!("service `{}`", name.name), span: name.span });

        if !self.at_punct(Punct::LParen) {
            let span = self.span();
            let d = self
                .diag(span, "a `service` needs a UUID", "expected `(uuid: \"...\")`")
                .note("a service groups characteristic bindings under a GATT service UUID (§10)")
                .help(format!(
                    "write `service {}(uuid: \"0000180a-0000-1000-8000-00805f9b34fb\")`",
                    name.name
                ));
            return Err(self.emit(d));
        }
        self.bump(); // `(`
        let uuid_arg = self.expect_ident("`uuid`")?;
        if uuid_arg.name != "uuid" {
            let mut d = self
                .diag(
                    uuid_arg.span,
                    format!("unknown service argument `{}`", uuid_arg.name),
                    "expected `uuid`",
                )
                .note("a `service` takes only a UUID; properties belong to its characteristics (§10)");
            if let Some(s) = suggest(&uuid_arg.name, &["uuid"]) {
                d = d.help(format!("did you mean `{s}`?"));
            }
            return Err(self.emit(d));
        }
        self.expect(Punct::Colon, "`uuid`")?;
        let uuid = self.expect_string("a service UUID")?;
        self.eat_punct(Punct::Comma);
        self.expect(Punct::RParen, "the service UUID")?;

        let (characteristics, body_span) = self.parse_service_body(&name)?;
        self.ctx.pop();
        Ok(ServiceDecl { docs, name, uuid, characteristics, span: kw_span.to(body_span) })
    }

    /// The service body is a sequence of `;`-terminated characteristic
    /// bindings — no separators, so it gets its own loop.
    fn parse_service_body(&mut self, service: &Ident) -> PResult<(Vec<Characteristic>, Span)> {
        let open = self.expect(Punct::LBrace, "the service UUID")?;
        let mut characteristics = Vec::new();
        loop {
            if self.at_punct(Punct::RBrace) {
                return Ok((characteristics, open.to(self.bump().span)));
            }
            if self.at_eof() {
                return Err(self.unclosed(Punct::LBrace, open, Punct::RBrace));
            }

            let docs = self.collect_docs();
            if !self.at_kw(Kw::Characteristic) {
                let span = self.span();
                let found = self.peek().describe();
                let mut d = self.diag(
                    span,
                    format!("expected `characteristic` or `}}`, found {found}"),
                    "expected `characteristic`",
                );
                d = d.note(format!(
                    "a service body contains only characteristic bindings (§10); `{}` ends at its `}}`",
                    service.name
                ));
                if let TokKind::Ident(word) = self.peek().clone()
                    && let Some(s) = suggest(&word, &["characteristic"])
                {
                    d = d.help(format!("did you mean `{s}`?"));
                }
                self.emit(d);
                match self.recover_in_list(Punct::RBrace) {
                    Recover::NextItem => continue,
                    Recover::Close if self.at_punct(Punct::RBrace) => {
                        return Ok((characteristics, open.to(self.bump().span)));
                    }
                    _ => return Err(self.unclosed(Punct::LBrace, open, Punct::RBrace)),
                }
            }

            match self.parse_characteristic(docs) {
                Ok(c) => characteristics.push(c),
                Err(Bail) => match self.recover_in_list(Punct::RBrace) {
                    Recover::NextItem => continue,
                    Recover::Close if self.at_punct(Punct::RBrace) => {
                        return Ok((characteristics, open.to(self.bump().span)));
                    }
                    _ => return Err(self.unclosed(Punct::LBrace, open, Punct::RBrace)),
                },
            }
        }
    }

    fn parse_characteristic(&mut self, docs: Docs) -> PResult<Characteristic> {
        let kw_span = self.bump().span; // `characteristic`
        let name = self.expect_ident("a characteristic name")?;
        let outer_ctx = self.ctx.len();
        self.ctx.push(Ctx { what: format!("characteristic `{}`", name.name), span: name.span });

        let mut uuid: Option<Spanned<String>> = None;
        let mut properties: Option<(Vec<Spanned<Property>>, Span)> = None;
        let (args, args_span) = self.delimited(
            Punct::LParen,
            Punct::RParen,
            Sep::Required,
            "the characteristic name",
            Self::parse_characteristic_arg,
        )?;

        for arg in args {
            match arg {
                CharArg::Uuid(value) => match &uuid {
                    Some(first) => {
                        let d = Diagnostic::error("duplicate `uuid` argument")
                            .primary(value.span, "second `uuid`")
                            .secondary(first.span, "first one here");
                        self.emit(d);
                    }
                    None => uuid = Some(value),
                },
                CharArg::Properties(list, span) => match &properties {
                    Some((_, first)) => {
                        let d = Diagnostic::error("duplicate `properties` argument")
                            .primary(span, "second `properties`")
                            .secondary(*first, "first one here");
                        self.emit(d);
                    }
                    None => properties = Some((list, span)),
                },
            }
        }

        let uuid = match uuid {
            Some(u) => u,
            None => {
                let d = self
                    .diag(
                        args_span,
                        format!("characteristic `{}` is missing its `uuid`", name.name),
                        "expected a `uuid: \"...\"` argument",
                    )
                    .note("a characteristic binding is UUID + properties + value type (§10)")
                    .help("add `uuid: \"7d8f0001-3c1a-4e8a-9b5a-000000000000\"`");
                return Err(self.emit(d));
            }
        };
        let properties = match properties {
            Some((list, span)) => {
                if list.is_empty() {
                    let d = self
                        .diag(
                            span,
                            format!("characteristic `{}` declares no properties", name.name),
                            "expected at least one property",
                        )
                        .note("without a property, nothing can read, write or subscribe to the value (§10)")
                        .help("list what the peripheral supports, e.g. `properties: [read, notify]`");
                    return Err(self.emit(d));
                }
                list
            }
            None => {
                let d = self
                    .diag(
                        args_span,
                        format!("characteristic `{}` is missing its `properties`", name.name),
                        "expected a `properties: [...]` argument",
                    )
                    .note("a characteristic binding is UUID + properties + value type (§10)")
                    .help("add `properties: [read, notify]`");
                return Err(self.emit(d));
            }
        };

        self.expect(Punct::Colon, "the characteristic's arguments")?;
        let ty = self.parse_characteristic_type(&name)?;
        let semi = self.expect(Punct::Semi, "the characteristic's value type")?;

        self.ctx.truncate(outer_ctx);
        Ok(Characteristic { docs, name, uuid, properties, ty, span: kw_span.to(semi) })
    }

    /// A characteristic binds a *declared* type by name (§10); primitives and
    /// inline compound types have to go through an `alias`.
    fn parse_characteristic_type(&mut self, name: &Ident) -> PResult<Ident> {
        let ty = self.parse_field_type()?;
        match ty.kind {
            FieldTypeKind::Scalar(ScalarType { kind: ScalarKind::Named(ident), .. }) => Ok(ident),
            FieldTypeKind::Scalar(_) => {
                let text = ty.span.text(self.src).to_string();
                let d = self
                    .diag(
                        ty.span,
                        format!("characteristic `{}` cannot bind the primitive type `{text}`", name.name),
                        "expected a declared type name",
                    )
                    .note("a characteristic binds a `struct`, tagged-union `enum`, or `alias` (§10)")
                    .help(format!("declare `alias {}Value = {text};` and bind that", name.name));
                Err(self.emit(d))
            }
            _ => {
                let text = ty.span.text(self.src).to_string();
                let d = self
                    .diag(
                        ty.span,
                        "a characteristic cannot bind an inline compound type",
                        "expected a declared type name",
                    )
                    .note("a characteristic binds a `struct`, tagged-union `enum`, or `alias` (§10)")
                    .help(format!("declare `alias {}Value = {text};` and bind that", name.name));
                Err(self.emit(d))
            }
        }
    }

    fn parse_characteristic_arg(&mut self) -> PResult<CharArg> {
        let arg = self.expect_ident("`uuid` or `properties`")?;
        self.expect(Punct::Colon, format!("`{}`", arg.name).as_str())?;
        match arg.name.as_str() {
            "uuid" => Ok(CharArg::Uuid(self.expect_string("a characteristic UUID")?)),
            "properties" => {
                let (props, span) = self.delimited(
                    Punct::LBracket,
                    Punct::RBracket,
                    Sep::Required,
                    "`properties:`",
                    Self::parse_property,
                )?;
                Ok(CharArg::Properties(props, arg.span.to(span)))
            }
            other => {
                let mut d = self
                    .diag(
                        arg.span,
                        format!("unknown characteristic argument `{other}`"),
                        "expected `uuid` or `properties`",
                    )
                    .note("v1 has no descriptors, permissions or security metadata in the schema (§14)");
                if let Some(s) = suggest(other, &["uuid", "properties"]) {
                    d = d.help(format!("did you mean `{s}`?"));
                }
                Err(self.emit(d))
            }
        }
    }

    fn parse_property(&mut self) -> PResult<Spanned<Property>> {
        let word = self.expect_ident("a GATT property")?;
        match Property::from_name(&word.name) {
            Some(p) => Ok(Spanned::new(p, word.span)),
            None => {
                let all: Vec<&str> = Property::ALL.iter().map(|p| p.as_str()).collect();
                let mut d = self
                    .diag(word.span, format!("unknown GATT property `{}`", word.name), "not a GATT property")
                    .note(format!("the standard set is {} (§10)", all.join(", ")));
                if let Some(s) = suggest(&word.name, &all) {
                    d = d.help(format!("did you mean `{s}`?"));
                }
                Err(self.emit(d))
            }
        }
    }

    fn expect_string(&mut self, what: &str) -> PResult<Spanned<String>> {
        match self.peek().clone() {
            TokKind::Str(value) => {
                let span = self.bump().span;
                Ok(Spanned::new(value, span))
            }
            _ => Err(self.expected(&format!("{what} (a quoted string)"))),
        }
    }
}

enum CharArg {
    Uuid(Spanned<String>),
    Properties(Vec<Spanned<Property>>, Span),
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn join_or(options: &[&str]) -> String {
    match options {
        [] => "something else".to_string(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}
