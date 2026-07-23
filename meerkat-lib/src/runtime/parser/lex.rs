// L1 Compiler
//! Lexer
// Author: Miles Conn <mconn@andrew.cmu.edu>

// Update this file to lex the necessary keywords and other tokens
// in order to make the grammar forward compatible with C0.
// Note this project relies on logos 0.12.1 see docs [here]
// (https://docs.rs/logos/0.12.1/logos/index.html)

#![allow(clippy::upper_case_acronyms)]
use enum_as_inner::EnumAsInner;
use logos::{Lexer, Logos, Skip};
use std::fmt;
use strum_macros::AsRefStr;

fn from_num<'b>(lex: &mut Lexer<'b, Token<'b>>) -> Result<i32, String> {
    let slice = lex.slice();

    let out: i64 = match slice.parse() {
        Ok(val) => val,
        Err(e) => return Err(format!("Parsing failed with Error {:?}", e)),
    };
    if out > i32::MAX as i64 {
        // All numbers are positive because - is lexed separately
        return Err(format!("Number {} is out of bounds", out));
    }

    Ok(out as i32) // returning i32 since numbers are defined as i32
}

fn skip_multi_line_comments<'b>(lex: &mut Lexer<'b, Token<'b>>) -> Skip {
    use logos::internal::LexerInternal;
    let mut balanced_comments: isize = 1;
    if lex.slice() == "/*" {
        loop {
            // Read the current value
            let x: Option<u8> = lex.read();
            match x {
                // Some(0) => panic!("Reached end of file or not?"),
                Some(b'*') => {
                    lex.bump_unchecked(1);
                    if let Some(b'/') = lex.read() {
                        lex.bump_unchecked(1);
                        balanced_comments -= 1;
                        if balanced_comments == 0 {
                            // No more comments
                            break;
                        }
                    }
                }
                Some(b'/') => {
                    lex.bump(1);
                    if let Some(b'*') = lex.read() {
                        lex.bump_unchecked(1);
                        // We just started a new comment
                        balanced_comments += 1;
                    }
                }
                None => break,
                _ => {
                    lex.bump_unchecked(1);
                }
            }
        }
    }
    Skip
}

// #39: Lex an html literal of the form `( <...> )`. The opening `(` and the
// following `<` have already been consumed by the token regex. Starting with
// paren depth 1 (for that `(`), scan forward until the matching `)` brings the
// depth back to 0, then return the inner slice (between the outer parens).
// Balanced parens inside the literal (e.g. in a `{ f(x) }` interpolation) do
// not terminate it; only the final unbalanced `)` does. Known limitation: a
// literal `)` inside a string inside an interpolation is not accounted for and
// would terminate the literal early; the current examples do not hit this.
fn lex_html_literal<'b>(lex: &mut Lexer<'b, Token<'b>>) -> Option<&'b str> {
    // On entry the current token is `(` + optional whitespace + `<`, so paren
    // depth is 1 for that opening `(`. `remainder()` is the source after the
    // `<`; scan it for the matching `)`, counting balanced parens (so parens
    // inside a `{ f(x) }` interpolation do not terminate the literal).
    let rem = lex.remainder();
    let mut depth: isize = 1;
    for (consumed, &byte) in rem.as_bytes().iter().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    // Extend the token through the closing `)` (a byte count;
                    // `)` is ASCII so this lands on a char boundary).
                    lex.bump(consumed + 1);
                    // The token slice is now `( ... )`; the body is the slice
                    // with the outer parens stripped.
                    let full = lex.slice();
                    return Some(&full[1..full.len() - 1]);
                }
            }
            _ => {}
        }
    }
    None // unterminated html literal
}

impl<'a> fmt::Display for Token<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#?}", self)
    }
}

