//! Expression engine for synth column rules (issue #70).
//!
//! Parser + whitelist validation + exact `rust_decimal` evaluation for
//! `derive` expressions and branch predicates. Contract: see
//! `docs/plans/2026-09-15-synth-rules-v1-extension.md` sections 2 and 3
//! (validation V7-V10, AC2 of issue #70).
//!
//! # Grammar (frozen, §2.2)
//!
//! ```text
//! expr    := or
//! or      := and ( "||" and )*
//! and     := cmp ( "&&" cmp )*
//! cmp     := add ( ("==" | "!=" | "<=" | ">=" | "<" | ">") add )?
//! add     := mul ( ("+" | "-") mul )*
//! mul     := unary ( ("*" | "/" | "%") unary )*
//! unary   := "-" unary | primary
//! primary := number | string | column | "(" expr ")"
//! number  := decimal literal, parsed into `rust_decimal::Decimal`
//! string  := 'single quoted'
//! column  := identifier
//! ```
//!
//! # Security whitelist (issue #70 AC2, §3 V10)
//!
//! The parser rejects, at load time and before any evaluation, every node that
//! is outside the grammar above. In particular:
//!
//! * function calls such as `min(price, qty)`,
//! * attribute access such as `price.__class__`,
//! * subscripting such as `cols[0]`,
//! * any character outside the quoted/unquoted forms listed above.
//!
//! Each rejection names the offending node (`function call `min``, ...) so the
//! caller can surface a fail-fast diagnostic that points at the bad construct.
//!
//! # Semantics
//!
//! * Arithmetic is `Decimal` end to end: no `f64` round trip, so `0.1 * 3`
//!   compares equal to `0.3` exactly.
//! * Division or remainder by zero is an [`ExprError`], never a panic or `Inf`.
//! * Comparing with NULL (`serde_json::Value::Null`, or a column the lookup does
//!   not know) is always `false`; NULL propagates through arithmetic and turns
//!   into [`ExprError::NullResult`] if it reaches [`Expr::eval_decimal`].
//! * Comparing a number with a string is an [`ExprError::MixedTypeComparison`].
//! * `&&` and `||` require boolean operands and short-circuit.
//!
//! The module is pure: no I/O, no global state. Column values are supplied by
//! the caller through a lookup closure.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use rust_decimal::Decimal;
use serde_json::Value as JsonValue;

// ─── Errors ───

/// Everything that can go wrong while parsing, validating or evaluating an
/// expression. Every variant carries enough detail for a fail-fast load-time
/// diagnostic that names the offending node or column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExprError {
    /// Lexical or grammar violation, with the byte offset of the offender.
    Syntax { message: String, position: usize },
    /// A syntactically recognizable construct outside the whitelist.
    Disallowed {
        /// Node category, e.g. `"function call"`.
        construct: &'static str,
        /// The offending text, e.g. the callee or base identifier.
        detail: String,
    },
    /// A referenced column is not part of the declaring table.
    UnknownColumn { name: String },
    /// Division where the divisor evaluated to zero.
    DivisionByZero { op: &'static str },
    /// Remainder where the divisor evaluated to zero.
    RemainderByZero { op: &'static str },
    /// Operands of different types were compared.
    MixedTypeComparison {
        left: &'static str,
        right: &'static str,
    },
    /// An operator was applied to an operand of the wrong type.
    TypeMismatch {
        op: &'static str,
        kind: &'static str,
    },
    /// A predicate did not evaluate to a boolean.
    NonBooleanPredicate { found: &'static str },
    /// The expression evaluated to NULL and cannot be used as a decimal.
    NullResult,
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExprError::Syntax { message, position } => {
                write!(f, "syntax error at position {position}: {message}")
            }
            ExprError::Disallowed { construct, detail } => write!(
                f,
                "{construct} `{detail}` is not permitted in synth expressions"
            ),
            ExprError::UnknownColumn { name } => {
                write!(f, "unknown column `{name}` referenced in expression")
            }
            ExprError::DivisionByZero { op } => {
                write!(f, "division by zero in expression (operator `{op}`)")
            }
            ExprError::RemainderByZero { op } => {
                write!(f, "remainder by zero in expression (operator `{op}`)")
            }
            ExprError::MixedTypeComparison { left, right } => {
                write!(f, "cannot compare {left} with {right} in expression")
            }
            ExprError::TypeMismatch { op, kind } => {
                write!(f, "operator `{op}` cannot be applied to {kind}")
            }
            ExprError::NonBooleanPredicate { found } => {
                write!(f, "predicate must evaluate to a boolean, got {found}")
            }
            ExprError::NullResult => write!(f, "expression evaluated to NULL"),
        }
    }
}

