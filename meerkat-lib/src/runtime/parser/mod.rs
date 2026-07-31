pub mod lex;

// LALRPOP-generated parser
#[allow(clippy::all)]
#[allow(unused)]
pub mod meerkat {
    include!(concat!(env!("OUT_DIR"), "/runtime/parser/meerkat.rs"));
}

/// Result of attempting to parse a `REPL` input buffer using `ReplParseResult`
pub enum ReplParseResult {
    /// Input parsed successfully into one or more statements
    Complete(Vec<crate::ast::Stmt>),
    /// Input is syntactically incomplete (e.g., an open brace with no matching close)
    ///
    /// The `REPL` should prompt for more input and append it to the buffer
    Incomplete,
    /// Input has a real syntax error that won't be resolved by adding more text
    Error(String),
}

use crate::ast::Stmt;
use crate::runtime::interner::Interner;
use crate::runtime::limits::{MAX_IDENTIFIER_LENGTH, MAX_STRING_LITERAL_LENGTH};
use logos::Logos;

/// Parse a string input into a vector of statements
///
/// Args:
///     `input` (`&str`): The raw string input to parse
///     `interner` (`&mut Interner`): The string interner instance
///
/// Returns:
///     `Result<Vec<Stmt>, String>`: The parsed statements, or an error string
/// #39: Lex `src` into a token stream, enforcing the identifier and string
/// literal length limits. Shared by the top-level parse paths and html
/// interpolation parsing so the checks cannot drift out of sync.
fn build_validated_lex_stream(src: &str) -> Result<Vec<(usize, lex::Token<'_>, usize)>, String> {
    let mut lex_stream = Vec::new();
    for (t, span) in lex::Token::lexer(src).spanned() {
        match t {
            lex::Token::Ident(name) if name.len() > MAX_IDENTIFIER_LENGTH => {
                return Err(format!(
                    "Parse error: identifier exceeds maximum length of {} characters",
                    MAX_IDENTIFIER_LENGTH
                ));
            }
            lex::Token::StrLit(val) if val.len() > MAX_STRING_LITERAL_LENGTH => {
                return Err(format!(
                    "Parse error: string literal exceeds maximum length of {} characters",
                    MAX_STRING_LITERAL_LENGTH
                ));
            }
            _ => {}
        }
        lex_stream.push((span.start, t, span.end));
    }
    Ok(lex_stream)
}

pub fn parse_string(input: &str, interner: &mut Interner) -> Result<Vec<Stmt>, String> {
    let lex_stream = build_validated_lex_stream(input)?;

    meerkat::ProgParser::new()
        .parse(input, interner, lex_stream)
        .map_err(|e| format!("Parse error: {:?}", e))
}

/// Parse a file path into a vector of statements
///
/// Args:
///     `filename` (`&str`): The path of the file to parse
///     `interner` (`&mut Interner`): The string interner instance
///
/// Returns:
///     `Result<Vec<Stmt>, String>`: The parsed statements, or an error string
pub fn parse_file(filename: &str, interner: &mut Interner) -> Result<Vec<Stmt>, String> {
    let content =
        std::fs::read_to_string(filename).map_err(|e| format!("Failed to read file: {}", e))?;
    parse_string(&content, interner)
}

/// Try to parse accumulated `REPL` input, distinguishing incomplete input from real errors
///
/// Returns `Incomplete` when the grammar signals `UnrecognizedEof`, meaning the user
/// is mid-statement and the `REPL` should collect more lines before evaluating
///
/// Args:
///     `input` (`&str`): The accumulated REPL input buffer
///     `interner` (`&mut Interner`): The string interner instance
///
/// Returns:
///     `ReplParseResult`: The parsed result status
pub fn parse_repl(input: &str, interner: &mut Interner) -> ReplParseResult {
    use lalrpop_util::ParseError;

    if input.trim().is_empty() {
        return ReplParseResult::Incomplete;
    }

    let mut lex_stream = Vec::new();
    for (t, span) in lex::Token::lexer(input).spanned() {
        match t {
            lex::Token::Ident(name) if name.len() > MAX_IDENTIFIER_LENGTH => {
                return ReplParseResult::Error(format!(
                    "Parse error: identifier exceeds maximum length of {} characters",
                    MAX_IDENTIFIER_LENGTH
                ));
            }
            lex::Token::StrLit(val) if val.len() > MAX_STRING_LITERAL_LENGTH => {
                return ReplParseResult::Error(format!(
                    "Parse error: string literal exceeds maximum length of {} characters",
                    MAX_STRING_LITERAL_LENGTH
                ));
            }
            _ => {}
        }
        lex_stream.push((span.start, t, span.end));
    }

    let parser = meerkat::ProgParser::new();
    match parser.parse(input, interner, lex_stream) {
        Ok(stmts) => match stmts.first() {
            Some(_) => ReplParseResult::Complete(stmts),
            None => ReplParseResult::Incomplete,
        },
        Err(ParseError::UnrecognizedEof { .. }) => ReplParseResult::Incomplete,
        Err(e) => ReplParseResult::Error(format!("{:?}", e)),
    }
}

