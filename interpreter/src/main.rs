//! birch, step 2: the scanner.
//!
//! Reads birch source and produces a list of tokens, each carrying the
//! line and column it started at. Errors go into a list instead of
//! stopping the scan, so one bad character does not hide the rest.

#[derive(Debug, Clone)]
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

fn main() {
    let source = r#"// the counter, from the design article
var greeting = "hi";
fun makeCounter(start) {
  var n = start;
  return n;
}

var tick = makeCounter(10);
print tick();
"#;

    let (tokens, errors) = Scanner::new(source).scan_tokens();

    for e in &errors {
        println!("error: {}:{} {}", e.line, e.col, e.message);
    }
    for t in &tokens {
        println!("{:3}:{:<3} {:<12} {}", t.line, t.col, format!("{:?}", t.kind), t.lexeme);
    }
}