impl std::error::Error for ExprError {}

// ─── Tokens ───

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(Decimal),
    Str(String),
    Ident(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Number(value) => write!(f, "{value}"),
            Token::Str(value) => write!(f, "'{value}'"),
            Token::Ident(name) => write!(f, "{name}"),
            Token::LParen => write!(f, "("),
            Token::RParen => write!(f, ")"),
            Token::LBracket => write!(f, "["),
            Token::RBracket => write!(f, "]"),
            Token::Comma => write!(f, ","),
            Token::Dot => write!(f, "."),
            Token::Plus => write!(f, "+"),
            Token::Minus => write!(f, "-"),
            Token::Star => write!(f, "*"),
            Token::Slash => write!(f, "/"),
            Token::Percent => write!(f, "%"),
            Token::EqEq => write!(f, "=="),
            Token::NotEq => write!(f, "!="),
            Token::Lt => write!(f, "<"),
            Token::Le => write!(f, "<="),
            Token::Gt => write!(f, ">"),
            Token::Ge => write!(f, ">="),
            Token::AndAnd => write!(f, "&&"),
            Token::OrOr => write!(f, "||"),
        }
    }
}

#[derive(Debug, Clone)]
struct Spanned {
    token: Token,
    position: usize,
}

fn syntax(message: impl Into<String>, position: usize) -> ExprError {
    ExprError::Syntax {
        message: message.into(),
        position,
    }
}

/// Split `src` into tokens. Every byte of the input must belong to a token or
/// be ASCII whitespace; anything else is a syntax error naming the character.
fn lex(src: &str) -> Result<Vec<Spanned>, ExprError> {
    let bytes = src.as_bytes();
    let mut out: Vec<Spanned> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let mut push = |token: Token| {
            out.push(Spanned {
                token,
                position: start,
            })
        };
        match byte {
            b'(' => {
                push(Token::LParen);
                i += 1;
            }
            b')' => {
                push(Token::RParen);
                i += 1;
            }
            b'[' => {
                push(Token::LBracket);
                i += 1;
            }
            b']' => {
                push(Token::RBracket);
                i += 1;
            }
            b',' => {
                push(Token::Comma);
                i += 1;
            }
            b'.' => {
                push(Token::Dot);
                i += 1;
            }
            b'+' => {
                push(Token::Plus);
                i += 1;
            }
            b'-' => {
                push(Token::Minus);
                i += 1;
            }
            b'*' => {
                push(Token::Star);
                i += 1;
            }
            b'/' => {
                push(Token::Slash);
                i += 1;
            }
            b'%' => {
                push(Token::Percent);
                i += 1;
            }
            b'\'' => {
                let rest = &src[i + 1..];
                let end = rest.find('\'').ok_or_else(|| {
                    syntax("unterminated string literal (missing closing `'`)", start)
                })?;
                push(Token::Str(rest[..end].to_string()));
                i += 1 + end + 1;
            }
            b'0'..=b'9' => {
                let mut j = i;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j + 1 < bytes.len() && bytes[j] == b'.' && bytes[j + 1].is_ascii_digit() {
                    j += 1;
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                }
                let text = &src[i..j];
                let value = Decimal::from_str(text)
                    .map_err(|_| syntax(format!("invalid numeric literal `{text}`"), start))?;
                push(Token::Number(value));
                i = j;
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let mut j = i;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                push(Token::Ident(src[i..j].to_string()));
                i = j;
            }
            b'=' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    push(Token::EqEq);
                    i += 2;
                } else {
                    return Err(syntax("unexpected `=`; use `==` for equality", start));
                }
            }
            b'!' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    push(Token::NotEq);
                    i += 2;
                } else {
                    return Err(syntax("unexpected `!`; use `!=` for inequality", start));
                }
            }
            b'<' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    push(Token::Le);
                    i += 2;
                } else {
                    push(Token::Lt);
                    i += 1;
                }
            }
            b'>' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    push(Token::Ge);
                    i += 2;
                } else {
                    push(Token::Gt);
                    i += 1;
                }
            }
            b'&' => {
                if bytes.get(i + 1) == Some(&b'&') {
                    push(Token::AndAnd);
                    i += 2;
                } else {
                    return Err(syntax("unexpected `&`; use `&&` for logical and", start));
                }
            }
            b'|' => {
                if bytes.get(i + 1) == Some(&b'|') {
                    push(Token::OrOr);
                    i += 2;
                } else {
                    return Err(syntax("unexpected `|`; use `||` for logical or", start));
                }
            }
            other => {
                return Err(syntax(
                    format!("unexpected character `{}`", other as char),
                    start,
                ));
            }
        }
    }
    Ok(out)
}

