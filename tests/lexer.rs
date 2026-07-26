//! Token-level behaviour: comments, literals, and the keyword/identifier split.

use defgen::lexer::{Kw, Punct, TokKind, lex};

fn kinds(src: &str) -> Vec<TokKind> {
    let (tokens, errors) = lex(src);
    assert!(
        errors.is_empty(),
        "unexpected lexical errors: {:?}",
        errors.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    tokens.into_iter().map(|t| t.kind).filter(|k| *k != TokKind::Eof).collect()
}

#[test]
fn line_comments_are_trivia_and_doc_comments_are_tokens() {
    assert_eq!(kinds("// just a comment\n"), vec![]);
    assert_eq!(kinds("/// docs\n"), vec![TokKind::Doc("docs".to_string())]);
    // Only the single space after `///` is stripped; deeper indentation stays.
    assert_eq!(kinds("///   indented\n"), vec![TokKind::Doc("  indented".to_string())]);
    // A `////` rule line is a comment, not documentation.
    assert_eq!(kinds("//// ----\n"), vec![]);
}

#[test]
fn integer_literals_accept_hex_and_underscores() {
    assert_eq!(kinds("42"), vec![TokKind::Int(42)]);
    assert_eq!(kinds("0xffff"), vec![TokKind::Int(0xffff)]);
    assert_eq!(kinds("0x00_01"), vec![TokKind::Int(1)]);
    assert_eq!(kinds("1_000"), vec![TokKind::Int(1000)]);
}

#[test]
fn numbers_distinguish_integers_from_floats() {
    assert_eq!(kinds("0.01"), vec![TokKind::Float(0.01)]);
    assert_eq!(kinds("1e3"), vec![TokKind::Float(1000.0)]);
    assert_eq!(kinds("-0.5"), vec![TokKind::Punct(Punct::Minus), TokKind::Float(0.5)]);
}

#[test]
fn dash_runs_separate_the_separator_from_a_minus_sign() {
    assert_eq!(kinds("---"), vec![TokKind::Punct(Punct::Separator)]);
    assert_eq!(kinds("- 1"), vec![TokKind::Punct(Punct::Minus), TokKind::Int(1)]);
}

#[test]
fn only_reserved_words_lex_as_keywords() {
    assert_eq!(kinds("struct"), vec![TokKind::Kw(Kw::Struct)]);
    assert_eq!(kinds("padding"), vec![TokKind::Kw(Kw::Padding)]);
    // Contextual words stay identifiers, so they remain usable as names.
    for word in
        ["version", "endian", "little", "big", "max", "uuid", "properties", "read", "bool", "f32", "id"]
    {
        assert_eq!(kinds(word), vec![TokKind::Ident(word.to_string())], "`{word}` should not be a keyword");
    }
}

#[test]
fn string_literals_decode_escapes() {
    assert_eq!(kinds(r#""a\"b""#), vec![TokKind::Str("a\"b".to_string())]);
    assert_eq!(kinds(r#""tab\there""#), vec![TokKind::Str("tab\there".to_string())]);
}

#[test]
fn spans_cover_exactly_the_token_text() {
    let src = "struct Status: u64";
    let (tokens, _) = lex(src);
    let texts: Vec<&str> = tokens.iter().take(4).map(|t| t.span.text(src)).collect();
    assert_eq!(texts, vec!["struct", "Status", ":", "u64"]);
}

#[test]
fn lexical_errors_carry_a_span() {
    let (_, errors) = lex("alias A = u8;\nalias B = §;");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("unexpected character"));
    let (line, col) = defgen::span::line_col("alias A = u8;\nalias B = §;", errors[0].span().start);
    assert_eq!((line, col), (2, 11));
}
