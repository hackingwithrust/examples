//! birch, step 9: a REPL for the interpreter.
//!
//! Until now every run was a batch: a script handed to a driver that
//! scanned, parsed and executed it top to bottom. This step splits the
//! driver in two — `birch program.birch` still runs a file, and with no
//! argument the same pipeline runs inside a read-eval-print loop: one
//! environment across the whole session, a prompt flushed by hand, and
//! a parser that can tell `wrong` from `not finished yet`, so a
//! declaration too long for one line can simply be continued.

use std::collections::HashMap;
use std::io::{self, Write};
use std::rc::Rc;

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

#[derive(Debug, Clone, PartialEq)]
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
// `Vec` or an `Option` break the recursion for free. `Clone` and
// `PartialEq` exist because a function value holds part of the tree,
// so the tree has to be a clonable, comparable citizen.
#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Number(f64),
    Str(String),
    Bool(bool),
    Nil,
    // The variants that can fail at runtime keep the whole token, not
    // a distillation of it: an operator that needs numbers knows where
    // it sits, a name that goes missing knows its own position, a call
    // that goes wrong knows which paren opened it. `Logical` keeps the
    // bare kind — it can never fail, so it has nothing to point at.
    Variable(Token),
    Unary { op: Token, operand: Box<Expr> },
    Binary { op: Token, left: Box<Expr>, right: Box<Expr> },
    Logical { op: TokenKind, left: Box<Expr>, right: Box<Expr> },
    Assign { name: Token, value: Box<Expr> },
    Call { callee: Box<Expr>, args: Vec<Expr>, paren: Token },
    Index { object: Box<Expr>, index: Box<Expr>, bracket: Token },
    List(Vec<Expr>),
}

// One variant per shape a statement can take. `if` and `while` hold a
// single statement each, so the statement type recurses through `Box`
// too; a block's `Vec<Stmt>` needs no box of its own.
#[derive(Debug, Clone, PartialEq)]
enum Stmt {
    Var { name: String, init: Expr },
    Fun { name: String, params: Vec<Token>, body: Vec<Stmt> },
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
    // true when the culprit token is the end of input: nothing was
    // wrong, the input just stopped. The REPL reads this one bit as
    // `not finished yet` and asks for another line; a file run never
    // asks, because a file cannot grow.
    incomplete: bool,
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
        let name = self.expect_ident("a variable name")?.lexeme;
        self.expect(TokenKind::Assign, "`=`")?;
        let init = self.expression(0)?;
        self.expect(TokenKind::Semicolon, "`;`")?;
        Ok(Stmt::Var { name, init })
    }

    // funDecl → "fun" IDENT "(" parameters? ")" block
    fn fun_declaration(&mut self) -> Result<Stmt, ParseError> {
        self.advance(); // the `fun`
        let name = self.expect_ident("a function name")?.lexeme;
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

    // The Pratt loop from two steps ago, building nodes.
    fn expression(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.prefix()?;
        loop {
            // calls and indexes bind tighter than anything in the table:
            // they hug whatever is on their left, whatever the minimum
            match self.peek().kind {
                TokenKind::LeftParen => {
                    // the paren itself goes into the tree: when this
                    // call fails at runtime, the error points here
                    let paren = self.advance();
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
                    lhs = Expr::Call { callee: Box::new(lhs), args, paren };
                    continue;
                }
                TokenKind::LeftBracket => {
                    // same story: the bracket is where an index error
                    // will point
                    let bracket = self.advance();
                    let index = self.expression(0)?;
                    self.expect(TokenKind::RightBracket, "`]`")?;
                    lhs = Expr::Index { object: Box::new(lhs), index: Box::new(index), bracket };
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
                // level two steps ago is now a question about the node
                let Expr::Variable(name) = lhs else {
                    return Err(ParseError {
                        line: op.line,
                        col: op.col,
                        message: String::from("invalid assignment target"),
                        // the `=` itself was found; the target is
                        // what is wrong, and no more input fixes it
                        incomplete: false,
                    });
                };
                let value = self.expression(right)?;
                lhs = Expr::Assign { name, value: Box::new(value) };
                continue;
            }
            let rhs = self.expression(right)?;
            lhs = if op.kind == TokenKind::And || op.kind == TokenKind::Or {
                // a separate variant: `and`/`or` short-circuit, and the
                // tree records that — as a bare kind, because neither
                // one can ever produce a runtime error
                Expr::Logical { op: op.kind, left: Box::new(lhs), right: Box::new(rhs) }
            } else {
                // the whole token, not just the kind: when this
                // operator fails at runtime, the error needs its
                // position
                Expr::Binary { op, left: Box::new(lhs), right: Box::new(rhs) }
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
            TokenKind::Ident => Ok(Expr::Variable(token)),
            TokenKind::Minus | TokenKind::Bang => {
                let operand = self.expression(UNARY)?;
                Ok(Expr::Unary { op: token, operand: Box::new(operand) })
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
                incomplete: token.kind == TokenKind::Eof,
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
            incomplete: token.kind == TokenKind::Eof,
        })
    }

    // The whole token, not just the name: callers that will need a
    // position for a later error — function parameters, for one —
    // keep everything the scanner recorded.
    fn expect_ident(&mut self, what: &str) -> Result<Token, ParseError> {
        self.expect(TokenKind::Ident, what)
    }
}

// ------------------------------------------------------------ the values

// A function value: the declaration the parser built, held as runtime
// data. The tree's `Stmt::Fun` describes a definition site; this is
// the thing a name gets bound to and a call runs. `Rc` keeps it cheap
// inside a `Value`, which must stay clonable; the derived `Debug` and
// `PartialEq` are what `Value`'s own derives ask for.
#[derive(Debug, Clone, PartialEq)]
struct FunctionValue {
    name: String,
    params: Vec<String>,
    body: Vec<Stmt>,
}

// What an expression evaluates *to*. The tree describes computation; a
// value is the thing computation produces. One enum again, and this one
// has no recursion at all: values are flat.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    Number(f64),
    Str(String),
    Bool(bool),
    Nil,
    List(Vec<Value>),
    Function(Rc<FunctionValue>),
}

impl Value {
    // everything is true except `nil` and `false` themselves — the
    // rule Python and Lua use, and the shortest one to explain
    fn is_truthy(&self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }
}

// `Debug` would print `Number(7.0)`; the language's own printer should
// print `7`. This impl is what makes values look like *birch* values.
impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Number(n) => write!(f, "{n}"),
            Value::Str(s) => write!(f, "{s}"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Nil => write!(f, "nil"),
            // a function has no printed form, only a name to be
            // recognised by — the choice Python made with `<fun ...>`
            Value::Function(function) => write!(f, "<fun {}>", function.name),
            Value::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
        }
    }
}

