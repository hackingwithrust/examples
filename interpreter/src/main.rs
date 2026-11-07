//! birch, step 4: the tree.
//!
//! The parser now builds nodes instead of strings: one enum per node
//! kind, `Box` exactly where a field would otherwise make the type
//! infinite, and the statement half of the grammar arriving as more
//! variants. Each snippet prints back its `{:#?}` tree, so the enum
//! structure the parser chose is visible.

#[derive(Debug, Clone, PartialEq)]
enum TokenKind {
    // keywords
    Var,
    Fun,
    If,
    Else,
    While,
    Return,
    True,
    False,
    Nil,
    And,
    Or,
    // literals
    Ident,
    Number,
    Str,
    // punctuation
    LeftParen,
    RightParen,
    LeftBrace,
    RightBrace,
    LeftBracket,
    RightBracket,
    Comma,
    Semicolon,
    // operators
    Assign,
    Bang,
    BangEqual,
    EqualEqual,
    Greater,
    GreaterEqual,
    Less,
    LessEqual,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    // end of input
    Eof,
}

#[derive(Clone)]
struct Token {
    kind: TokenKind,
    lexeme: String,
    line: usize,
    col: usize,
}

struct ScanError {
    line: usize,
    col: usize,
    message: String,
}

struct Scanner {
    chars: Vec<char>,
    start: usize, // first char of the token being scanned
    current: usize,
    line: usize,
    col: usize,
    tokens: Vec<Token>,
    errors: Vec<ScanError>,
}

impl Scanner {
    fn new(source: &str) -> Self {
        Scanner {
            chars: source.chars().collect(),
            start: 0,
            current: 0,
            line: 1,
            col: 1,
            tokens: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn scan_tokens(mut self) -> (Vec<Token>, Vec<ScanError>) {
        while !self.at_end() {
            self.start = self.current;
            self.scan_token();
        }
        self.tokens.push(Token {
            kind: TokenKind::Eof,
            lexeme: String::new(),
            line: self.line,
            col: self.col,
        });
        (self.tokens, self.errors)
    }

    fn scan_token(&mut self) {
        let line = self.line;
        let col = self.col;
        let c = self.advance();
        match c {
            '(' => self.add(TokenKind::LeftParen, line, col),
            ')' => self.add(TokenKind::RightParen, line, col),
            '{' => self.add(TokenKind::LeftBrace, line, col),
            '}' => self.add(TokenKind::RightBrace, line, col),
            '[' => self.add(TokenKind::LeftBracket, line, col),
            ']' => self.add(TokenKind::RightBracket, line, col),
            ',' => self.add(TokenKind::Comma, line, col),
            ';' => self.add(TokenKind::Semicolon, line, col),
            '+' => self.add(TokenKind::Plus, line, col),
            '-' => self.add(TokenKind::Minus, line, col),
            '*' => self.add(TokenKind::Star, line, col),
            '%' => self.add(TokenKind::Percent, line, col),
            '/' => {
                if self.match_next('/') {
                    // a comment runs to the end of the line
                    while self.peek() != Some('\n') && self.peek().is_some() {
                        self.advance();
                    }
                } else {
                    self.add(TokenKind::Slash, line, col);
                }
            }
            '=' => {
                let kind = if self.match_next('=') {
                    TokenKind::EqualEqual
                } else {
                    TokenKind::Assign
                };
                self.add(kind, line, col);
            }
            '!' => {
                let kind = if self.match_next('=') {
                    TokenKind::BangEqual
                } else {
                    TokenKind::Bang
                };
                self.add(kind, line, col);
            }
            '<' => {
                let kind = if self.match_next('=') {
                    TokenKind::LessEqual
                } else {
                    TokenKind::Less
                };
                self.add(kind, line, col);
            }
            '>' => {
                let kind = if self.match_next('=') {
                    TokenKind::GreaterEqual
                } else {
                    TokenKind::Greater
                };
                self.add(kind, line, col);
            }
            '"' => self.string(line, col),
            c if c.is_ascii_digit() => self.number(line, col),
            c if c.is_ascii_alphabetic() || c == '_' => self.identifier(line, col),
            c if c.is_whitespace() => {
                if c == '\n' {
                    self.line += 1;
                    self.col = 1;
                }
            }
            other => self.errors.push(ScanError {
                line,
                col,
                message: format!("unexpected character `{other}`"),
            }),
        }
    }

    fn string(&mut self, line: usize, col: usize) {
        loop {
            match self.peek() {
                Some('"') => {
                    self.advance();
                    self.add(TokenKind::Str, line, col);
                    return;
                }
                Some('\n') | None => {
                    self.errors.push(ScanError {
                        line,
                        col,
                        message: String::from("unterminated string"),
                    });
                    return;
                }
                Some(_) => {
                    self.advance();
                }
            }
        }
    }

    fn number(&mut self, line: usize, col: usize) {
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.advance();
        }
        // a fraction, but only when the dot is followed by a digit:
        // `1.5` is one number, `1..3` is `1`, `.`, `.3`
        if self.peek() == Some('.') && self.peek2().is_some_and(|c| c.is_ascii_digit()) {
            self.advance(); // the dot
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.advance();
            }
        }
        self.add(TokenKind::Number, line, col);
    }

