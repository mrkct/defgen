//! Diagnostics: what the compiler reports, and how it gets printed.
//!
//! The front end never formats error text itself — it builds [`Diagnostic`]
//! values (message + labelled spans + notes) and lets this module render them.
//! Rendering goes through `ariadne`, so a diagnostic looks like:
//!
//! ```text
//! error: expected `,` or `}` after struct field
//!    ╭─[ status.defs:6:5 ]
//!    │
//!  5 │     volume: u4
//!    │               ─ help: add a `,` here
//!  6 │     mode: u4,
//!    │     ──┬─
//!    │       ╰─── expected `,` or `}`, found identifier `mode`
//!    │
//!    │ Note: in struct `Status`
//! ───╯
//! ```

use std::io::Write;

use ariadne::{Color, Config, IndexType, Label as ALabel, Report, ReportKind};

use crate::span::{Span, line_col};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// A span called out in a diagnostic. Exactly one label is the primary one —
/// the place the compiler wants the reader to look first.
#[derive(Debug, Clone)]
pub struct Label {
    pub span: Span,
    pub message: String,
    pub primary: bool,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub labels: Vec<Label>,
    /// Background: why this rule exists, what the spec says.
    pub notes: Vec<String>,
    /// Actionable: what to write instead.
    pub helps: Vec<String>,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Error,
            message: message.into(),
            labels: Vec::new(),
            notes: Vec::new(),
            helps: Vec::new(),
        }
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Diagnostic { severity: Severity::Warning, ..Diagnostic::error(message) }
    }

    /// The span the error is *at*. Every diagnostic should have one.
    pub fn primary(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push(Label { span, message: message.into(), primary: true });
        self
    }

    /// A span that gives the primary one context: the declaration being
    /// parsed, the delimiter that was left open, the earlier duplicate.
    pub fn secondary(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push(Label { span, message: message.into(), primary: false });
        self
    }

    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    pub fn help(mut self, help: impl Into<String>) -> Self {
        self.helps.push(help.into());
        self
    }

    /// Where the diagnostic points, for sorting and for plain-text output.
    pub fn span(&self) -> Span {
        self.labels
            .iter()
            .find(|l| l.primary)
            .or_else(|| self.labels.first())
            .map(|l| l.span)
            .unwrap_or(Span::new(0, 0))
    }

    fn to_report(&self, filename: &str, color: bool) -> Report<'static, (String, std::ops::Range<usize>)> {
        let primary_color = match self.severity {
            Severity::Error => Color::Red,
            Severity::Warning => Color::Yellow,
        };
        let kind = match self.severity {
            Severity::Error => ReportKind::Error,
            Severity::Warning => ReportKind::Warning,
        };

        let mut report = Report::build(kind, (filename.to_owned(), self.span().range()))
            .with_config(Config::default().with_color(color).with_index_type(IndexType::Byte))
            .with_message(&self.message);

        for label in &self.labels {
            let mut l = ALabel::new((filename.to_owned(), label.span.range())).with_color(if label.primary {
                primary_color
            } else {
                Color::Cyan
            });
            if !label.message.is_empty() {
                l = l.with_message(&label.message);
            }
            // Primary labels render closest to the source line.
            report = report.with_label(l.with_order(if label.primary { 0 } else { 1 }));
        }
        for note in &self.notes {
            report = report.with_note(note);
        }
        for help in &self.helps {
            report = report.with_help(help);
        }
        report.finish()
    }

    /// Renders to a string, with or without ANSI colour.
    pub fn render(&self, filename: &str, src: &str, color: bool) -> String {
        let mut out: Vec<u8> = Vec::new();
        let cache = ariadne::sources([(filename.to_owned(), src)]);
        if self.to_report(filename, color).write(cache, &mut out).is_err() {
            return self.render_plain(filename, src);
        }
        String::from_utf8(out).unwrap_or_else(|_| self.render_plain(filename, src))
    }

    /// One-line `file:line:col: severity: message` fallback, also handy for
    /// tests and non-tty consumers.
    pub fn render_plain(&self, filename: &str, src: &str) -> String {
        let (line, col) = line_col(src, self.span().start);
        let mut s = format!("{filename}:{line}:{col}: {}: {}", self.severity.as_str(), self.message);
        for label in &self.labels {
            if label.message.is_empty() {
                continue;
            }
            let (l, c) = line_col(src, label.span.start);
            let kind = if label.primary { "" } else { "note: " };
            s.push_str(&format!("\n  {l}:{c}: {kind}{}", label.message));
        }
        for note in &self.notes {
            s.push_str(&format!("\n  note: {note}"));
        }
        for help in &self.helps {
            s.push_str(&format!("\n  help: {help}"));
        }
        s
    }
}

/// Prints diagnostics to stderr, sorted by position, colouring only if stderr
/// is a terminal and `NO_COLOR` is unset.
pub fn emit_all(diagnostics: &[Diagnostic], filename: &str, src: &str) {
    let color = use_color();
    let mut sorted: Vec<&Diagnostic> = diagnostics.iter().collect();
    sorted.sort_by_key(|d| (d.span().start, d.span().end));

    let mut stderr = std::io::stderr().lock();
    for d in sorted {
        let _ = stderr.write_all(d.render(filename, src, color).as_bytes());
    }
    let _ = stderr.flush();
}

fn use_color() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::IsTerminal::is_terminal(&std::io::stderr())
}

/// Closest candidate within a small edit distance, for `did you mean` hints.
pub(crate) fn suggest<'c>(word: &str, candidates: &[&'c str]) -> Option<&'c str> {
    let lower = word.to_ascii_lowercase();
    let budget = (word.len() / 3).max(1);
    candidates
        .iter()
        .map(|c| (edit_distance(&lower, &c.to_ascii_lowercase()), *c))
        .filter(|(d, _)| *d <= budget)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut cur = vec![0; b_chars.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b_chars.len()]
}