// The word for a value in error messages: "got a string and a number"
// beats "got Str and Number".
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Number(_) => "a number",
        Value::Str(_) => "a string",
        Value::Bool(_) => "a bool",
        Value::Nil => "nil",
        Value::List(_) => "a list",
        Value::Function(_) => "a function",
    }
}

// The third error type catches up with the other two: a runtime error
// now carries the position of the token that caused it. Scan errors
// have had `line`/`col` since the scanning step and parse errors since
// the parsing one; `evaluate` fills both fields in from the token the
// failing arm kept — which is why the tree stores whole tokens where
// an error is possible.
struct RuntimeError {
    line: usize,
    col: usize,
    message: String,
}

// How a statement finishes. Most statements run to the end and say so;
// a `return` has to travel — out of the body it sits in, past every
// enclosing block and loop, to the one place that knows what to do
// with it: the call running the function. Travelling as the `Ok` half
// of the result keeps the `Err` half for actual errors.
enum Flow {
    Normal,
    Return(Value),
}

// ------------------------------------------------------- the environment

// The interpreter's memory. One scope is one map of names to values, and
// the stack of scopes *is* the chain the classic interpreter picture
// draws: a name is looked up in the innermost scope first, then the one
// below it, and so on down to the global scope at the bottom.
struct Environment {
    scopes: Vec<HashMap<String, Value>>,
}

impl Environment {
    fn new() -> Self {
        Environment {
            scopes: vec![HashMap::new()],
        }
    }

    // `var`: a name appears in the innermost scope, shadowing anything
    // of the same name further out
    fn define(&mut self, name: String, value: Value) {
        self.scopes.last_mut().unwrap().insert(name, value);
    }