/// #39: Parse the raw body of an HTML literal into an `HtmlTemplate`.
///
/// Lives in `runtime::parser` because it interns (via the expression parser),
/// and only `parser`/`net::codec` may write to the append-only interner. The
/// template representation itself is owned by `runtime::html`; this function
/// builds one through the html builder without naming its internal parts.
///
/// Literal text is copied verbatim; each brace-delimited `{ ... }`
/// interpolation is parsed as an expression through the public `Expr` rule.
/// A `{` with no matching `}` is a parse error.
/// #153: if `text` ends with an event-attribute prefix like `onclick=` or
/// `onclick="` (optionally with a quote), return the event name and the byte
/// offset where the attribute begins, so the caller can strip it from the
/// literal text and treat the following `{expr}` as an event handler rather
/// than interpolated content. Only names beginning with `on` qualify.
fn trailing_event_attr(text: &str) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    let mut i = bytes.len();
    // optional opening quote right before the `{`
    if i > 0 && (bytes[i - 1] == b'"' || bytes[i - 1] == b'\'') {
        i -= 1;
    }
    // require an `=`
    if i == 0 || bytes[i - 1] != b'=' {
        return None;
    }
    i -= 1;
    let name_end = i;
    // attribute name: ascii letters/digits/-, scanning left
    while i > 0 {
        let b = bytes[i - 1];
        if b.is_ascii_alphanumeric() || b == b'-' {
            i -= 1;
        } else {
            break;
        }
    }
    let name = &text[i..name_end];
    // the char before the name must be whitespace or `<` (attribute boundary)
    let boundary_ok = i == 0 || {
        let pb = bytes[i - 1];
        pb.is_ascii_whitespace() || pb == b'<'
    };
    if boundary_ok && name.len() > 2 && name.starts_with("on") {
        Some((name.to_string(), i))
    } else {
        None
    }
}

pub fn parse_template(
    raw: &str,
    interner: &mut Interner,
) -> Result<crate::runtime::html::HtmlTemplate, String> {
    let mut builder = crate::runtime::html::HtmlTemplateBuilder::new();
    let mut text = String::new();
    let mut chars = raw.char_indices().peekable();

    while let Some((_, c)) = chars.next() {
        if c == '{' {
            // #153: detect an event-attribute prefix so the following
            // interpolation becomes a handler, not rendered content.
            let handler_event = trailing_event_attr(&text);
            if let Some((_, start)) = &handler_event {
                text.truncate(*start);
            }
            builder.push_text(std::mem::take(&mut text).as_str());
            // #39: brace depth counting does not account for a `}` inside a
            // string literal within the interpolation (e.g. `{ "}" }`); such a
            // brace would desync the counter. This mirrors the analogous
            // paren-depth limitation documented in the html lexer; making both
            // string-aware is tracked as a follow-up.
            let mut depth: usize = 1;
            let mut frag = String::new();
            let mut closed = false;
            for (_, ic) in chars.by_ref() {
                match ic {
                    '{' => {
                        depth += 1;
                        frag.push(ic);
                    }
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            closed = true;
                            break;
                        }
                        frag.push(ic);
                    }
                    _ => frag.push(ic),
                }
            }
            if !closed {
                return Err(
                    "Parse error: unterminated { } interpolation in html literal".to_string(),
                );
            }
            let expr = parse_expr_fragment(frag.trim(), interner)?;
            match handler_event {
                Some((event, _)) => builder.push_handler(event, expr),
                None => builder.push_expr(expr),
            }
        } else {
            text.push(c);
        }
    }
    builder.push_text(text.as_str());
    Ok(builder.build())
}

