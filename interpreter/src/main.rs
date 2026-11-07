//! birch, step 3: the expression parser.
//!
//! A Pratt parser: precedence is one table, not one function per level
//! of the grammar. Each snippet prints back with every grouping the
//! parser chose made explicit, so the parse decisions are visible.

#[derive(Debug, Clone, PartialEq)]
enum TokenKind {
    // keywords
    Var,
    Fun,
    If,
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

    fn parse(&mut self) -> Result<String, ParseError> {
        let (tree, _) = self.expression(0)?;
        let trailing = self.advance();
        if trailing.kind != TokenKind::Eof {
            return Err(ParseError {
                line: trailing.line,
                col: trailing.col,
                message: format!("unexpected `{}` after the expression", trailing.lexeme),
            });
        }
        Ok(tree)
    }

    // Returns the expression printed back with every grouping explicit,
    // plus whether it is a bare variable — the one thing `=` cares about.
    fn expression(&mut self, min_prec: u8) -> Result<(String, bool), ParseError> {
        let (mut lhs, mut is_var) = self.prefix()?;
        loop {
            // calls and indexes bind tighter than anything in the table:
            // they hug whatever is on their left, whatever the minimum
            match self.peek().kind {
                TokenKind::LeftParen => {
                    self.pos += 1;
                    let mut args = Vec::new();
                    if self.peek().kind != TokenKind::RightParen {
                        loop {
                            args.push(self.expression(0)?.0);
                            if !self.match_kind(TokenKind::Comma) {
                                break;
                            }
                        }
                    }
                    self.expect(TokenKind::RightParen, "`)`")?;
                    lhs = format!("({lhs}({}))", args.join(", "));
                    is_var = false;
                    continue;
                }
                TokenKind::LeftBracket => {
                    self.pos += 1;
                    let index = self.expression(0)?.0;
                    self.expect(TokenKind::RightBracket, "`]`")?;
                    lhs = format!("({lhs}[{index}])");
                    is_var = false;
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
            if op.kind == TokenKind::Assign && !is_var {
                return Err(ParseError {
                    line: op.line,
                    col: op.col,
                    message: String::from("invalid assignment target"),
                });
            }
            let rhs = self.expression(right)?.0;
            lhs = format!("({lhs} {} {rhs})", op.lexeme);
            is_var = false;
        }
        Ok((lhs, is_var))
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

    fn prefix(&mut self) -> Result<(String, bool), ParseError> {
        let token = self.advance();
        match token.kind {
            TokenKind::Ident => Ok((token.lexeme, true)),
            TokenKind::Number | TokenKind::Str | TokenKind::True | TokenKind::False
            | TokenKind::Nil => Ok((token.lexeme, false)),
            TokenKind::Minus | TokenKind::Bang => {
                let operand = self.expression(UNARY)?.0;
                Ok((format!("({} {operand})", token.lexeme), false))
            }
            TokenKind::LeftParen => {
                let inner = self.expression(0)?.0;
                self.expect(TokenKind::RightParen, "`)`")?;
                Ok((inner, false))
            }
            TokenKind::LeftBracket => {
                let mut items = Vec::new();
                if self.peek().kind != TokenKind::RightBracket {
                    loop {
                        items.push(self.expression(0)?.0);
                        if !self.match_kind(TokenKind::Comma) {
                            break;
                        }
                    }
                }
                self.expect(TokenKind::RightBracket, "`]`")?;
                Ok((format!("([{}])", items.join(", ")), false))
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

    fn expect(&mut self, kind: TokenKind, what: &str) -> Result<(), ParseError> {
        let token = self.advance();
        if token.kind == kind {
            return Ok(());
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
}

fn main() {
    let snippets = [
        "1 + 2 * 3",
        "(1 + 2) * 3",
        "10 - 4 - 3",
        "-n * 2",
        "!done and count < 10",
        "n = n + 1",
        "names[i + 1]",
        "makeCounter(10)()",
        "1 +",
        "1 = 2",
    ];

    for source in snippets {
        println!("{source}");
        let (tokens, errors) = Scanner::new(source).scan_tokens();
        if !errors.is_empty() {
            for e in &errors {
                println!("  scan error: {}:{} {}", e.line, e.col, e.message);
            }
            continue;
        }
        match Parser::new(tokens).parse() {
            Ok(tree) => println!("  = {tree}"),
            Err(e) => println!("  parse error: {}:{} {}", e.line, e.col, e.message),
        }
    }
}