    // reading walks the chain from the top down and returns the first
    // match; a missing name is the caller's error to report
    fn get(&self, name: &str) -> Option<Value> {
        for scope in self.scopes.iter().rev() {
            if let Some(value) = scope.get(name) {
                return Some(value.clone());
            }
        }
        None
    }

    // assignment also walks the chain, but it can only *update* a scope
    // that already has the name — `count = 1` inside a block writes to
    // the outer binding, or fails if no scope down the chain has it
    fn assign(&mut self, name: &str, value: Value) -> bool {
        for scope in self.scopes.iter_mut().rev() {
            if scope.contains_key(name) {
                scope.insert(name.to_string(), value);
                return true;
            }
        }
        false
    }
}

// --------------------------------------------------------- the evaluator

// One function per half of the grammar, each one a `match` over the
// enum the parser builds. The shape of the code is the shape of the
// tree: children are evaluated first (post-order), then the operator
// acts on the values that came back. Both functions now carry the
// environment with them, so any node that mentions a name can reach it.
fn evaluate(expr: &Expr, env: &mut Environment) -> Result<Value, RuntimeError> {
    match expr {
        Expr::Number(n) => Ok(Value::Number(*n)),
        Expr::Str(s) => Ok(Value::Str(s.clone())),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Nil => Ok(Value::Nil),
        Expr::List(items) => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                values.push(evaluate(item, env)?);
            }
            Ok(Value::List(values))
        }
        Expr::Unary { op, operand } => {
            let value = evaluate(operand, env)?;
            match op.kind {
                TokenKind::Minus => match value {
                    Value::Number(n) => Ok(Value::Number(-n)),
                    _ => Err(RuntimeError {
                        line: op.line,
                        col: op.col,
                        message: format!(
                            "`{}` needs a number, got {}",
                            op.lexeme,
                            type_name(&value)
                        ),
                    }),
                },
                // `!` accepts anything: the question is only truthiness
                TokenKind::Bang => Ok(Value::Bool(!value.is_truthy())),
                _ => unreachable!("the parser only builds Unary from `-` and `!`"),
            }
        }
        Expr::Binary { op, left, right } => {
            // both operands are evaluated first: `Binary` is the eager
            // half, `Logical` below is the half that can skip
            let l = evaluate(left, env)?;
            let r = evaluate(right, env)?;
            match op.kind {
                TokenKind::Plus => match (&l, &r) {
                    // one operator, two behaviours: numbers add,
                    // strings concatenate — anything else is a runtime
                    // error, not a silent coercion
                    (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a + b)),
                    (Value::Str(a), Value::Str(b)) => Ok(Value::Str(format!("{a}{b}"))),
                    _ => Err(RuntimeError {
                        line: op.line,
                        col: op.col,
                        message: format!(
                            "`{}` needs two numbers or two strings, got {} and {}",
                            op.lexeme,
                            type_name(&l),
                            type_name(&r)
                        ),
                    }),
                },
                TokenKind::Minus | TokenKind::Star | TokenKind::Slash
                | TokenKind::Percent => {
                    let (Value::Number(a), Value::Number(b)) = (&l, &r) else {
                        return Err(RuntimeError {
                            line: op.line,
                            col: op.col,
                            message: format!(
                                "`{}` needs two numbers, got {} and {}",
                                op.lexeme,
                                type_name(&l),
                                type_name(&r)
                            ),
                        });
                    };
                    let value = match op.kind {
                        TokenKind::Slash | TokenKind::Percent if *b == 0.0 => {
                            // f64 division would happily return `inf`
                            // or `NaN`; the language refuses instead
                            return Err(RuntimeError {
                                line: op.line,
                                col: op.col,
                                message: String::from("division by zero"),
                            });
                        }
                        TokenKind::Minus => a - b,
                        TokenKind::Star => a * b,
                        TokenKind::Slash => a / b,
                        _ => a % b,
                    };
                    Ok(Value::Number(value))
                }
                TokenKind::Less | TokenKind::LessEqual | TokenKind::Greater
                | TokenKind::GreaterEqual => {
                    let (Value::Number(a), Value::Number(b)) = (&l, &r) else {
                        return Err(RuntimeError {
                            line: op.line,
                            col: op.col,
                            message: format!(
                                "`{}` needs two numbers, got {} and {}",
                                op.lexeme,
                                type_name(&l),
                                type_name(&r)
                            ),
                        });
                    };
                    Ok(Value::Bool(match op.kind {
                        TokenKind::Less => a < b,
                        TokenKind::LessEqual => a <= b,
                        TokenKind::Greater => a > b,
                        _ => a >= b,
                    }))
                }
                // derived `PartialEq` gives the right answer across
                // variants for free: `1 == "1"` is simply false, no
                // coercion, no error
                TokenKind::EqualEqual => Ok(Value::Bool(l == r)),
                TokenKind::BangEqual => Ok(Value::Bool(l != r)),
                _ => unreachable!(
                    "the parser only builds Binary from the operators above"
                ),
            }
        }
        Expr::Logical { op, left, right } => {
            let value = evaluate(left, env)?;
            let decided = match op {
                // `and`: a falsey left has already decided the result
                TokenKind::And => !value.is_truthy(),
                // `or`: a truthy left has already decided the result
                TokenKind::Or => value.is_truthy(),
                _ => unreachable!("the parser only builds Logical from `and` and `or`"),
            };
            // the result is the deciding operand's *value*, so
            // `nil or "fallback"` is "fallback" — and when the left
            // decides, the right is never evaluated at all
            if decided {
                Ok(value)
            } else {
                evaluate(right, env)
            }
        }
        // The name-reading arms. `Variable` asks the environment;
        // `Assign` evaluates its right side, then asks the environment
        // to update the first scope down the chain that already has
        // the name.
        Expr::Variable(token) => env.get(&token.lexeme).ok_or(RuntimeError {
            line: token.line,
            col: token.col,
            message: format!("undefined variable `{}`", token.lexeme),
        }),
        Expr::Assign { name, value } => {
            // assignment is an expression: its value is the value
            // assigned, which is what makes `a = b = 2` parse and work
            // with no extra machinery — the parser already made `=`
            // right-associative, and the value just falls through
            let value = evaluate(value, env)?;
            if env.assign(&name.lexeme, value.clone()) {
                Ok(value)
            } else {
                Err(RuntimeError {
                    line: name.line,
                    col: name.col,
                    message: format!("undefined variable `{}`", name.lexeme),
                })
            }
        }
        Expr::Call { callee, args, paren } => {
            // the callee evaluates first, so an unknown name reports
            // itself, then the arguments — the order a real call would
            // use. Both errors a call can produce point at the `(`
            // that opened it: the token that says "this is a call"
            let callee = evaluate(callee, env)?;
            let mut values = Vec::with_capacity(args.len());
            for arg in args {
                values.push(evaluate(arg, env)?);
            }
            let Value::Function(function) = callee else {
                return Err(RuntimeError {
                    line: paren.line,
                    col: paren.col,
                    message: format!("{} cannot be called", type_name(&callee)),
                });
            };
            if values.len() != function.params.len() {
                return Err(RuntimeError {
                    line: paren.line,
                    col: paren.col,
                    message: format!(
                        "`{}` expects {} argument{}, got {}",
                        function.name,
                        function.params.len(),
                        if function.params.len() == 1 { "" } else { "s" },
                        values.len()
                    ),
                });
            }
            call(&function, values, env)
        }
        Expr::Index { object, index, bracket } => {
            // indexing needs no environment: both sides evaluate to
            // plain values, so this variant works in full today. Every
            // error here points at the `[` that opened the index
            let object = evaluate(object, env)?;
            let index = evaluate(index, env)?;
            match (object, index) {
                (Value::List(items), Value::Number(n)) => {
                    if n.fract() != 0.0 || n < 0.0 {
                        return Err(RuntimeError {
                            line: bracket.line,
                            col: bracket.col,
                            message: format!("list indices must be whole numbers, got {n}"),
                        });
                    }
                    let i = n as usize;
                    items.get(i).cloned().ok_or(RuntimeError {
                        line: bracket.line,
                        col: bracket.col,
                        message: format!(
                            "index {i} is out of bounds, the list has {} elements",
                            items.len()
                        ),
                    })
                }
                (object, Value::Number(_)) => Err(RuntimeError {
                    line: bracket.line,
                    col: bracket.col,
                    message: format!(
                        "only lists can be indexed, got {}",
                        type_name(&object)
                    ),
                }),
                (object, index) => Err(RuntimeError {
                    line: bracket.line,
                    col: bracket.col,
                    message: format!(
                        "list indices must be numbers, got {} (indexing {})",
                        type_name(&index),
                        type_name(&object)
                    ),
                }),
            }
        }
    }
}