// ─── AST ───

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

impl ArithOp {
    fn symbol(self) -> &'static str {
        match self {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
            ArithOp::Div => "/",
            ArithOp::Rem => "%",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    fn symbol(self) -> &'static str {
        match self {
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }
}

/// Whitelisted AST. Each node is a construct the grammar explicitly allows;
/// there is no variant for calls, attribute access or subscripts, so those can
/// only ever fail during parsing.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    Number(Decimal),
    Str(String),
    Column(String),
    Negate(Box<Node>),
    Arith(ArithOp, Box<Node>, Box<Node>),
    Compare(CmpOp, Box<Node>, Box<Node>),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
}

// ─── Parser ───

struct Parser {
    tokens: Vec<Spanned>,
    pos: usize,
    end: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|spanned| &spanned.token)
    }

    fn position(&self) -> usize {
        self.tokens
            .get(self.pos)
            .map_or(self.end, |spanned| spanned.position)
    }

    fn advance(&mut self) -> Option<Spanned> {
        let spanned = self.tokens.get(self.pos).cloned();
        if spanned.is_some() {
            self.pos += 1;
        }
        spanned
    }

    fn eat(&mut self, want: &Token) -> bool {
        if self.peek() == Some(want) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, want: &Token, label: &str) -> Result<(), ExprError> {
        if self.eat(want) {
            return Ok(());
        }
        let position = self.position();
        match self.peek() {
            Some(found) => Err(syntax(
                format!("expected {label}, found `{found}`"),
                position,
            )),
            None => Err(syntax(
                format!("expected {label}, found end of expression"),
                position,
            )),
        }
    }

    /// Reject whitelist-violating constructs that only become obvious once the
    /// base of a suffix expression has been parsed (`min(...)`, `a.b`, `c[0]`).
    fn reject_suffix(&self, base: &str) -> Result<(), ExprError> {
        let construct = match self.peek() {
            Some(Token::LParen) => "function call",
            Some(Token::LBracket) => "subscript access",
            Some(Token::Dot) => "attribute access",
            _ => return Ok(()),
        };
        Err(ExprError::Disallowed {
            construct,
            detail: base.to_string(),
        })
    }

    fn parse_or(&mut self) -> Result<Node, ExprError> {
        let mut left = self.parse_and()?;
        while self.eat(&Token::OrOr) {
            let right = self.parse_and()?;
            left = Node::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Node, ExprError> {
        let mut left = self.parse_cmp()?;
        while self.eat(&Token::AndAnd) {
            let right = self.parse_cmp()?;
            left = Node::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_cmp(&mut self) -> Result<Node, ExprError> {
        let left = self.parse_add()?;
        let Some(op) = self.peek().and_then(cmp_op) else {
            return Ok(left);
        };
        self.pos += 1;
        let right = self.parse_add()?;
        Ok(Node::Compare(op, Box::new(left), Box::new(right)))
    }

    fn parse_add(&mut self) -> Result<Node, ExprError> {
        let mut left = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Some(Token::Plus) => ArithOp::Add,
                Some(Token::Minus) => ArithOp::Sub,
                _ => return Ok(left),
            };
            self.pos += 1;
            let right = self.parse_mul()?;
            left = Node::Arith(op, Box::new(left), Box::new(right));
        }
    }

    fn parse_mul(&mut self) -> Result<Node, ExprError> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Token::Star) => ArithOp::Mul,
                Some(Token::Slash) => ArithOp::Div,
                Some(Token::Percent) => ArithOp::Rem,
                _ => return Ok(left),
            };
            self.pos += 1;
            let right = self.parse_unary()?;
            left = Node::Arith(op, Box::new(left), Box::new(right));
        }
    }

    fn parse_unary(&mut self) -> Result<Node, ExprError> {
        if self.eat(&Token::Minus) {
            let inner = self.parse_unary()?;
            return Ok(Node::Negate(Box::new(inner)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Node, ExprError> {
        let Some(spanned) = self.advance() else {
            return Err(syntax(
                "unexpected end of expression; expected a value, column, number, string, or `(...)`",
                self.end,
            ));
        };
        match spanned.token {
            Token::Number(value) => Ok(Node::Number(value)),
            Token::Str(value) => Ok(Node::Str(value)),
            Token::Ident(name) => {
                self.reject_suffix(&name)?;
                Ok(Node::Column(name))
            }
            Token::LParen => {
                let inner = self.parse_or()?;
                self.expect(&Token::RParen, "`)` to close the group")?;
                self.reject_suffix("(...)")?;
                Ok(inner)
            }
            found => Err(syntax(
                format!(
                    "unexpected token `{found}`; expected a value, column, number, string, or `(...)`"
                ),
                spanned.position,
            )),
        }
    }
}

fn cmp_op(token: &Token) -> Option<CmpOp> {
    match token {
        Token::EqEq => Some(CmpOp::Eq),
        Token::NotEq => Some(CmpOp::Ne),
        Token::Lt => Some(CmpOp::Lt),
        Token::Le => Some(CmpOp::Le),
        Token::Gt => Some(CmpOp::Gt),
        Token::Ge => Some(CmpOp::Ge),
        _ => None,
    }
}

fn parse_node(src: &str) -> Result<Node, ExprError> {
    let tokens = lex(src)?;
    let mut parser = Parser {
        tokens,
        pos: 0,
        end: src.len(),
    };
    let node = parser.parse_or()?;
    if let Some(spanned) = parser.tokens.get(parser.pos) {
        return Err(syntax(
            format!(
                "unexpected trailing token `{}`; expression must end here",
                spanned.token
            ),
            spanned.position,
        ));
    }
    Ok(node)
}

// ─── Evaluation ───

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Null,
    Bool(bool),
    Number(Decimal),
    Str(String),
}

impl Value {
    fn kind(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::Str(_) => "string",
        }
    }

    fn from_json(value: &JsonValue) -> Result<Value, ExprError> {
        match value {
            JsonValue::Null => Ok(Value::Null),
            JsonValue::Bool(flag) => Ok(Value::Bool(*flag)),
            // `Number::to_string` is the exact literal text, so no f64 round
            // trip can introduce a binary tail.
            JsonValue::Number(number) => Decimal::from_str(&number.to_string())
                .map(Value::Number)
                .map_err(|_| ExprError::TypeMismatch {
                    op: "coerce column value",
                    kind: "number",
                }),
            JsonValue::String(text) => Ok(Value::Str(text.clone())),
            JsonValue::Array(_) => Err(ExprError::TypeMismatch {
                op: "coerce column value",
                kind: "array",
            }),
            JsonValue::Object(_) => Err(ExprError::TypeMismatch {
                op: "coerce column value",
                kind: "object",
            }),
        }
    }
}

fn eval(node: &Node, lookup: &dyn Fn(&str) -> Option<JsonValue>) -> Result<Value, ExprError> {
    match node {
        Node::Number(value) => Ok(Value::Number(*value)),
        Node::Str(text) => Ok(Value::Str(text.clone())),
        Node::Column(name) => match lookup(name) {
            Some(value) => Value::from_json(&value),
            // A column the row lookup does not know behaves like SQL NULL.
            None => Ok(Value::Null),
        },
        Node::Negate(inner) => match eval(inner, lookup)? {
            Value::Number(value) => Ok(Value::Number(-value)),
            Value::Null => Ok(Value::Null),
            other => Err(ExprError::TypeMismatch {
                op: "negate",
                kind: other.kind(),
            }),
        },
        Node::Arith(op, left, right) => {
            let left = eval(left, lookup)?;
            let right = eval(right, lookup)?;
            arith(*op, left, right)
        }
        Node::Compare(op, left, right) => {
            let left = eval(left, lookup)?;
            let right = eval(right, lookup)?;
            compare(*op, left, right)
        }
        Node::And(left, right) => match eval(left, lookup)? {
            Value::Bool(false) => Ok(Value::Bool(false)),
            Value::Bool(true) => match eval(right, lookup)? {
                Value::Bool(flag) => Ok(Value::Bool(flag)),
                other => Err(ExprError::NonBooleanPredicate {
                    found: other.kind(),
                }),
            },
            other => Err(ExprError::NonBooleanPredicate {
                found: other.kind(),
            }),
        },
        Node::Or(left, right) => match eval(left, lookup)? {
            Value::Bool(true) => Ok(Value::Bool(true)),
            Value::Bool(false) => match eval(right, lookup)? {
                Value::Bool(flag) => Ok(Value::Bool(flag)),
                other => Err(ExprError::NonBooleanPredicate {
                    found: other.kind(),
                }),
            },
            other => Err(ExprError::NonBooleanPredicate {
                found: other.kind(),
            }),
        },
    }
}

fn arith(op: ArithOp, left: Value, right: Value) -> Result<Value, ExprError> {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => match op {
            ArithOp::Add => Ok(Value::Number(a + b)),
            ArithOp::Sub => Ok(Value::Number(a - b)),
            ArithOp::Mul => Ok(Value::Number(a * b)),
            ArithOp::Div => a
                .checked_div(b)
                .map(Value::Number)
                .ok_or(ExprError::DivisionByZero { op: op.symbol() }),
            ArithOp::Rem => a
                .checked_rem(b)
                .map(Value::Number)
                .ok_or(ExprError::RemainderByZero { op: op.symbol() }),
        },
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (left, right) => Err(ExprError::TypeMismatch {
            op: op.symbol(),
            kind: if matches!(left, Value::Number(_)) {
                right.kind()
            } else {
                left.kind()
            },
        }),
    }
}

