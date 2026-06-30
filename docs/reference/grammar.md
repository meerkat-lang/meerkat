# Meerkat Programming Language Grammar

The following is is the official reference grammar for the Meerkat programming language.


```
<prog> ::=
| { <stmt> }

<stmt> ::=
| "service" <name> "{" { <decl> } "}"
| "@test" "(" <name> ")" "{" { <action_stmt> } "}"
| "import" <name>
| <action_stmt>
| "watch" <expr> ";"

<decl> ::=
| "var" <name> [ ":" <type> ] "=" <expr> ";"
| [ "pub" ] "def" <name> [ ":" <type> ] "=" <expr> ";"
| "table" <name> "{" <table_fields> "}" ";"

<table_field> ::=
| <name> ":" <table_type> ","

<table_fields> ::=
| { <table_field> }

<action_stmt> ::=
| "let" <name> [ ":" <type> ] "=" <expr> ";"
| "do" <expr> ";"
| "assert" "(" <expr> ")" ";"
| "insert" <expr> "into" <name>
| <name> "=" <expr> ";"
| <expr> ";"

<expr> ::=
| <sub_expr>
| "-" <expr>
| "!" <expr>
| <name> ":" <expr>
| <expr> "*" <expr>
| <expr> "/" <expr>
| <expr> "+" <expr>
| <expr> "-" <expr>
| <expr> "==" <expr>
| <expr> "<" <expr>
| <expr> ">" <expr>
| <expr> "&&" <expr>
| <expr> "||" <expr>
| "if" <expr> "then" <expr> "else" <expr>
| "fn" <params> "=>" <expr>
| "select" <name> { "," <name> } "from" <name> "where" <expr>
| "fold" "(" <name> "." <name> "," <expr> "," <expr> ")"

<sub_expr> ::=
| <num>
| <bool>
| <strlit>
| <name>
| "{" [ <expr> { "," <expr> } ] "}"
| "(" <expr> ")"
| <sub_expr> "(" [ <expr> { "," <expr> } ] ")"
| <name> "." <name>
| "action" "{" { <action_stmt> } "}"

<simple_type> ::=
| "int"
| "string"
| "bool"
| "unit"
| "(" <type> { "," <type> } ")"

<type> ::=
| <simple_type> [ "->" <type>]
| "()" "->" <type>

<table_type> ::=
| "int"
| "string"
| "bool"

<params> ::=
| "()"
| <name>
| "(" <param> { "," <param> } ")"

<param> ::=
| <name> [ ":" <type> ]

```