// The machinery behind the `Call` arm: push a scope of parameters onto
// the *caller's* environment, run the body, and unwrap whatever comes
// back. A body that falls off the end returns `nil`; a `return`
// statement surfaces here as `Flow::Return` and finally becomes the
// value it was always headed for.
//
// Note where the parameters live: a scope on the caller's stack, not a
// saved copy of the scopes that existed when `fun` ran. birch
// functions are not closures — a body sees the caller's variables,
// which is wrong in the general case. The fix is environments that
// outlive the block that made them, and that is a heap of shared
// maps, not this stack.
fn call(
    function: &FunctionValue,
    args: Vec<Value>,
    env: &mut Environment,
) -> Result<Value, RuntimeError> {
    env.scopes.push(HashMap::new());
    for (param, value) in function.params.iter().zip(args) {
        env.scopes.last_mut().unwrap().insert(param.clone(), value);
    }
    let mut result = Ok(Flow::Normal);
    for stmt in &function.body {
        match execute(stmt, env) {
            Ok(Flow::Normal) => {}
            other => {
                result = other;
                break;
            }
        }
    }
    env.scopes.pop();
    match result {
        Ok(Flow::Normal) => Ok(Value::Nil),
        Ok(Flow::Return(value)) => Ok(value),
        Err(e) => Err(e),
    }
}