    fn identifier(&mut self, line: usize, col: usize) {
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            self.advance();
        }
        let word: String = self.chars[self.start..self.current].iter().collect();
        self.add(keyword(&word), line, col);
    }

    // -- cursor helpers -------------------------------------------------

    fn at_end(&self) -> bool {
        self.current >= self.chars.len()
    }

    fn advance(&mut self) -> char {
        let c = self.chars[self.current];
        self.current += 1;
        self.col += 1;
        c
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.current).copied()
    }

    fn peek2(&self) -> Option<char> {
        self.chars.get(self.current + 1).copied()
    }

    fn match_next(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn add(&mut self, kind: TokenKind, line: usize, col: usize) {
        let lexeme: String = self.chars[self.start..self.current].iter().collect();
        self.tokens.push(Token {
            kind,
            lexeme,
            line,
            col,
        });
    }
}

fn keyword(word: &str) -> TokenKind {
    match word {
        "var" => TokenKind::Var,
        "fun" => TokenKind::Fun,
        "if" => TokenKind::If,
        "else" => TokenKind::Else,
        "while" => TokenKind::While,
        "return" => TokenKind::Return,
        "true" => TokenKind::True,
        "false" => TokenKind::False,
        "nil" => TokenKind::Nil,
        "and" => TokenKind::And,
        "or" => TokenKind::Or,
        _ => TokenKind::Ident,
    }
}

// ------------------------------------------------------------- the tree

// One variant per shape an expression can take. Every field that would
// make the enum infinitely sized holds a `Box`; fields already behind a
// `Vec` or an `Option` break the recursion for free.
#[derive(Debug)]
enum Expr {
    Number(f64),
    Str(String),
    Bool(bool),
    Nil,
    Variable(String),
    Unary { op: TokenKind, operand: Box<Expr> },
    Binary { op: TokenKind, left: Box<Expr>, right: Box<Expr> },
    Logical { op: TokenKind, left: Box<Expr>, right: Box<Expr> },
    Assign { name: String, value: Box<Expr> },
    Call { callee: Box<Expr>, args: Vec<Expr> },
    Index { object: Box<Expr>, index: Box<Expr> },
    List(Vec<Expr>),
}

// One variant per shape a statement can take. `if` and `while` hold a
// single statement each, so the statement type recurses through `Box`
// too; a block's `Vec<Stmt>` needs no box of its own.
#[derive(Debug)]
enum Stmt {
    Var { name: String, init: Expr },
    Fun { name: String, params: Vec<String>, body: Vec<Stmt> },
    Expression(Expr),
    If { condition: Expr, then_branch: Box<Stmt>, else_branch: Option<Box<Stmt>> },
    While { condition: Expr, body: Box<Stmt> },
    Return { value: Option<Expr> },
    Block(Vec<Stmt>),
}

// ------------------------------------------------------------ the parser