#[allow(non_camel_case_types)]
#[derive(Clone, Logos, Debug, PartialEq, AsRefStr, EnumAsInner)]
#[logos(subpattern identifier = r"[A-Za-z_][A-Za-z0-9_]*")]
pub enum Token<'a> {
    #[regex(r#""[^"]*""#, |lex| lex.slice().trim_matches('"'))] // regex for string within ""
    StrLit(&'a str),
    // #39: html literal triggered by `(` + optional whitespace + `<`. Longer
    // than the bare `(` token, so logos prefers it only when a literal begins.
    #[regex(r"\(\s*<", lex_html_literal)]
    HtmlLit(&'a str),
    #[regex(r"(?&identifier)")]
    Ident(&'a str),

    #[regex(r"0|[1-9][0-9]*", from_num)]
    Number(i32),

    #[token("true")]
    TRUE,
    #[token("false")]
    FALSE,

    //Operators
    #[token("-")]
    Minus,
    #[token("+")]
    Plus,
    #[token("*")]
    Asterisk,
    #[token("/")]
    Div,
    #[token("=")]
    Assgn,
    #[token("=>")]
    Fn_Assgn,
    #[token("->")]
    Arrow,
    #[token("==")]
    EQ_EQ,
    #[token("<")]
    LT,
    #[token(">")]
    GT,
    #[token("&&")]
    AND_AND,
    #[token("||")]
    OR_OR,
    #[token("!")]
    NOT_NOT,

    //Punctuation
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token("[")]
    LSquare,
    #[token("]")]
    RSquare,
    #[token(";")]
    Semicolon,
    #[token(",")]
    Comma,
    #[token(":")]
    Colon,
    #[token(".")]
    Dot,

    // Reserved Keywords
    #[token("update")]
    UPDATE_KW,
    #[token("service")]
    SERVICE,
    #[token("@test")]
    TEST_KW,
    #[token("do")]
    DO_KW,
    #[token("assert")]
    ASSERT_KW,
    #[token("import")]
    IMPORT_KW,
    #[token("var")]
    VAR_KW,
    #[token("pub")]
    PUB_KW,
    #[token("def")]
    DEF_KW,
    #[token("table")]
    TABLE_KW,
    #[token("insert")]
    INSERT_KW,
    #[token("select")]
    SELECT_KW,
    #[token("from")]
    FROM_KW,
    #[token("where")]
    WHERE_KW,
    #[token("into")]
    INTO_KW,
    #[token("fold")]
    FOLD_KW,
    #[token("action")]
    ACTION_KW,
    #[token("fn")]
    FN_KW,
    #[token("then")]
    THEN_KW,
    #[token("if")]
    IF_KW,
    #[token("else")]
    ELSE_KW,
    #[token("string")]
    STRING_KW,
    #[token("bool")]
    BOOL_KW,
    #[token("let")]
    LET_KW,
    #[token("int")]
    INT_KW,
    #[token("unit")]
    UNIT_KW,
    #[token("watch")]
    WATCH_KW,
    #[token("list")]
    LIST_KW,
    #[token("for")]
    FOR_KW,
    #[token("in")]
    IN_KW,
    #[token("..")]
    DotDot,

    #[regex(r"\s*", logos::skip)]
    #[regex(r#"(//)[^\n]*"#, logos::skip)] // Regex for a single line comment
    // Yes there is regex for this no I could not get it to work
    #[token("/*", skip_multi_line_comments)] // Match start of multiline
    Comment,

    #[error]
    #[regex(r#"[^\x00-\x7F]"#)] // Error on non ascii characters
    Error,
}

#[cfg(test)]
mod html_lex_tests {
    use super::*;
    use logos::Logos;

    // #39: collect the tokens (and captured slices) for a source string.
    fn lex_all(src: &str) -> Vec<Token<'_>> {
        Token::lexer(src).collect::<Vec<_>>()
    }

    #[test]
    fn test_html_literal_basic() {
        let toks = lex_all("(<p>hello</p>)");
        assert_eq!(
            toks.len(),
            1,
            "expected a single HtmlLit token, got {:?}",
            toks
        );
        match &toks[0] {
            Token::HtmlLit(inner) => assert_eq!(*inner, "<p>hello</p>"),
            other => panic!("expected HtmlLit, got {:?}", other),
        }
    }

    #[test]
    fn test_html_literal_with_interpolation() {
        let toks = lex_all("(<p>The count is {count}.</p>)");
        assert_eq!(
            toks.len(),
            1,
            "expected a single HtmlLit token, got {:?}",
            toks
        );
        match &toks[0] {
            Token::HtmlLit(inner) => assert_eq!(*inner, "<p>The count is {count}.</p>"),
            other => panic!("expected HtmlLit, got {:?}", other),
        }
    }

    #[test]
    fn test_html_literal_ignores_balanced_parens_in_interpolation() {
        let toks = lex_all("(<p>{f(x)}</p>)");
        assert_eq!(
            toks.len(),
            1,
            "balanced parens inside must not end the literal: {:?}",
            toks
        );
        match &toks[0] {
            Token::HtmlLit(inner) => assert_eq!(*inner, "<p>{f(x)}</p>"),
            other => panic!("expected HtmlLit, got {:?}", other),
        }
    }

    #[test]
    fn test_html_literal_with_leading_whitespace() {
        // `(` then whitespace then `<` still triggers the literal.
        let toks = lex_all("(  <p>x</p>)");
        assert_eq!(
            toks.len(),
            1,
            "expected a single HtmlLit token, got {:?}",
            toks
        );
        match &toks[0] {
            Token::HtmlLit(inner) => assert_eq!(*inner, "  <p>x</p>"),
            other => panic!("expected HtmlLit, got {:?}", other),
        }
    }

    #[test]
    fn test_plain_paren_is_not_html() {
        // `(` not followed by `<` must remain an ordinary LParen, not html.
        let toks = lex_all("(1 + 2)");
        assert!(
            matches!(toks.first(), Some(Token::LParen)),
            "plain paren must lex as LParen, got {:?}",
            toks
        );
        assert!(
            !toks.iter().any(|t| matches!(t, Token::HtmlLit(_))),
            "no HtmlLit expected in a plain parenthesized expression: {:?}",
            toks
        );
    }
}