fn execute(stmt: &Stmt, env: &mut Environment) -> Result<Flow, RuntimeError> {
    match stmt {
        // an expression statement evaluates and prints its value — the
        // `print` in read-eval-print, and the only output a bare
        // expression ever gives
        Stmt::Expression(expr) => {
            let value = evaluate(expr, env)?;
            println!("{value}");
            Ok(Flow::Normal)
        }
        // `if`, `while` and blocks need no state: their conditions and
        // bodies are plain subtrees, so they work in full today
        Stmt::If { condition, then_branch, else_branch } => {
            if evaluate(condition, env)?.is_truthy() {
                execute(then_branch, env)
            } else if let Some(else_branch) = else_branch {
                execute(else_branch, env)
            } else {
                Ok(Flow::Normal)
            }
        }
        Stmt::While { condition, body } => {
            while evaluate(condition, env)?.is_truthy() {
                execute(body, env)?;
            }
            Ok(Flow::Normal)
        }
        // a block is the one statement that changes the environment:
        // its body runs in a freshly pushed scope, which dies with it
        Stmt::Block(statements) => execute_scoped(statements, env),
        // `var` and `fun` both define: the first stores a computed
        // value, the second stores the declaration itself, wrapped as
        // a callable value
        Stmt::Var { name, init } => {
            // the initializer evaluates first, in the scope the name
            // is about to live in — so `var x = x;` reads the outer
            // `x`, the way most dynamic languages do
            let value = evaluate(init, env)?;
            env.define(name.clone(), value);
            Ok(Flow::Normal)
        }
        Stmt::Fun { name, params, body } => {
            // one check that needs no machinery: two parameters with
            // the same name could never be told apart
            let mut seen: Vec<&str> = Vec::new();
            for param in params {
                if seen.contains(&param.lexeme.as_str()) {
                    return Err(RuntimeError {
                        line: param.line,
                        col: param.col,
                        message: format!(
                            "duplicate parameter `{}` in `{name}`",
                            param.lexeme
                        ),
                    });
                }
                seen.push(&param.lexeme);
            }
            // defining is storing: the declaration the parser built,
            // wrapped in `Rc` so the `Value` can hold it without
            // moving the body on every clone. Recursion needs nothing
            // extra — the body only runs at call time, by which point
            // the name is findable in the caller's scopes
            env.define(
                name.clone(),
                Value::Function(Rc::new(FunctionValue {
                    name: name.clone(),
                    params: params.iter().map(|t| t.lexeme.clone()).collect(),
                    body: body.clone(),
                })),
            );
            Ok(Flow::Normal)
        }
        Stmt::Return { value } => {
            // the value is evaluated here, then handed upward as the
            // Ok half of the result: `return` is not an error, it is
            // an early way to finish. Nothing catches it until `call`,
            // which is why a top-level `return` still ends the script
            // with an error — the driver in `main` is where one lands
            let value = match value {
                Some(expr) => evaluate(expr, env)?,
                None => Value::Nil,
            };
            Ok(Flow::Return(value))
        }
    }
}