struct ParseError {
    line: usize,
    col: usize,
    message: String,
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

// binds tighter than every infix operator, so `-n * 2` groups as `(-n) * 2`
const UNARY: u8 = 9;

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Parser { tokens, pos: 0 }
    }

    // program → declaration* EOF
    fn program(&mut self) -> Result<Vec<Stmt>, ParseError> {
        let mut declarations = Vec::new();
        while self.peek().kind != TokenKind::Eof {
            declarations.push(self.declaration()?);
        }
        Ok(declarations)
    }

    // declaration → varDecl | funDecl | statement
    fn declaration(&mut self) -> Result<Stmt, ParseError> {
        match self.peek().kind {
            TokenKind::Var => self.var_declaration(),
            TokenKind::Fun => self.fun_declaration(),
            _ => self.statement(),
        }
    }

    // varDecl → "var" IDENT "=" expression ";"
    fn var_declaration(&mut self) -> Result<Stmt, ParseError> {
        self.advance(); // the `var`
        let name = self.expect_ident("a variable name")?;
        self.expect(TokenKind::Assign, "`=`")?;
        let init = self.expression(0)?;
        self.expect(TokenKind::Semicolon, "`;`")?;
        Ok(Stmt::Var { name, init })
    }

    // funDecl → "fun" IDENT "(" parameters? ")" block
    fn fun_declaration(&mut self) -> Result<Stmt, ParseError> {
        self.advance(); // the `fun`
        let name = self.expect_ident("a function name")?;
        self.expect(TokenKind::LeftParen, "`(`")?;
        let mut params = Vec::new();
        if self.peek().kind != TokenKind::RightParen {
            loop {
                params.push(self.expect_ident("a parameter name")?);
                if !self.match_kind(TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(TokenKind::RightParen, "`)`")?;
        let body = self.block()?;
        Ok(Stmt::Fun { name, params, body })
    }

    // statement → exprStmt | ifStmt | whileStmt | returnStmt | block
    fn statement(&mut self) -> Result<Stmt, ParseError> {
        match self.peek().kind {
            TokenKind::If => self.if_statement(),
            TokenKind::While => self.while_statement(),
            TokenKind::Return => self.return_statement(),
            TokenKind::LeftBrace => Ok(Stmt::Block(self.block()?)),
            _ => {
                let expr = self.expression(0)?;
                self.expect(TokenKind::Semicolon, "`;`")?;
                Ok(Stmt::Expression(expr))
            }
        }
    }

    // ifStmt → "if" "(" expression ")" statement ("else" statement)?
    fn if_statement(&mut self) -> Result<Stmt, ParseError> {
        self.advance(); // the `if`
        self.expect(TokenKind::LeftParen, "`(`")?;
        let condition = self.expression(0)?;
        self.expect(TokenKind::RightParen, "`)`")?;
        let then_branch = Box::new(self.statement()?);
        let else_branch = if self.match_kind(TokenKind::Else) {
            Some(Box::new(self.statement()?))
        } else {
            None
        };
        Ok(Stmt::If { condition, then_branch, else_branch })
    }

    // whileStmt → "while" "(" expression ")" statement
    fn while_statement(&mut self) -> Result<Stmt, ParseError> {
        self.advance(); // the `while`
        self.expect(TokenKind::LeftParen, "`(`")?;
        let condition = self.expression(0)?;
        self.expect(TokenKind::RightParen, "`)`")?;
        let body = Box::new(self.statement()?);
        Ok(Stmt::While { condition, body })
    }

    // returnStmt → "return" expression? ";"
    fn return_statement(&mut self) -> Result<Stmt, ParseError> {
        self.advance(); // the `return`
        let value = if self.peek().kind == TokenKind::Semicolon {
            None
        } else {
            Some(self.expression(0)?)
        };
        self.expect(TokenKind::Semicolon, "`;`")?;
        Ok(Stmt::Return { value })
    }

    // block → "{" declaration* "}"
    fn block(&mut self) -> Result<Vec<Stmt>, ParseError> {
        self.expect(TokenKind::LeftBrace, "`{`")?;
        let mut body = Vec::new();
        while self.peek().kind != TokenKind::RightBrace
            && self.peek().kind != TokenKind::Eof
        {
            body.push(self.declaration()?);
        }
        self.expect(TokenKind::RightBrace, "`}`")?;
        Ok(body)
    }

    // The Pratt loop from the last step, now building nodes instead of
    // strings. Nothing else about the algorithm changed.
    fn expression(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.prefix()?;
        loop {
            // calls and indexes bind tighter than anything in the table:
            // they hug whatever is on their left, whatever the minimum
            match self.peek().kind {
                TokenKind::LeftParen => {
                    self.pos += 1;
                    let mut args = Vec::new();
                    if self.peek().kind != TokenKind::RightParen {
                        loop {
                            args.push(self.expression(0)?);
                            if !self.match_kind(TokenKind::Comma) {
                                break;
                            }
                        }
                    }
                    self.expect(TokenKind::RightParen, "`)`")?;
                    lhs = Expr::Call { callee: Box::new(lhs), args };
                    continue;
                }
                TokenKind::LeftBracket => {
                    self.pos += 1;
                    let index = self.expression(0)?;
                    self.expect(TokenKind::RightBracket, "`]`")?;
                    lhs = Expr::Index { object: Box::new(lhs), index: Box::new(index) };
                    continue;
                }
                _ => {}
            }
            let Some((left, right)) = self.infix_binding_power() else {
                break;
            };
            if left < min_prec {
                break;
            }
            let op = self.advance();
            if op.kind == TokenKind::Assign {
                // the check that needed a bool threaded through every
                // level last step is now a question about the node itself
                let Expr::Variable(name) = lhs else {
                    return Err(ParseError {
                        line: op.line,
                        col: op.col,
                        message: String::from("invalid assignment target"),
                    });
                };
                let value = self.expression(right)?;
                lhs = Expr::Assign { name, value: Box::new(value) };
                continue;
            }
            let rhs = self.expression(right)?;
            lhs = if op.kind == TokenKind::And || op.kind == TokenKind::Or {
                // a separate variant: `and`/`or` will short-circuit when
                // the evaluator arrives, and the tree records that
                Expr::Logical { op: op.kind, left: Box::new(lhs), right: Box::new(rhs) }
            } else {
                Expr::Binary { op: op.kind, left: Box::new(lhs), right: Box::new(rhs) }
            };
        }
        Ok(lhs)
    }

    // Infix binding powers, lowest row first. A pair is (left, right):
    // parsing the right side with `right` as its minimum is what makes
    // associativity work. `left + 1` stops the right side before another
    // operator of the same row — left-associative. `=` is the one
    // right-associative operator, so its right power sits below its left.
    fn infix_binding_power(&self) -> Option<(u8, u8)> {
        Some(match self.peek().kind {
            TokenKind::Assign => (1, 0),
            TokenKind::Or => (2, 3),
            TokenKind::And => (3, 4),
            TokenKind::EqualEqual | TokenKind::BangEqual => (4, 5),
            TokenKind::Less | TokenKind::LessEqual | TokenKind::Greater
            | TokenKind::GreaterEqual => (5, 6),
            TokenKind::Plus | TokenKind::Minus => (6, 7),
            TokenKind::Star | TokenKind::Slash | TokenKind::Percent => (7, 8),
            _ => return None,
        })
    }

    fn prefix(&mut self) -> Result<Expr, ParseError> {
        let token = self.advance();
        match token.kind {
            TokenKind::Number => {
                // the scanner only ever hands over digit strings, so
                // this parse cannot fail
                let value: f64 = token.lexeme.parse().unwrap();
                Ok(Expr::Number(value))
            }
            TokenKind::Str => {
                // strip the two quote characters the scanner kept
                Ok(Expr::Str(token.lexeme[1..token.lexeme.len() - 1].to_string()))
            }
            TokenKind::True => Ok(Expr::Bool(true)),
            TokenKind::False => Ok(Expr::Bool(false)),
            TokenKind::Nil => Ok(Expr::Nil),
            TokenKind::Ident => Ok(Expr::Variable(token.lexeme)),
            TokenKind::Minus | TokenKind::Bang => {
                let operand = self.expression(UNARY)?;
                Ok(Expr::Unary { op: token.kind, operand: Box::new(operand) })
            }
            TokenKind::LeftParen => {
                // no Grouping variant: the tree's shape *is* the grouping,
                // the parentheses have done their job once the inner
                // expression is built
                let inner = self.expression(0)?;
                self.expect(TokenKind::RightParen, "`)`")?;
                Ok(inner)
            }
            TokenKind::LeftBracket => {
                let mut items = Vec::new();
                if self.peek().kind != TokenKind::RightBracket {
                    loop {
                        items.push(self.expression(0)?);
                        if !self.match_kind(TokenKind::Comma) {
                            break;
                        }
                    }
                }
                self.expect(TokenKind::RightBracket, "`]`")?;
                Ok(Expr::List(items))
            }
            _ => Err(ParseError {
                line: token.line,
                col: token.col,
                message: if token.kind == TokenKind::Eof {
                    String::from("expected an expression, found the end of input")
                } else {
                    format!("expected an expression, found `{}`", token.lexeme)
                },
            }),
        }
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn advance(&mut self) -> Token {
        let token = self.tokens[self.pos].clone();
        if token.kind != TokenKind::Eof {
            self.pos += 1;
        }
        token
    }

    fn match_kind(&mut self, kind: TokenKind) -> bool {
        if self.peek().kind == kind {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: TokenKind, what: &str) -> Result<Token, ParseError> {
        let token = self.advance();
        if token.kind == kind {
            return Ok(token);
        }
        let message = if token.kind == TokenKind::Eof {
            format!("expected {what}, found the end of input")
        } else {
            format!("expected {what}, found `{}`", token.lexeme)
        };
        Err(ParseError {
            line: token.line,
            col: token.col,
            message,
        })
    }

    fn expect_ident(&mut self, what: &str) -> Result<String, ParseError> {
        let token = self.expect(TokenKind::Ident, what)?;
        Ok(token.lexeme)
    }
}

fn main() {
    let sources = [
        r#"var count = 0;
while (count < 3) {
  count = count + 1;
}
if (count == 3 and !done) {
  var label = "hot";
}
fun twice(n) { return n * 2; }
var xs = [twice(count), twice(count * 10)];
var first = xs[0];"#,
        "var = 5;",
    ];

    for source in sources {
        println!("{source}");
        let (tokens, errors) = Scanner::new(source).scan_tokens();
        if !errors.is_empty() {
            for e in &errors {
                println!("  scan error: {}:{} {}", e.line, e.col, e.message);
            }
            continue;
        }
        match Parser::new(tokens).program() {
            Ok(tree) => println!("{tree:#?}"),
            Err(e) => println!("  parse error: {}:{} {}", e.line, e.col, e.message),
        }
    }
}
