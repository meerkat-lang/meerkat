pub mod lex;

// LALRPOP-generated parser
#[allow(clippy::all)]
#[allow(unused)]
pub mod meerkat {
    include!(concat!(env!("OUT_DIR"), "/runtime/parser/meerkat.rs"));
}

pub mod parser {
    use logos::Logos;
    use crate::ast::Prog;
    
    pub fn parse_string(input: &str) -> Result<Prog, String> {
        let lex_stream = super::lex::Token::lexer(input)
            .spanned()
            .map(|(t, span)| (span.start, t, span.end));

        super::meerkat::ProgParser::new()
            .parse(lex_stream)
            .map_err(|e| format!("Parse error: {:?}", e))
    }
    
    pub fn parse_file(filename: &str) -> Result<Prog, String> {
        let content = std::fs::read_to_string(filename)
            .map_err(|e| format!("Failed to read file: {}", e))?;
        parse_string(&content)
    }
}