// A block is a scope: push a fresh map, run the body, pop it — even when
// a statement in the middle fails, so the caller's environment comes
// back exactly the way it went in. A `Flow::Return` walks straight
// through untouched: a block pops a scope, it does not catch a return.
fn execute_scoped(statements: &[Stmt], env: &mut Environment) -> Result<Flow, RuntimeError> {
    env.scopes.push(HashMap::new());
    let mut result = Ok(Flow::Normal);
    for statement in statements {
        match execute(statement, env) {
            Ok(Flow::Normal) => {}
            other => {
                result = other;
                break;
            }
        }
    }
    env.scopes.pop();
    result
}

// ------------------------------------------------------------- the driver

// The statements a successful parse produced, run to completion in the
// given environment. The first uncaught error prints and ends the
// batch — and everything defined before it stays defined, which is the
// behaviour both callers want.
fn run_statements(statements: &[Stmt], env: &mut Environment) {
    for statement in statements {
        match execute(statement, env) {
            Ok(Flow::Normal) => {}
            // a Flow::Return reaching the top has no call to land in —
            // same verdict as last step, and the one error with
            // nowhere to point: the verdict is issued here, past
            // every token
            Ok(Flow::Return(_)) => {
                println!("  runtime error: return outside of a function");
                return;
            }
            Err(e) => {
                println!("  runtime error: {}:{} {}", e.line, e.col, e.message);
                return;
            }
        }
    }
}

// One source, start to finish: scan, parse, run, with every error
// printed at the line:col address it has carried since step 2. This
// is the file half of the driver: `birch program.birch` reads the
// file and hands it here, one environment per file.
fn run(source: &str, env: &mut Environment) {
    let (tokens, errors) = Scanner::new(source).scan_tokens();
    if !errors.is_empty() {
        for e in &errors {
            println!("  scan error: {}:{} {}", e.line, e.col, e.message);
        }
        return;
    }
    match Parser::new(tokens).program() {
        Ok(statements) => run_statements(&statements, env),
        Err(e) => println!("  parse error: {}:{} {}", e.line, e.col, e.message),
    }
}

// The read-eval-print loop. One environment lives across the whole
// session — `var x = 1;` on one line is still visible ten lines later,
// the one thing a batch runner cannot offer. The buffer collects
// lines until the parser stops asking for more: input that ends in
// the middle of a declaration is not wrong, it is unfinished.
fn repl() {
    println!("birch — Ctrl-D to exit");
    let mut env = Environment::new();
    let mut pending = String::new();
    loop {
        if pending.is_empty() {
            print!(">>> ");
        } else {
            print!("... ");
        }
        // stdout is line-buffered: a prompt with no newline on it
        // sits in the buffer until something flushes it. Without this
        // line the user stares at a silent terminal.
        io::stdout().flush().unwrap();
        let mut line = String::new();
        // read_line reports 0 at end of input — Ctrl-D at a terminal,
        // a closed pipe anywhere else. That is the way out.
        if io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            println!();
            return;
        }
        pending.push_str(&line);
        let (tokens, errors) = Scanner::new(&pending).scan_tokens();
        if !errors.is_empty() {
            for e in &errors {
                println!("  scan error: {}:{} {}", e.line, e.col, e.message);
            }
            pending.clear();
            continue;
        }
        match Parser::new(tokens).program() {
            Ok(statements) => {
                pending.clear();
                run_statements(&statements, &mut env);
            }
            // the parse ran off the end of a half-typed declaration:
            // keep the buffer, print nothing, ask for the next line
            Err(e) if e.incomplete => {}
            Err(e) => {
                println!("  parse error: {}:{} {}", e.line, e.col, e.message);
                pending.clear();
            }
        }
    }
}

fn main() {
    // `birch program.birch` runs a file; no argument, the loop
    match std::env::args().nth(1) {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(source) => run(&source, &mut Environment::new()),
            Err(e) => println!("cannot read {path}: {e}"),
        },
        None => repl(),
    }
}