fn compare(op: CmpOp, left: Value, right: Value) -> Result<Value, ExprError> {
    if matches!((&left, &right), (Value::Null, _) | (_, Value::Null)) {
        return Ok(Value::Bool(false));
    }
    let ordering = match (&left, &right) {
        (Value::Number(a), Value::Number(b)) => a.cmp(b),
        (Value::Str(a), Value::Str(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => match op {
            CmpOp::Eq => return Ok(Value::Bool(a == b)),
            CmpOp::Ne => return Ok(Value::Bool(a != b)),
            _ => {
                return Err(ExprError::TypeMismatch {
                    op: op.symbol(),
                    kind: "boolean",
                })
            }
        },
        _ => {
            return Err(ExprError::MixedTypeComparison {
                left: left.kind(),
                right: right.kind(),
            })
        }
    };
    let result = match op {
        CmpOp::Eq => ordering == Ordering::Equal,
        CmpOp::Ne => ordering != Ordering::Equal,
        CmpOp::Lt => ordering == Ordering::Less,
        CmpOp::Le => ordering != Ordering::Greater,
        CmpOp::Gt => ordering == Ordering::Greater,
        CmpOp::Ge => ordering != Ordering::Less,
    };
    Ok(Value::Bool(result))
}

fn collect_columns(node: &Node, out: &mut BTreeSet<String>) {
    match node {
        Node::Column(name) => {
            out.insert(name.clone());
        }
        Node::Negate(inner) => collect_columns(inner, out),
        Node::Arith(_, left, right)
        | Node::Compare(_, left, right)
        | Node::And(left, right)
        | Node::Or(left, right) => {
            collect_columns(left, out);
            collect_columns(right, out);
        }
        Node::Number(_) | Node::Str(_) => {}
    }
}

// ─── Public API ───

/// A parsed, whitelist-checked expression.
#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    root: Node,
}

impl Expr {
    /// Parse and whitelist-check `src`. Does not touch column names; call
    /// [`Expr::check_columns`] for the load-time existence check.
    pub fn parse(src: &str) -> Result<Expr, ExprError> {
        parse_node(src).map(|root| Expr { root })
    }

    /// Fail-fast load-time validation: every referenced column must be declared
    /// in `known`. The error names the first missing column (in deterministic
    /// lexicographic order).
    pub fn check_columns(&self, known: &BTreeSet<String>) -> Result<(), ExprError> {
        for name in self.referenced_columns() {
            if !known.contains(&name) {
                return Err(ExprError::UnknownColumn { name });
            }
        }
        Ok(())
    }

    /// Columns referenced by this expression, in deterministic order.
    pub fn referenced_columns(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        collect_columns(&self.root, &mut out);
        out
    }

    /// Evaluate as a predicate. The result must be a boolean; a NULL comparison
    /// inside the expression is `false`.
    pub fn eval_bool(&self, lookup: &dyn Fn(&str) -> Option<JsonValue>) -> Result<bool, ExprError> {
        match eval(&self.root, lookup)? {
            Value::Bool(flag) => Ok(flag),
            other => Err(ExprError::NonBooleanPredicate {
                found: other.kind(),
            }),
        }
    }

    /// Evaluate as an exact decimal, for `derive` targets.
    pub fn eval_decimal(
        &self,
        lookup: &dyn Fn(&str) -> Option<JsonValue>,
    ) -> Result<Decimal, ExprError> {
        match eval(&self.root, lookup)? {
            Value::Number(value) => Ok(value),
            Value::Null => Err(ExprError::NullResult),
            other => Err(ExprError::TypeMismatch {
                op: "decimal result",
                kind: other.kind(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;
    use std::str::FromStr;

    use serde_json::json;
    use serde_json::Value as Json;

    fn cols(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn row<'a>(pairs: &'a [(&'a str, Json)]) -> impl Fn(&str) -> Option<Json> + 'a {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        }
    }

    fn dec(text: &str) -> Decimal {
        Decimal::from_str(text).expect("literal must parse as decimal")
    }

    fn eval_decimal(src: &str, pairs: &[(&str, Json)]) -> Result<Decimal, ExprError> {
        Expr::parse(src)
            .expect("expression must parse")
            .eval_decimal(&row(pairs))
    }

    fn eval_bool(src: &str, pairs: &[(&str, Json)]) -> Result<bool, ExprError> {
        Expr::parse(src)
            .expect("expression must parse")
            .eval_bool(&row(pairs))
    }

    #[test]
    fn should_evaluate_decimal_without_binary_tail() {
        assert_eq!(eval_decimal("0.1 * 3", &[]).expect("eval"), dec("0.3"));
        assert_ne!(
            eval_decimal("0.1 * 3", &[]).expect("eval"),
            dec("0.30000004")
        );
        assert_eq!(
            eval_decimal("price * 1.005", &[("price", json!(1000))]).expect("eval"),
            dec("1005.000")
        );
    }

    #[test]
    fn should_reject_function_call_node() {
        let err = Expr::parse("min(price, qty)").expect_err("must reject");
        match &err {
            ExprError::Disallowed { construct, detail } => {
                assert_eq!(*construct, "function call");
                assert_eq!(detail, "min");
            }
            other => panic!("expected disallowed node, got {other:?}"),
        }
        assert!(err.to_string().contains("min"), "message: {err}");
    }

    #[test]
    fn should_reject_attribute_access_node() {
        let err = Expr::parse("price.__class__").expect_err("must reject");
        match &err {
            ExprError::Disallowed { construct, detail } => {
                assert_eq!(*construct, "attribute access");
                assert_eq!(detail, "price");
            }
            other => panic!("expected disallowed node, got {other:?}"),
        }
        assert!(err.to_string().contains("price"), "message: {err}");
    }

    #[test]
    fn should_reject_empty_parenthesized_attribute_access() {
        let err = Expr::parse("().__class__").expect_err("must reject");
        assert!(
            matches!(err, ExprError::Syntax { .. }),
            "expected syntax error, got {err:?}"
        );
    }

    #[test]
    fn should_reject_subscript_node() {
        let err = Expr::parse("cols[0]").expect_err("must reject");
        match &err {
            ExprError::Disallowed { construct, detail } => {
                assert_eq!(*construct, "subscript access");
                assert_eq!(detail, "cols");
            }
            other => panic!("expected disallowed node, got {other:?}"),
        }
        assert!(err.to_string().contains("cols"), "message: {err}");
    }

    #[test]
    fn should_reject_unknown_column_at_load_time() {
        let expr = Expr::parse("ghost + 1").expect("parse");
        let err = expr
            .check_columns(&cols(&["price", "qty"]))
            .expect_err("unknown column must fail validation");
        match &err {
            ExprError::UnknownColumn { name } => assert_eq!(name, "ghost"),
            other => panic!("expected unknown column, got {other:?}"),
        }
        assert!(err.to_string().contains("ghost"), "message: {err}");
        let ok = Expr::parse("price * qty").expect("parse");
        assert!(ok.check_columns(&cols(&["price", "qty"])).is_ok());
    }

    #[test]
    fn should_report_referenced_columns() {
        let expr = Expr::parse("price * qty + price").expect("parse");
        assert_eq!(
            expr.referenced_columns(),
            cols(&["price", "qty"]),
            "duplicates collapse into one entry"
        );
    }

    #[test]
    fn should_report_division_by_zero_as_error() {
        let err = eval_decimal("price / qty", &[("price", json!(10)), ("qty", json!(0))])
            .expect_err("division by zero must be an error, not a panic");
        assert!(matches!(err, ExprError::DivisionByZero { .. }));
        assert!(
            err.to_string().contains("division by zero"),
            "message: {err}"
        );
        let err = eval_decimal("price % qty", &[("price", json!(10)), ("qty", json!(0))])
            .expect_err("modulo by zero must be an error");
        assert!(matches!(err, ExprError::RemainderByZero { .. }));
        assert!(
            err.to_string().contains("remainder by zero"),
            "message: {err}"
        );
    }

    #[test]
    fn should_treat_null_comparison_as_false() {
        assert!(!eval_bool("qty > 5", &[("qty", json!(null))]).expect("eval"));
        assert!(!eval_bool("qty == 5", &[("qty", json!(null))]).expect("eval"));
        assert!(!eval_bool("qty != 5", &[("qty", json!(null))]).expect("eval"));
        assert!(!eval_bool("status == 'A'", &[("status", json!(null))]).expect("eval"));
        // A column missing from the row lookup behaves like SQL NULL.
        assert!(!eval_bool("qty == 5", &[]).expect("eval"));
    }

    #[test]
    fn should_reject_mixed_type_comparison() {
        let err = eval_bool("name == 1", &[("name", json!("abc"))]).expect_err("must reject");
        match &err {
            ExprError::MixedTypeComparison { left, right } => {
                assert_eq!(*left, "string");
                assert_eq!(*right, "number");
            }
            other => panic!("expected mixed type comparison, got {other:?}"),
        }
        assert!(err.to_string().contains("string"), "message: {err}");
        assert!(err.to_string().contains("number"), "message: {err}");
    }

    #[test]
    fn should_honor_operator_precedence() {
        assert_eq!(eval_decimal("1 + 2 * 3", &[]).expect("eval"), dec("7"));
        assert_eq!(eval_decimal("(1 + 2) * 3", &[]).expect("eval"), dec("9"));
        assert_eq!(eval_decimal("10 - 2 - 3", &[]).expect("eval"), dec("5"));
        assert_eq!(eval_decimal("-2 * 3", &[]).expect("eval"), dec("-6"));
        assert_eq!(eval_decimal("8 % 3", &[]).expect("eval"), dec("2"));
        assert_eq!(
            eval_decimal(
                "price - qty * 2",
                &[("price", json!(10)), ("qty", json!(3))]
            )
            .expect("eval"),
            dec("4")
        );
    }

    #[test]
    fn should_evaluate_logical_operators_for_predicates() {
        let pairs = [("status", json!("A")), ("qty", json!(5))];
        assert!(eval_bool("status == 'A' && qty > 2", &pairs).expect("eval"));
        assert!(eval_bool("status == 'A' || qty > 100", &pairs).expect("eval"));
        assert!(!eval_bool("status == 'A' && qty > 100", &pairs).expect("eval"));
        assert!(!eval_bool("status != 'A'", &pairs).expect("eval"));
        // "&&" binds tighter than "||".
        assert!(eval_bool("status == 'A' || status == 'B' && qty > 100", &pairs).expect("eval"));
        assert!(!eval_bool("status == 'B' || status == 'C' && qty > 100", &pairs).expect("eval"));
    }

    #[test]
    fn should_propagate_null_through_arithmetic() {
        let err = eval_decimal("price * qty", &[("price", json!(2)), ("qty", json!(null))])
            .expect_err("NULL result cannot be a decimal");
        assert!(matches!(err, ExprError::NullResult));
        // Inside a comparison, the propagated NULL makes the predicate false.
        assert!(!eval_bool(
            "price * qty == 0",
            &[("price", json!(2)), ("qty", json!(null))]
        )
        .expect("eval"));
    }

    #[test]
    fn should_reject_arithmetic_on_non_numbers() {
        let err = eval_decimal("name + 1", &[("name", json!("abc"))]).expect_err("must reject");
        match &err {
            ExprError::TypeMismatch { op, kind } => {
                assert_eq!(*op, "+");
                assert_eq!(*kind, "string");
            }
            other => panic!("expected type mismatch, got {other:?}"),
        }
        let err = eval_decimal("-'abc'", &[]).expect_err("must reject");
        assert!(matches!(err, ExprError::TypeMismatch { op: "negate", .. }));
    }

    #[test]
    fn should_reject_non_boolean_predicate() {
        let err = eval_bool("qty + 1", &[("qty", json!(1))]).expect_err("must reject");
        match &err {
            ExprError::NonBooleanPredicate { found } => assert_eq!(*found, "number"),
            other => panic!("expected non boolean predicate, got {other:?}"),
        }
        let err = eval_bool("status && true", &[("status", json!("A"))]).expect_err("must reject");
        assert!(matches!(err, ExprError::NonBooleanPredicate { .. }));
    }

    #[test]
    fn should_report_syntax_errors_with_position() {
        let err = Expr::parse("1 + 2 foo").expect_err("trailing token must be rejected");
        match &err {
            ExprError::Syntax { position, .. } => assert_eq!(*position, 6),
            other => panic!("expected syntax error, got {other:?}"),
        }
        let err = Expr::parse("name == 'abc").expect_err("unterminated string");
        assert!(err.to_string().contains("unterminated"), "message: {err}");
        let err = Expr::parse("").expect_err("empty expression");
        assert!(
            err.to_string().contains("end of expression"),
            "message: {err}"
        );
        let err = Expr::parse("price = 1").expect_err("single = is not a comparison");
        assert!(err.to_string().contains("=="), "message: {err}");
    }
}