/// #39: Parse a single interpolation fragment into an `Expr` using the public
/// `Expr` grammar rule, reusing the same lexer and parser as top-level code.
/// The identifier and string-literal length limits enforced by the top-level
/// parser are applied here too, so interpolations cannot bypass those limits.
fn parse_expr_fragment(src: &str, interner: &mut Interner) -> Result<crate::ast::Expr, String> {
    let lex_stream = build_validated_lex_stream(src)?;
    meerkat::ExprParser::new()
        .parse(src, interner, lex_stream)
        .map_err(|e| format!("Parse error in html interpolation: {:?}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::interner::Interner;

    /// Verify that parsing an identifier exceeding the limit
    /// returns an error
    #[test]
    fn test_parse_oversized_identifier() {
        let mut interner = Interner::new();
        let long_ident = "a".repeat(MAX_IDENTIFIER_LENGTH + 1);
        let input = format!("let {} = 42;", long_ident);
        let res = parse_string(&input, &mut interner);
        assert!(res.is_err());
        assert!(res
            .unwrap_err()
            .contains("identifier exceeds maximum length"));
    }

    /// Verify that parsing a string literal exceeding the limit
    /// returns an error
    #[test]
    fn test_parse_oversized_string_literal() {
        let mut interner = Interner::new();
        let long_str = "a".repeat(MAX_STRING_LITERAL_LENGTH + 1);
        let input = format!("let x = \"{}\";", long_str);
        let res = parse_string(&input, &mut interner);
        assert!(res.is_err());
        assert!(res
            .unwrap_err()
            .contains("string literal exceeds maximum length"));
    }
    /// Verify that parsing an assertion captures the correct
    /// raw string
    #[test]
    fn test_parse_assert_captures_string() {
        use crate::ast::{ActionStmt, Stmt};

        let mut interner = Interner::new();
        let input = "assert (x == 5);";
        let res = parse_string(input, &mut interner);
        assert!(res.is_ok());
        let ast = res.unwrap();
        assert_eq!(ast.len(), 1);
        match &ast[0] {
            Stmt::ActionStmt(ActionStmt::Assert(_, text)) => {
                assert_eq!(text, "x == 5");
            }
            _ => panic!("Expected ActionStmt::Assert"),
        }
    }

    /// Verify that parsing an assertion exceeding length
    /// limit fails
    #[test]
    fn test_parse_oversized_assert() {
        let mut interner = Interner::new();
        let limit = MAX_STRING_LITERAL_LENGTH;
        let long_expr = "1+".repeat((limit / 2) + 1) + "1";
        let input = format!("assert ({});", long_expr);
        let res = parse_string(&input, &mut interner);
        assert!(res.is_err());
        assert!(res
            .unwrap_err()
            .contains("Assertion text exceeds maximum length"));
    }

    /// Verify that parsing an `assert` exceeding the length limit
    /// returns a `ParseError::User` error variant
    #[test]
    fn test_parse_oversized_assert_error_type() {
        use lalrpop_util::ParseError;
        let mut interner = Interner::new();
        let limit = MAX_STRING_LITERAL_LENGTH;
        let half_limit = limit / 2;
        let repeat_count = half_limit + 1;
        let repeated = "1+".repeat(repeat_count);
        let long_expr = format!("{}1", repeated);
        let input = format!("assert ({});", long_expr);
        let mut lex_stream = Vec::new();
        for (t, span) in lex::Token::lexer(&input).spanned() {
            lex_stream.push((span.start, t, span.end));
        }
        let parser = meerkat::ProgParser::new();
        let res = parser.parse(&input, &mut interner, lex_stream);
        assert!(matches!(
            res,
            Err(ParseError::User { ref error }) if error.contains("Assertion text exceeds maximum length")
        ));
    }

    /// Verify that parsing a table declaration with unified `int` type
    /// is successful
    #[test]
    fn test_parse_table_definition() {
        use crate::ast::{Decl, Stmt, TableType};

        let mut interner = Interner::new();
        let input = "service test_srv { \
            table test_tbl { \
                id: int, \
                name: string, \
                active: bool, \
            }; \
        }";
        let res = parse_string(input, &mut interner);
        assert!(res.is_ok());
        let ast = res.unwrap();
        assert_eq!(ast.len(), 1);
        if let Stmt::Service { decls, .. } = &ast[0] {
            assert_eq!(decls.len(), 1);
            if let Decl::TableDecl { fields, .. } = &decls[0] {
                assert_eq!(fields.len(), 3);
                assert_eq!(fields[0].ty, TableType::Int);
                assert_eq!(fields[1].ty, TableType::String);
                assert_eq!(fields[2].ty, TableType::Bool);
            } else {
                panic!("Expected TableDecl");
            }
        } else {
            panic!("Expected Service Stmt");
        }
    }

    /// #39: parse_template splits literal text and interpolations, exposing the
    /// embedded expression through the template interface.
    #[test]
    fn test_parse_template_interpolation() {
        use crate::ast::Expr;
        let mut interner = Interner::new();
        let template =
            parse_template("The count is {count}.", &mut interner).expect("should parse");
        let exprs: Vec<&Expr> = template.embedded_exprs().collect();
        assert_eq!(exprs.len(), 1, "expected one embedded expression");
        assert!(
            matches!(exprs[0], Expr::Variable { .. }),
            "expected Variable, got {:?}",
            exprs[0]
        );
    }

    /// #39: a template with no interpolation exposes no embedded expressions.
    #[test]
    fn test_parse_template_text_only() {
        let mut interner = Interner::new();
        let template = parse_template("<p>hello</p>", &mut interner).expect("should parse");
        assert_eq!(template.embedded_exprs().count(), 0);
    }

    /// #153: an `on*` attribute interpolation becomes a handler part, and
    /// the `on...=` prefix is stripped from the literal text.
    #[test]
    fn test_event_attr_becomes_handler() {
        let mut interner = Interner::new();
        let template = parse_template("<button onclick={bump}>go</button>", &mut interner).unwrap();
        // the handler's expr is still an embedded expr (so deps track it)
        assert_eq!(template.embedded_exprs().count(), 1);
        // and it is recorded as a handler, not content: rendering it with a
        // placeholder value must not put the value inside the markup.
        let printed = format!("{:?}", template);
        assert!(
            printed.contains("Handler"),
            "expected a Handler part, got {}",
            printed
        );
        assert!(
            !printed.contains("onclick="),
            "onclick= prefix should be stripped from literal text: {}",
            printed
        );
    }

    /// #39: an unterminated interpolation is an error, not a panic.
    #[test]
    fn test_parse_template_unterminated() {
        let mut interner = Interner::new();
        let res = parse_template("count is {count", &mut interner);
        assert!(res.is_err());
    }

    /// #39: the html literal lexes and parses through to an Expr::Html node.
    #[test]
    fn test_html_literal_parses() {
        use crate::ast::{Decl, Expr, Stmt};
        let mut interner = Interner::new();
        let input = "service s { pub def h = (<p>hi</p>); }";
        let res = parse_string(input, &mut interner);
        assert!(res.is_ok(), "parse failed: {:?}", res);
        let ast = res.unwrap();
        match &ast[0] {
            Stmt::Service { decls, .. } => match &decls[0] {
                Decl::DefDecl { val, .. } => {
                    assert!(
                        matches!(val, Expr::Html(_)),
                        "expected Expr::Html, got {:?}",
                        val
                    );
                }
                other => panic!("expected DefDecl, got {:?}", other),
            },
            other => panic!("expected Service, got {:?}", other),
        }
    }

    /// #39: the full pipeline parses an html def with an interpolation; the
    /// template exposes the parsed embedded expression through its interface.
    #[test]
    fn test_html_literal_full_parse() {
        use crate::ast::{Decl, Expr, Stmt};
        let mut interner = Interner::new();
        let input = "service webClient { pub def html = (<p>The count is {count}.</p>); }";
        let res = parse_string(input, &mut interner);
        assert!(res.is_ok(), "parse failed: {:?}", res);
        let ast = res.unwrap();
        let template = match &ast[0] {
            Stmt::Service { decls, .. } => match &decls[0] {
                Decl::DefDecl {
                    val: Expr::Html(template),
                    ..
                } => template,
                other => panic!("expected DefDecl with Expr::Html, got {:?}", other),
            },
            other => panic!("expected Service, got {:?}", other),
        };
        let exprs: Vec<&Expr> = template.embedded_exprs().collect();
        assert_eq!(exprs.len(), 1, "expected one embedded expression");
        assert!(
            matches!(exprs[0], Expr::Variable { .. }),
            "expected Variable, got {:?}",
            exprs[0]
        );
    }
}
