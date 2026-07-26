//! Source locations. Every span is a half-open byte range into the one file
//! currently being compiled.

use std::fmt;
use std::ops::Range;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span { start: start as u32, end: end as u32 }
    }

    /// The smallest span covering both operands.
    pub fn to(self, other: Span) -> Span {
        Span { start: self.start.min(other.start), end: self.end.max(other.end) }
    }

    pub fn range(self) -> Range<usize> {
        self.start as usize..self.end as usize
    }

    pub fn text(self, src: &str) -> &str {
        &src[self.range()]
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

/// A value plus the span of the syntax it came from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Spanned<T> {
    pub value: T,
    pub span: Span,
}

impl<T> Spanned<T> {
    pub fn new(value: T, span: Span) -> Self {
        Spanned { value, span }
    }
}

impl<T: fmt::Debug> fmt::Debug for Spanned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}@{:?}", self.value, self.span)
    }
}

/// Converts a byte offset to a 1-based `(line, column)` pair, counting columns
/// in characters. Used for plain-text error output and tests.
pub fn line_col(src: &str, offset: u32) -> (usize, usize) {
    let offset = (offset as usize).min(src.len());
    let line_start = src[..offset].rfind('\n').map_or(0, |i| i + 1);
    let line = src[..line_start].matches('\n').count() + 1;
    let col = src[line_start..offset].chars().count() + 1;
    (line, col)
}
