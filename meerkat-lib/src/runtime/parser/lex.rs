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
use std::{fmt, num::ParseIntError};
use strum_macros::AsRefStr;

fn from_num<'b>(lex: &mut Lexer<'b, Token<'b>>) -> Result<i32, String> {
    let slice = lex.slice();

    let res = slice.parse();

    if res.is_err() {
        return Err(format!("Parsing failed wtih Error {:?}", res.unwrap_err()));
    }
    let out: i64 = res.unwrap();
    if out > ((i32::MIN as i64).abs()) {
        // All numbers are positive because - is lexed seperately
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
                None => panic!("Reached end of file"),
                _ => {
                    lex.bump_unchecked(1);
                }
            }
        }
    }
    Skip
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
  #[token("number")]
  NUMBER_KW,
  #[token("string")]
  STRING_KW,
  #[token("bool")]
  BOOL_KW,
  

    #[regex(r"\s*", logos::skip)]
    #[regex(r#"(//)[^\n]*"#, logos::skip)] // Regex for a single line comment
    // Yes there is regex for this no I could not get it to work
    #[token("/*", skip_multi_line_comments)] // Match start of multiline
    Comment,
    
    #[error]
    #[regex(r#"[^\x00-\x7F]"#)] // Error on non ascii characters
    Error,
}
