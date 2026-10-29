pub mod eval;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::OnceLock;

use serde_json::Value;

use crate::codec::AwsError;

use super::attr::Attr;
use super::validation;

const MAX_DEPTH: usize = 32;

fn is_reserved(word: &str) -> bool {
    static WORDS: OnceLock<HashSet<&'static str>> = OnceLock::new();
    WORDS
        .get_or_init(|| include_str!("reserved.txt").lines().collect())
        .contains(word.to_ascii_uppercase().as_str())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Field(String),
    Index(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Path {
    pub root: String,
    pub steps: Vec<Step>,
}

impl Path {
    pub fn top(name: &str) -> Self {
        Self {
            root: name.to_string(),
            steps: Vec::new(),
        }
    }

    pub fn overlaps(&self, other: &Self) -> bool {
        self.root == other.root && self.steps.iter().zip(&other.steps).all(|(a, b)| a == b)
    }

    pub fn describe(&self) -> String {
        let mut parts = vec![self.root.clone()];
        for s in &self.steps {
            parts.push(match s {
                Step::Field(f) => f.clone(),
                Step::Index(i) => format!("[{i}]"),
            });
        }
        format!("[{}]", parts.join(", "))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone)]
pub enum Operand {
    Path(Path),
    Value(Attr),
    Size(Path),
}

#[derive(Debug, Clone)]
pub enum Cond {
    Cmp(CmpOp, Operand, Operand),
    Between(Operand, Operand, Operand),
    In(Operand, Vec<Operand>),
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
    Not(Box<Cond>),
    Exists(Path),
    NotExists(Path),
    Type(Path, String),
    BeginsWith(Path, Operand),
    Contains(Path, Operand),
}

#[derive(Debug, Clone)]
pub enum SetOperand {
    Path(Path),
    Value(Attr),
    IfNotExists(Path, Box<SetOperand>),
    ListAppend(Box<SetOperand>, Box<SetOperand>),
}

#[derive(Debug, Clone)]
pub enum SetValue {
    Operand(SetOperand),
    Plus(SetOperand, SetOperand),
    Minus(SetOperand, SetOperand),
}

#[derive(Debug, Clone, Default)]
pub struct Update {
    pub set: Vec<(Path, SetValue)>,
    pub remove: Vec<Path>,
    pub add: Vec<(Path, Attr)>,
    pub delete: Vec<(Path, Attr)>,
}

impl Update {
    pub fn targets(&self) -> Vec<&Path> {
        self.set
            .iter()
            .map(|(p, _)| p)
            .chain(&self.remove)
            .chain(self.add.iter().map(|(p, _)| p))
            .chain(self.delete.iter().map(|(p, _)| p))
            .collect()
    }
}

#[derive(Debug, Clone)]
pub enum RangeCond {
    Cmp(CmpOp, Attr),
    Between(Attr, Attr),
    BeginsWith(Attr),
}

#[derive(Debug, Clone)]
pub struct KeyCond {
    pub hash: (String, Attr),
    pub range: Option<(String, RangeCond)>,
}

#[derive(Debug, Default)]
pub struct Bindings {
    names: BTreeMap<String, String>,
    values: BTreeMap<String, Attr>,
    used_names: BTreeSet<String>,
    used_values: BTreeSet<String>,
    pub allow_reserved: bool,
}

impl Bindings {
    pub fn from_input(input: &Value) -> Result<Self, AwsError> {
        let mut b = Self::default();
        if let Some(names) = input
            .get("ExpressionAttributeNames")
            .and_then(Value::as_object)
        {
            if names.is_empty() {
                return Err(validation("ExpressionAttributeNames must not be empty"));
            }
            for (k, v) in names {
                if !k.starts_with('#') || k.len() < 2 {
                    return Err(validation(format!(
                        "ExpressionAttributeNames contains invalid key: Syntax error; key: \"{k}\""
                    )));
                }
                let v = v.as_str().unwrap_or_default();
                if v.is_empty() {
                    return Err(validation(format!(
                        "ExpressionAttributeNames contains invalid value: Empty attribute name for key {k}"
                    )));
                }
                b.names.insert(k.clone(), v.to_string());
            }
        }
        if let Some(values) = input
            .get("ExpressionAttributeValues")
            .and_then(Value::as_object)
        {
            if values.is_empty() {
                return Err(validation("ExpressionAttributeValues must not be empty"));
            }
            for (k, v) in values {
                if !k.starts_with(':') || k.len() < 2 {
                    return Err(validation(format!(
                        "ExpressionAttributeValues contains invalid key: Syntax error; key: \"{k}\""
                    )));
                }
                let attr = Attr::from_value(v).map_err(|e| {
                    validation(format!(
                        "ExpressionAttributeValues contains invalid value: {} for key {k}",
                        e.message
                    ))
                })?;
                b.values.insert(k.clone(), attr);
            }
        }
        Ok(b)
    }

    pub fn check_unused(&self) -> Result<(), AwsError> {
        let unused_names: Vec<&str> = self
            .names
            .keys()
            .filter(|k| !self.used_names.contains(*k))
            .map(String::as_str)
            .collect();
        if !unused_names.is_empty() {
            return Err(validation(format!(
                "Value provided in ExpressionAttributeNames unused in expressions: keys: {{{}}}",
                unused_names.join(", ")
            )));
        }
        let unused_values: Vec<&str> = self
            .values
            .keys()
            .filter(|k| !self.used_values.contains(*k))
            .map(String::as_str)
            .collect();
        if !unused_values.is_empty() {
            return Err(validation(format!(
                "Value provided in ExpressionAttributeValues unused in expressions: keys: {{{}}}",
                unused_values.join(", ")
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Ident(String),
    Name(String),
    Value(String),
    Int(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Cmp(CmpOp),
    Plus,
    Minus,
    Kw(Kw),
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kw {
    Set,
    Remove,
    Add,
    Delete,
    Between,
    In,
    And,
    Or,
    Not,
}

impl Tok {
    fn text(&self) -> String {
        match self {
            Tok::Ident(s) | Tok::Name(s) | Tok::Value(s) | Tok::Int(s) => s.clone(),
            Tok::LParen => "(".into(),
            Tok::RParen => ")".into(),
            Tok::LBracket => "[".into(),
            Tok::RBracket => "]".into(),
            Tok::Comma => ",".into(),
            Tok::Dot => ".".into(),
            Tok::Plus => "+".into(),
            Tok::Minus => "-".into(),
            Tok::Cmp(op) => match op {
                CmpOp::Eq => "=",
                CmpOp::Ne => "<>",
                CmpOp::Lt => "<",
                CmpOp::Le => "<=",
                CmpOp::Gt => ">",
                CmpOp::Ge => ">=",
            }
            .into(),
            Tok::Kw(k) => format!("{k:?}").to_ascii_uppercase(),
            Tok::Eof => "<EOF>".into(),
        }
    }
}

fn keyword(word: &str) -> Option<Kw> {
    Some(match word.to_ascii_uppercase().as_str() {
        "SET" => Kw::Set,
        "REMOVE" => Kw::Remove,
        "ADD" => Kw::Add,
        "DELETE" => Kw::Delete,
        "BETWEEN" => Kw::Between,
        "IN" => Kw::In,
        "AND" => Kw::And,
        "OR" => Kw::Or,
        "NOT" => Kw::Not,
        _ => return None,
    })
}

pub fn tokenize(kind: &str, input: &str) -> Result<Vec<Tok>, AwsError> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let word = |start: usize| {
        let mut j = start;
        while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
            j += 1;
        }
        j
    };
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let single = match c {
            '(' => Some(Tok::LParen),
            ')' => Some(Tok::RParen),
            '[' => Some(Tok::LBracket),
            ']' => Some(Tok::RBracket),
            ',' => Some(Tok::Comma),
            '.' => Some(Tok::Dot),
            '+' => Some(Tok::Plus),
            '-' => Some(Tok::Minus),
            '=' => Some(Tok::Cmp(CmpOp::Eq)),
            _ => None,
        };
        if let Some(t) = single {
            out.push(t);
            i += 1;
            continue;
        }
        match c {
            '<' | '>' => {
                let next = chars.get(i + 1).copied();
                let (op, len) = match (c, next) {
                    ('<', Some('=')) => (CmpOp::Le, 2),
                    ('<', Some('>')) => (CmpOp::Ne, 2),
                    ('<', _) => (CmpOp::Lt, 1),
                    (_, Some('=')) => (CmpOp::Ge, 2),
                    _ => (CmpOp::Gt, 1),
                };
                out.push(Tok::Cmp(op));
                i += len;
            }
            '#' | ':' => {
                let j = word(i + 1);
                let text: String = chars[i..j].iter().collect();
                if j == i + 1 {
                    return Err(validation(format!(
                        "Invalid {kind}: Syntax error; token: \"{text}\", near: \"{text}\""
                    )));
                }
                out.push(if c == '#' {
                    Tok::Name(text)
                } else {
                    Tok::Value(text)
                });
                i = j;
            }
            c if c.is_ascii_digit() => {
                let mut j = i;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                out.push(Tok::Int(chars[i..j].iter().collect()));
                i = j;
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let j = word(i);
                let text: String = chars[i..j].iter().collect();
                out.push(keyword(&text).map_or(Tok::Ident(text), Tok::Kw));
                i = j;
            }
            other => {
                return Err(validation(format!(
                    "Invalid {kind}: Syntax error; token: \"{other}\", near: \"{other}\""
                )));
            }
        }
    }
    out.push(Tok::Eof);
    Ok(out)
}

pub struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    kind: &'static str,
    b: &'a mut Bindings,
}

const CONDITION_FUNCTIONS: &[&str] = &[
    "attribute_exists",
    "attribute_not_exists",
    "attribute_type",
    "begins_with",
    "contains",
];
const TYPE_NAMES: &[&str] = &["S", "SS", "N", "NS", "B", "BS", "BOOL", "NULL", "L", "M"];

impl<'a> Parser<'a> {
    pub fn new(kind: &'static str, input: &str, b: &'a mut Bindings) -> Result<Self, AwsError> {
        Ok(Self::from_tokens(kind, tokenize(kind, input)?, b))
    }

    pub fn from_tokens(kind: &'static str, toks: Vec<Tok>, b: &'a mut Bindings) -> Self {
        Self {
            toks,
            pos: 0,
            kind,
            b,
        }
    }

    fn peek(&self) -> &Tok {
        self.toks.get(self.pos).unwrap_or(&Tok::Eof)
    }

    fn peek_at(&self, n: usize) -> &Tok {
        self.toks.get(self.pos + n).unwrap_or(&Tok::Eof)
    }

    fn next(&mut self) -> Tok {
        let t = self.peek().clone();
        if t != Tok::Eof {
            self.pos += 1;
        }
        t
    }

    pub fn error(&self, message: impl std::fmt::Display) -> AwsError {
        validation(format!("Invalid {}: {message}", self.kind))
    }

    fn syntax(&self) -> AwsError {
        let token = self.peek().text();
        let near = match self.pos.checked_sub(1).and_then(|p| self.toks.get(p)) {
            Some(prev) if *self.peek() != Tok::Eof => format!("{} {token}", prev.text()),
            Some(prev) => prev.text(),
            None => token.clone(),
        };
        self.error(format!(
            "Syntax error; token: \"{token}\", near: \"{near}\""
        ))
    }

    fn expect(&mut self, t: Tok) -> Result<(), AwsError> {
        if *self.peek() == t {
            self.next();
            Ok(())
        } else {
            Err(self.syntax())
        }
    }

    pub fn expect_end(&self) -> Result<(), AwsError> {
        if *self.peek() == Tok::Eof {
            Ok(())
        } else {
            Err(self.syntax())
        }
    }

    fn name(&mut self, tok: &Tok) -> Result<String, AwsError> {
        match tok {
            Tok::Name(n) => {
                let resolved = self.b.names.get(n).cloned().ok_or_else(|| {
                    self.error(format!(
                        "An expression attribute name used in the document path is not defined; attribute name: {n}"
                    ))
                })?;
                self.b.used_names.insert(n.clone());
                Ok(resolved)
            }
            Tok::Ident(w) => {
                if !self.b.allow_reserved && is_reserved(w) {
                    return Err(self.error(format!(
                        "Attribute name is a reserved keyword; reserved keyword: {w}"
                    )));
                }
                Ok(w.clone())
            }
            _ => Err(self.syntax()),
        }
    }

    fn value(&mut self) -> Result<Attr, AwsError> {
        let Tok::Value(v) = self.peek().clone() else {
            return Err(self.syntax());
        };
        self.next();
        let attr = self.b.values.get(&v).cloned().ok_or_else(|| {
            self.error(format!(
                "An expression attribute value used in expression is not defined; attribute value: {v}"
            ))
        })?;
        self.b.used_values.insert(v);
        Ok(attr)
    }

    pub fn path(&mut self) -> Result<Path, AwsError> {
        let head = self.peek().clone();
        if !matches!(head, Tok::Ident(_) | Tok::Name(_)) {
            return Err(self.syntax());
        }
        self.next();
        let root = self.name(&head)?;
        let mut steps = Vec::new();
        loop {
            match self.peek() {
                Tok::Dot => {
                    self.next();
                    let member = self.peek().clone();
                    if !matches!(member, Tok::Ident(_) | Tok::Name(_)) {
                        return Err(self.syntax());
                    }
                    self.next();
                    steps.push(Step::Field(self.name(&member)?));
                }
                Tok::LBracket => {
                    self.next();
                    let Tok::Int(n) = self.peek().clone() else {
                        return Err(self.syntax());
                    };
                    self.next();
                    self.expect(Tok::RBracket)?;
                    let index = n.parse().map_err(|_| self.syntax())?;
                    steps.push(Step::Index(index));
                }
                _ => break,
            }
            if steps.len() + 1 > MAX_DEPTH {
                return Err(
                    self.error("The document path has too many nesting levels; nesting levels: 33")
                );
            }
        }
        Ok(Path { root, steps })
    }

    fn operand(&mut self) -> Result<Operand, AwsError> {
        match self.peek().clone() {
            Tok::Value(_) => Ok(Operand::Value(self.value()?)),
            Tok::Ident(f) if *self.peek_at(1) == Tok::LParen => {
                if f != "size" {
                    return Err(self.error(format!("Invalid function name; function: {f}")));
                }
                self.next();
                self.next();
                let p = self.path()?;
                self.expect(Tok::RParen)?;
                Ok(Operand::Size(p))
            }
            Tok::Ident(_) | Tok::Name(_) => Ok(Operand::Path(self.path()?)),
            _ => Err(self.syntax()),
        }
    }

    pub fn condition(&mut self) -> Result<Cond, AwsError> {
        let mut left = self.and()?;
        while *self.peek() == Tok::Kw(Kw::Or) {
            self.next();
            left = Cond::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Cond, AwsError> {
        let mut left = self.not()?;
        while *self.peek() == Tok::Kw(Kw::And) {
            self.next();
            left = Cond::And(Box::new(left), Box::new(self.not()?));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Cond, AwsError> {
        if *self.peek() == Tok::Kw(Kw::Not) {
            self.next();
            return Ok(Cond::Not(Box::new(self.not()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Cond, AwsError> {
        if *self.peek() == Tok::LParen {
            self.next();
            let inner = self.condition()?;
            self.expect(Tok::RParen)?;
            return Ok(inner);
        }
        if let Tok::Ident(f) = self.peek().clone()
            && *self.peek_at(1) == Tok::LParen
            && CONDITION_FUNCTIONS.contains(&f.as_str())
        {
            return self.function(&f);
        }
        let left = self.operand()?;
        match self.peek().clone() {
            Tok::Cmp(op) => {
                self.next();
                Ok(Cond::Cmp(op, left, self.operand()?))
            }
            Tok::Kw(Kw::Between) => {
                self.next();
                let low = self.operand()?;
                self.expect(Tok::Kw(Kw::And))?;
                let high = self.operand()?;
                if let (Operand::Value(l), Operand::Value(h)) = (&low, &high)
                    && l.compare(h) == Some(std::cmp::Ordering::Greater)
                {
                    return Err(self.error(format!(
                        "The BETWEEN operator requires upper bound to be greater than or equal to lower bound; lower operand: {}, upper operand: {}",
                        l.to_value(),
                        h.to_value()
                    )));
                }
                Ok(Cond::Between(left, low, high))
            }
            Tok::Kw(Kw::In) => {
                self.next();
                self.expect(Tok::LParen)?;
                let mut list = vec![self.operand()?];
                while *self.peek() == Tok::Comma {
                    self.next();
                    list.push(self.operand()?);
                }
                self.expect(Tok::RParen)?;
                if list.len() > 100 {
                    return Err(self.error(
                        "The IN operator is provided with too many operands; number of operands: 101",
                    ));
                }
                Ok(Cond::In(left, list))
            }
            _ => Err(self.syntax()),
        }
    }

    fn function(&mut self, f: &str) -> Result<Cond, AwsError> {
        self.next();
        self.next();
        let path = self.path()?;
        let cond = match f {
            "attribute_exists" => Cond::Exists(path),
            "attribute_not_exists" => Cond::NotExists(path),
            "attribute_type" => {
                self.expect(Tok::Comma)?;
                let t = self.value()?;
                let name = match &t {
                    Attr::S(s) if TYPE_NAMES.contains(&s.as_str()) => s.clone(),
                    other => {
                        return Err(self.error(format!(
                            "Invalid attribute type name found; type: {}, valid types: {{ B,NULL,SS,BOOL,L,BS,N,NS,S,M }}",
                            match other {
                                Attr::S(s) => s.clone(),
                                v => v.type_name().to_string(),
                            }
                        )));
                    }
                };
                Cond::Type(path, name)
            }
            "begins_with" => {
                self.expect(Tok::Comma)?;
                Cond::BeginsWith(path, self.operand()?)
            }
            _ => {
                self.expect(Tok::Comma)?;
                Cond::Contains(path, self.operand()?)
            }
        };
        self.expect(Tok::RParen)?;
        Ok(cond)
    }

    fn set_operand(&mut self) -> Result<SetOperand, AwsError> {
        match self.peek().clone() {
            Tok::Value(_) => Ok(SetOperand::Value(self.value()?)),
            Tok::Ident(f) if *self.peek_at(1) == Tok::LParen => {
                self.next();
                self.next();
                let out = match f.as_str() {
                    "if_not_exists" => {
                        let p = self.path()?;
                        self.expect(Tok::Comma)?;
                        SetOperand::IfNotExists(p, Box::new(self.set_operand()?))
                    }
                    "list_append" => {
                        let a = self.set_operand()?;
                        self.expect(Tok::Comma)?;
                        SetOperand::ListAppend(Box::new(a), Box::new(self.set_operand()?))
                    }
                    _ => {
                        return Err(self.error(format!("Invalid function name; function: {f}")));
                    }
                };
                self.expect(Tok::RParen)?;
                Ok(out)
            }
            Tok::Ident(_) | Tok::Name(_) => Ok(SetOperand::Path(self.path()?)),
            _ => Err(self.syntax()),
        }
    }

    fn set_value(&mut self) -> Result<SetValue, AwsError> {
        let left = self.set_operand()?;
        Ok(match self.peek() {
            Tok::Plus => {
                self.next();
                SetValue::Plus(left, self.set_operand()?)
            }
            Tok::Minus => {
                self.next();
                SetValue::Minus(left, self.set_operand()?)
            }
            _ => SetValue::Operand(left),
        })
    }

    pub fn update(&mut self) -> Result<Update, AwsError> {
        let mut u = Update::default();
        let mut seen = Vec::new();
        while *self.peek() != Tok::Eof {
            let Tok::Kw(kw @ (Kw::Set | Kw::Remove | Kw::Add | Kw::Delete)) = self.peek().clone()
            else {
                return Err(self.syntax());
            };
            if seen.contains(&kw) {
                return Err(self.error(format!(
                    "The \"{}\" section can only be used once in an update expression;",
                    Tok::Kw(kw).text()
                )));
            }
            seen.push(kw);
            self.next();
            loop {
                let path = self.path()?;
                match kw {
                    Kw::Set => {
                        self.expect(Tok::Cmp(CmpOp::Eq))?;
                        let v = self.set_value()?;
                        u.set.push((path, v));
                    }
                    Kw::Remove => u.remove.push(path),
                    Kw::Add => {
                        let v = self.value()?;
                        u.add.push((path, v));
                    }
                    _ => {
                        let v = self.value()?;
                        u.delete.push((path, v));
                    }
                }
                if *self.peek() != Tok::Comma {
                    break;
                }
                self.next();
            }
        }
        if seen.is_empty() {
            return Err(self.syntax());
        }
        let targets = u.targets();
        for (i, a) in targets.iter().enumerate() {
            for b in &targets[i + 1..] {
                if a.overlaps(b) {
                    return Err(self.error(format!(
                        "Two document paths overlap with each other; must remove or rewrite one of these paths; path one: {}, path two: {}",
                        a.describe(),
                        b.describe()
                    )));
                }
            }
        }
        Ok(u)
    }

    pub fn projection(&mut self) -> Result<Vec<Path>, AwsError> {
        let mut paths = vec![self.path()?];
        while *self.peek() == Tok::Comma {
            self.next();
            paths.push(self.path()?);
        }
        self.expect_end()?;
        for (i, a) in paths.iter().enumerate() {
            for b in &paths[i + 1..] {
                if a.overlaps(b) {
                    return Err(self.error(format!(
                        "Two document paths overlap with each other; must remove or rewrite one of these paths; path one: {}, path two: {}",
                        a.describe(),
                        b.describe()
                    )));
                }
            }
        }
        Ok(paths)
    }
}

pub fn parse_condition(
    kind: &'static str,
    input: &str,
    b: &mut Bindings,
) -> Result<Cond, AwsError> {
    let mut p = Parser::new(kind, input, b)?;
    let c = p.condition()?;
    p.expect_end()?;
    Ok(c)
}

pub fn parse_update(input: &str, b: &mut Bindings) -> Result<Update, AwsError> {
    Parser::new("UpdateExpression", input, b)?.update()
}

pub fn parse_projection(input: &str, b: &mut Bindings) -> Result<Vec<Path>, AwsError> {
    Parser::new("ProjectionExpression", input, b)?.projection()
}

pub fn parse_key_condition(
    input: &str,
    b: &mut Bindings,
) -> Result<Vec<(String, RangeCond)>, AwsError> {
    let kind = "KeyConditionExpression";
    let cond = parse_condition(kind, input, b)?;
    let unsupported = || validation("Query key condition not supported");
    let mut leaves = Vec::new();
    let mut stack = vec![cond];
    while let Some(c) = stack.pop() {
        match c {
            Cond::And(a, b) => {
                stack.push(*b);
                stack.push(*a);
            }
            Cond::Or(..) => {
                return Err(validation(format!("Invalid operator used in {kind}: OR")));
            }
            Cond::Not(_) => {
                return Err(validation(format!("Invalid operator used in {kind}: NOT")));
            }
            other => leaves.push(other),
        }
    }
    let key_path = |p: Path| {
        if p.steps.is_empty() {
            Ok(p.root)
        } else {
            Err(validation(format!(
                "Key condition expression cannot reference nested attributes: {}",
                p.describe()
            )))
        }
    };
    let mut out = Vec::new();
    for leaf in leaves {
        out.push(match leaf {
            Cond::Cmp(op, Operand::Path(p), Operand::Value(v)) if op != CmpOp::Ne => {
                (key_path(p)?, RangeCond::Cmp(op, v))
            }
            Cond::Cmp(CmpOp::Ne, ..) => {
                return Err(validation(format!("Invalid operator used in {kind}: <>")));
            }
            Cond::Between(Operand::Path(p), Operand::Value(l), Operand::Value(h)) => {
                (key_path(p)?, RangeCond::Between(l, h))
            }
            Cond::BeginsWith(p, Operand::Value(v)) => (key_path(p)?, RangeCond::BeginsWith(v)),
            Cond::In(..) => {
                return Err(validation(format!("Invalid operator used in {kind}: IN")));
            }
            Cond::Exists(_) | Cond::NotExists(_) | Cond::Type(..) | Cond::Contains(..) => {
                return Err(validation(format!(
                    "Invalid operator used in {kind}: function"
                )));
            }
            _ => return Err(unsupported()),
        });
    }
    Ok(out)
}

impl KeyCond {
    pub fn resolve(
        parts: Vec<(String, RangeCond)>,
        hash: &str,
        range: Option<&str>,
    ) -> Result<Self, AwsError> {
        if parts.len() > 2 {
            return Err(validation(
                "KeyConditionExpressions must only contain one condition per key",
            ));
        }
        let mut hash_value = None;
        let mut range_cond = None;
        for (name, cond) in parts {
            if name == hash && hash_value.is_none() {
                match cond {
                    RangeCond::Cmp(CmpOp::Eq, v) => hash_value = Some(v),
                    _ => {
                        return Err(validation("Query key condition not supported"));
                    }
                }
            } else if Some(name.as_str()) == range && range_cond.is_none() {
                range_cond = Some((name, cond));
            } else if name == hash || Some(name.as_str()) == range {
                return Err(validation(
                    "KeyConditionExpressions must only contain one condition per key",
                ));
            } else {
                return Err(validation(format!(
                    "Query condition missed key schema element: {}",
                    range.filter(|_| hash_value.is_some()).unwrap_or(hash)
                )));
            }
        }
        let Some(v) = hash_value else {
            return Err(validation(format!(
                "Query condition missed key schema element: {hash}"
            )));
        };
        Ok(Self {
            hash: (hash.to_string(), v),
            range: range_cond,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bindings(names: Value, values: Value) -> Bindings {
        Bindings::from_input(&json!({
            "ExpressionAttributeNames": names,
            "ExpressionAttributeValues": values,
        }))
        .unwrap()
    }

    #[test]
    fn tokenizes_operators_and_case_insensitive_keywords() {
        let toks = tokenize("ConditionExpression", "a <> :v and b[1].c >= #n").unwrap();
        assert_eq!(toks[1], Tok::Cmp(CmpOp::Ne));
        assert_eq!(toks[3], Tok::Kw(Kw::And));
        assert_eq!(toks[5], Tok::LBracket);
        assert_eq!(toks[10], Tok::Cmp(CmpOp::Ge));
        assert_eq!(toks.last(), Some(&Tok::Eof));
    }

    #[test]
    fn parses_condition_precedence() {
        let mut b = bindings(
            json!({"#s": "status"}),
            json!({":a": {"S": "x"}, ":b": {"N": "1"}}),
        );
        let c = parse_condition(
            "ConditionExpression",
            "NOT #s = :a OR attribute_exists(n) AND n < :b",
            &mut b,
        )
        .unwrap();
        let Cond::Or(left, right) = c else {
            panic!("{c:?}")
        };
        assert!(matches!(*left, Cond::Not(_)));
        assert!(matches!(*right, Cond::And(..)));
        b.check_unused().unwrap();
    }

    #[test]
    fn reports_reserved_words_and_undefined_references() {
        let mut b = Bindings::default();
        let e = parse_condition("ConditionExpression", "size = :v", &mut b).unwrap_err();
        assert_eq!(
            e.message,
            "Invalid ConditionExpression: Attribute name is a reserved keyword; reserved keyword: size"
        );
        let e = parse_condition("ConditionExpression", "#x = :v", &mut b).unwrap_err();
        assert!(e.message.contains("attribute name: #x"), "{}", e.message);
        let e = parse_condition("ConditionExpression", "a = :v", &mut b).unwrap_err();
        assert!(e.message.contains("attribute value: :v"), "{}", e.message);
        let e = parse_condition("ConditionExpression", "a = ", &mut b).unwrap_err();
        assert!(
            e.message.contains("Syntax error; token: \"<EOF>\""),
            "{}",
            e.message
        );
    }

    #[test]
    fn detects_unused_bindings() {
        let mut b = bindings(
            json!({"#a": "a"}),
            json!({":v": {"N": "1"}, ":w": {"N": "2"}}),
        );
        parse_condition("ConditionExpression", "#a = :v", &mut b).unwrap();
        assert_eq!(
            b.check_unused().unwrap_err().message,
            "Value provided in ExpressionAttributeValues unused in expressions: keys: {:w}"
        );
    }

    #[test]
    fn parses_update_sections_and_rejects_overlaps() {
        let mut b = bindings(
            json!({"#l": "list"}),
            json!({":v": {"N": "1"}, ":s": {"SS": ["a"]}, ":z": {"L": []}}),
        );
        let u = parse_update(
            "SET a = a + :v, #l = list_append(if_not_exists(#l, :z), :z) REMOVE b[2] ADD c :v DELETE d :s",
            &mut b,
        )
        .unwrap();
        assert_eq!(u.set.len(), 2);
        assert_eq!(u.remove[0].steps, vec![Step::Index(2)]);
        let e = parse_update("SET a = :v REMOVE a", &mut b).unwrap_err();
        assert!(e.message.contains("overlap"), "{}", e.message);
        let e = parse_update("SET a = :v SET b = :v", &mut b).unwrap_err();
        assert!(e.message.contains("can only be used once"), "{}", e.message);
    }

    #[test]
    fn resolves_key_conditions_in_either_order() {
        let mut b = bindings(
            json!({"#sk": "sk"}),
            json!({":p": {"S": "p"}, ":lo": {"N": "1"}, ":hi": {"N": "5"}}),
        );
        let parts = parse_key_condition("#sk BETWEEN :lo AND :hi AND pk = :p", &mut b).unwrap();
        let k = KeyCond::resolve(parts, "pk", Some("sk")).unwrap();
        assert_eq!(k.hash.0, "pk");
        assert!(matches!(k.range, Some((_, RangeCond::Between(..)))));
        let parts = parse_key_condition("#sk = :lo", &mut b).unwrap();
        assert_eq!(
            KeyCond::resolve(parts, "pk", Some("sk"))
                .unwrap_err()
                .message,
            "Query condition missed key schema element: pk"
        );
        assert!(parse_key_condition("pk = :p OR pk = :lo", &mut b).is_err());
    }

    #[test]
    fn rejects_overlapping_projection() {
        let mut b = Bindings::default();
        assert!(parse_projection("a.b, a", &mut b).is_err());
        assert_eq!(
            parse_projection("a[0], a[2], b.c", &mut b).unwrap().len(),
            3
        );
    }
}
