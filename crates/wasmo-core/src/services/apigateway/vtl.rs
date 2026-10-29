//! The Velocity Template Language subset API Gateway mapping templates use:
//! references (`$x`, `$!x`, `${x}`, property and method chains), `#set`,
//! `#if` / `#elseif` / `#else`, `#foreach`, and the `$input`, `$util`,
//! `$context`, and `$stageVariables` objects.

use std::collections::HashMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use crate::jsonpath as path;

/// The request or response a template reads.
pub struct Input<'a> {
    pub body: &'a str,
    /// `path`, `querystring`, and `header` maps.
    pub params: Value,
}

pub struct Scope<'a> {
    pub input: Input<'a>,
    pub context: Value,
    pub stage_variables: Value,
}

#[derive(Debug, Clone)]
enum Node {
    Text(String),
    Ref { quiet: bool, expr: Expr },
    Set(String, Expr),
    If(Vec<(Expr, Vec<Node>)>, Vec<Node>),
    Foreach(String, Expr, Vec<Node>),
}

#[derive(Debug, Clone)]
enum Expr {
    Lit(Value),
    Interp(String),
    Ref(String, Vec<Step>),
    List(Vec<Expr>),
    Not(Box<Expr>),
    Bin(Box<Expr>, String, Box<Expr>),
}

#[derive(Debug, Clone)]
enum Step {
    Prop(String),
    Call(String, Vec<Expr>),
}

struct P<'a> {
    s: &'a [u8],
    src: &'a str,
    i: usize,
}

const OPS: &[(&str, u8)] = &[
    ("||", 1),
    ("or ", 1),
    ("&&", 2),
    ("and ", 2),
    ("==", 3),
    ("!=", 3),
    ("<=", 3),
    (">=", 3),
    ("<", 3),
    (">", 3),
    ("+", 4),
    ("-", 4),
];

fn ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

impl<'a> P<'a> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn at(&self, lit: &str) -> bool {
        self.src[self.i..].starts_with(lit)
    }

    fn ws(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn ident(&mut self) -> String {
        let start = self.i;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            self.i += 1;
        }
        self.src[start..self.i].to_string()
    }

    /// Parses nodes until one of `stops` (`#end`, `#else`, `#elseif`).
    fn nodes(&mut self, stops: &[&str]) -> (Vec<Node>, Option<String>) {
        let mut out = Vec::new();
        let mut text = String::new();
        while self.i < self.s.len() {
            if self.at("##") {
                while self.peek().is_some_and(|c| c != b'\n') {
                    self.i += 1;
                }
                continue;
            }
            if self.peek() == Some(b'#') {
                if let Some(stop) = stops.iter().find(|s| {
                    self.at(s)
                        && (s.ends_with('}')
                            || !self.src[self.i + s.len()..]
                                .starts_with(|c: char| c.is_ascii_alphabetic()))
                }) {
                    if !text.is_empty() {
                        out.push(Node::Text(std::mem::take(&mut text)));
                    }
                    self.i += stop.len();
                    return (out, Some(stop.to_string()));
                }
                let save = self.i;
                if let Some(node) = self.directive() {
                    if !text.is_empty() {
                        out.push(Node::Text(std::mem::take(&mut text)));
                    }
                    out.push(node);
                    continue;
                }
                self.i = save;
            }
            if self.peek() == Some(b'$') {
                let save = self.i;
                if let Some(node) = self.reference() {
                    if !text.is_empty() {
                        out.push(Node::Text(std::mem::take(&mut text)));
                    }
                    out.push(node);
                    continue;
                }
                self.i = save;
            }
            let ch = self.src[self.i..].chars().next().expect("in range");
            text.push(ch);
            self.i += ch.len_utf8();
        }
        if !text.is_empty() {
            out.push(Node::Text(text));
        }
        (out, None)
    }

    fn directive(&mut self) -> Option<Node> {
        self.i += 1;
        let braced = self.peek() == Some(b'{');
        if braced {
            self.i += 1;
        }
        let name = self.ident();
        if braced {
            (self.peek() == Some(b'}')).then(|| self.i += 1)?;
        }
        match name.as_str() {
            "set" => {
                self.ws();
                (self.peek() == Some(b'(')).then(|| self.i += 1)?;
                self.ws();
                (self.peek() == Some(b'$')).then(|| self.i += 1)?;
                if self.peek() == Some(b'{') {
                    self.i += 1;
                }
                let var = self.ident();
                if self.peek() == Some(b'}') {
                    self.i += 1;
                }
                self.ws();
                (self.peek() == Some(b'=')).then(|| self.i += 1)?;
                let e = self.expr()?;
                self.ws();
                (self.peek() == Some(b')')).then(|| self.i += 1)?;
                self.eat_newline();
                Some(Node::Set(var, e))
            }
            "if" => {
                let mut branches = Vec::new();
                let mut cond = self.paren_expr()?;
                loop {
                    let (body, stop) =
                        self.nodes(&["#elseif", "#else", "#end", "#{end}", "#{else}"]);
                    branches.push((cond, body));
                    match stop.as_deref() {
                        Some("#elseif") => cond = self.paren_expr()?,
                        Some("#else" | "#{else}") => {
                            self.eat_newline();
                            let (body, _) = self.nodes(&["#end", "#{end}"]);
                            self.eat_newline();
                            return Some(Node::If(branches, body));
                        }
                        _ => {
                            self.eat_newline();
                            return Some(Node::If(branches, Vec::new()));
                        }
                    }
                }
            }
            "foreach" => {
                self.ws();
                (self.peek() == Some(b'(')).then(|| self.i += 1)?;
                self.ws();
                (self.peek() == Some(b'$')).then(|| self.i += 1)?;
                let var = self.ident();
                self.ws();
                self.at("in").then(|| self.i += 2)?;
                let list = self.expr()?;
                self.ws();
                (self.peek() == Some(b')')).then(|| self.i += 1)?;
                self.eat_newline();
                let (body, _) = self.nodes(&["#end", "#{end}"]);
                self.eat_newline();
                Some(Node::Foreach(var, list, body))
            }
            _ => None,
        }
    }

    fn eat_newline(&mut self) {
        let save = self.i;
        while self.peek().is_some_and(|c| c == b' ' || c == b'\t') {
            self.i += 1;
        }
        if self.peek() == Some(b'\n') {
            self.i += 1;
        } else if self.at("\r\n") {
            self.i += 2;
        } else {
            self.i = save;
        }
    }

    fn paren_expr(&mut self) -> Option<Expr> {
        self.ws();
        (self.peek() == Some(b'(')).then(|| self.i += 1)?;
        let e = self.expr()?;
        self.ws();
        (self.peek() == Some(b')')).then(|| self.i += 1)?;
        self.eat_newline();
        Some(e)
    }

    fn reference(&mut self) -> Option<Node> {
        self.i += 1;
        let quiet = self.peek() == Some(b'!');
        if quiet {
            self.i += 1;
        }
        let braced = self.peek() == Some(b'{');
        if braced {
            self.i += 1;
        }
        let root = self.ident();
        if root.is_empty() || !root.as_bytes()[0].is_ascii_alphabetic() {
            return None;
        }
        let steps = self.steps();
        if braced {
            (self.peek() == Some(b'}')).then(|| self.i += 1)?;
        }
        Some(Node::Ref {
            quiet,
            expr: Expr::Ref(root, steps),
        })
    }

    fn steps(&mut self) -> Vec<Step> {
        let mut steps = Vec::new();
        loop {
            if self.peek() == Some(b'.')
                && self
                    .s
                    .get(self.i + 1)
                    .is_some_and(|c| c.is_ascii_alphabetic())
            {
                self.i += 1;
                let name = self.ident();
                if self.peek() == Some(b'(') {
                    self.i += 1;
                    let mut args = Vec::new();
                    loop {
                        self.ws();
                        if self.peek() == Some(b')') {
                            self.i += 1;
                            break;
                        }
                        match self.expr() {
                            Some(e) => args.push(e),
                            None => break,
                        }
                        self.ws();
                        if self.peek() == Some(b',') {
                            self.i += 1;
                        }
                    }
                    steps.push(Step::Call(name, args));
                } else {
                    steps.push(Step::Prop(name));
                }
            } else if self.peek() == Some(b'[') {
                self.i += 1;
                let e = self.expr();
                self.ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                }
                if let Some(e) = e {
                    steps.push(Step::Call("get".into(), vec![e]));
                }
            } else {
                return steps;
            }
        }
    }

    fn expr(&mut self) -> Option<Expr> {
        self.binary(0)
    }

    /// Precedence climbing over `OPS`.
    fn binary(&mut self, min: u8) -> Option<Expr> {
        let mut lhs = self.unary()?;
        loop {
            let save = self.i;
            self.ws();
            let Some(&(op, prec)) = OPS.iter().find(|(o, _)| self.at(o)) else {
                self.i = save;
                return Some(lhs);
            };
            if prec < min {
                self.i = save;
                return Some(lhs);
            }
            self.i += op.len();
            let rhs = self.binary(prec + 1)?;
            let op = match op.trim() {
                "and" => "&&",
                "or" => "||",
                o => o,
            };
            lhs = Expr::Bin(Box::new(lhs), op.to_string(), Box::new(rhs));
        }
    }

    fn unary(&mut self) -> Option<Expr> {
        self.ws();
        match self.peek()? {
            b'!' => {
                self.i += 1;
                Some(Expr::Not(Box::new(self.unary()?)))
            }
            b'(' => {
                self.i += 1;
                let e = self.expr()?;
                self.ws();
                (self.peek() == Some(b')')).then(|| self.i += 1)?;
                Some(e)
            }
            b'$' => {
                self.i += 1;
                if self.peek() == Some(b'!') {
                    self.i += 1;
                }
                let braced = self.peek() == Some(b'{');
                if braced {
                    self.i += 1;
                }
                let root = self.ident();
                let steps = self.steps();
                if braced && self.peek() == Some(b'}') {
                    self.i += 1;
                }
                Some(Expr::Ref(root, steps))
            }
            q @ (b'\'' | b'"') => {
                self.i += 1;
                let start = self.i;
                while self.peek().is_some_and(|c| c != q) {
                    self.i += 1;
                }
                let text = self.src[start..self.i].to_string();
                self.i += 1;
                Some(if q == b'"' && text.contains('$') {
                    Expr::Interp(text)
                } else {
                    Expr::Lit(Value::String(text))
                })
            }
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                loop {
                    self.ws();
                    if self.peek() == Some(b']') {
                        self.i += 1;
                        return Some(Expr::List(items));
                    }
                    items.push(self.expr()?);
                    self.ws();
                    if self.peek() == Some(b',') {
                        self.i += 1;
                    }
                }
            }
            _ => {
                let start = self.i;
                while self.peek().is_some_and(|c| ident_char(c) || c == b'.') {
                    self.i += 1;
                }
                let word = &self.src[start..self.i];
                match word {
                    "true" => Some(Expr::Lit(json!(true))),
                    "false" => Some(Expr::Lit(json!(false))),
                    "null" => Some(Expr::Lit(Value::Null)),
                    w => w.parse::<f64>().ok().map(|n| {
                        Expr::Lit(if n.fract() == 0.0 {
                            json!(n as i64)
                        } else {
                            json!(n)
                        })
                    }),
                }
            }
        }
    }
}

/// Renders `template` against the request or response in `scope`.
pub fn render(template: &str, scope: &Scope<'_>) -> String {
    let mut p = P {
        s: template.as_bytes(),
        src: template,
        i: 0,
    };
    let (nodes, _) = p.nodes(&[]);
    let mut vars = HashMap::new();
    let mut out = String::new();
    run(&nodes, scope, &mut vars, &mut out);
    out
}

fn run(nodes: &[Node], scope: &Scope<'_>, vars: &mut HashMap<String, Value>, out: &mut String) {
    for n in nodes {
        match n {
            Node::Text(t) => out.push_str(t),
            Node::Ref { quiet, expr } => match eval(expr, scope, vars) {
                Some(v) => out.push_str(&text(&v)),
                None if *quiet => {}
                None => out.push_str(&source(expr)),
            },
            Node::Set(name, e) => {
                let v = eval(e, scope, vars).unwrap_or(Value::Null);
                vars.insert(name.clone(), v);
            }
            Node::If(branches, otherwise) => {
                let chosen = branches
                    .iter()
                    .find(|(c, _)| truthy(&eval(c, scope, vars).unwrap_or(Value::Null)))
                    .map(|(_, b)| b)
                    .unwrap_or(otherwise);
                run(chosen, scope, vars, out);
            }
            Node::Foreach(var, list, body) => {
                let items = match eval(list, scope, vars) {
                    Some(Value::Array(a)) => a,
                    Some(Value::Object(o)) => o.values().cloned().collect(),
                    _ => Vec::new(),
                };
                let n = items.len();
                for (i, item) in items.into_iter().enumerate() {
                    vars.insert(var.clone(), item);
                    vars.insert(
                        "foreach".into(),
                        json!({ "hasNext": i + 1 < n, "index": i, "count": i + 1 }),
                    );
                    vars.insert("velocityCount".into(), json!(i + 1));
                    run(body, scope, vars, out);
                }
            }
        }
    }
}

fn source(e: &Expr) -> String {
    match e {
        Expr::Ref(root, steps) => {
            let mut s = format!("${root}");
            for st in steps {
                match st {
                    Step::Prop(p) => s.push_str(&format!(".{p}")),
                    Step::Call(m, _) => s.push_str(&format!(".{m}(...)")),
                }
            }
            s
        }
        _ => String::new(),
    }
}

pub fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn eval(e: &Expr, scope: &Scope<'_>, vars: &HashMap<String, Value>) -> Option<Value> {
    match e {
        Expr::Lit(v) => Some(v.clone()),
        Expr::Interp(t) => {
            let mut vars2 = vars.clone();
            let mut out = String::new();
            let mut p = P {
                s: t.as_bytes(),
                src: t,
                i: 0,
            };
            let (nodes, _) = p.nodes(&[]);
            run(&nodes, scope, &mut vars2, &mut out);
            Some(Value::String(out))
        }
        Expr::List(items) => Some(Value::Array(
            items
                .iter()
                .map(|x| eval(x, scope, vars).unwrap_or(Value::Null))
                .collect(),
        )),
        Expr::Not(x) => Some(json!(!truthy(&eval(x, scope, vars).unwrap_or(Value::Null)))),
        Expr::Bin(a, op, b) => {
            let a = eval(a, scope, vars).unwrap_or(Value::Null);
            let b = eval(b, scope, vars).unwrap_or(Value::Null);
            Some(binary(&a, op, &b))
        }
        Expr::Ref(root, steps) => {
            let (mut v, rest) = root_value(root, steps, scope, vars)?;
            for st in rest {
                v = step(&v, st, scope, vars)?;
            }
            Some(v)
        }
    }
}

fn binary(a: &Value, op: &str, b: &Value) -> Value {
    let num = |v: &Value| {
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    };
    match op {
        "&&" => json!(truthy(a) && truthy(b)),
        "||" => json!(truthy(a) || truthy(b)),
        "==" => json!(
            a == b || (num(a).is_some() && num(a) == num(b)) || text(a) == text(b) && !a.is_null()
        ),
        "!=" => json!(!(a == b || text(a) == text(b) && !a.is_null())),
        "+" => match (a, b) {
            (Value::Number(_), Value::Number(_)) => {
                json!(num(a).unwrap_or(0.0) + num(b).unwrap_or(0.0))
            }
            _ => json!(format!("{}{}", text(a), text(b))),
        },
        "-" => json!(num(a).unwrap_or(0.0) - num(b).unwrap_or(0.0)),
        o => {
            let ord = match (num(a), num(b)) {
                (Some(x), Some(y)) => x.partial_cmp(&y),
                _ => Some(text(a).cmp(&text(b))),
            };
            json!(ord.is_some_and(|o2| match o {
                "<" => o2.is_lt(),
                ">" => o2.is_gt(),
                "<=" => o2.is_le(),
                ">=" => o2.is_ge(),
                _ => false,
            }))
        }
    }
}

fn arg(args: &[Expr], i: usize, scope: &Scope<'_>, vars: &HashMap<String, Value>) -> Value {
    args.get(i)
        .and_then(|e| eval(e, scope, vars))
        .unwrap_or(Value::Null)
}

fn json_path(body: &str, p: &str) -> Value {
    let data: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    path::get(&data, p).ok().flatten().unwrap_or(Value::Null)
}

fn root_value<'s>(
    root: &str,
    steps: &'s [Step],
    scope: &Scope<'_>,
    vars: &HashMap<String, Value>,
) -> Option<(Value, &'s [Step])> {
    if let Some(v) = vars.get(root) {
        return Some((v.clone(), steps));
    }
    match root {
        "input" => {
            let (first, rest) = steps.split_first()?;
            let v = match first {
                Step::Prop(p) if p == "body" => json!(scope.input.body),
                Step::Call(m, args) if m == "json" => {
                    let p = text(&arg(args, 0, scope, vars));
                    json!(json_path(scope.input.body, &p).to_string())
                }
                Step::Call(m, args) if m == "path" => {
                    json_path(scope.input.body, &text(&arg(args, 0, scope, vars)))
                }
                Step::Call(m, args) if m == "params" && args.is_empty() => {
                    scope.input.params.clone()
                }
                Step::Call(m, args) if m == "params" => {
                    let name = text(&arg(args, 0, scope, vars));
                    ["path", "querystring", "header"]
                        .iter()
                        .find_map(|k| {
                            let m = scope.input.params.get(k)?.as_object()?;
                            m.get(&name).cloned().or_else(|| {
                                (*k == "header")
                                    .then(|| {
                                        m.iter()
                                            .find(|(h, _)| h.eq_ignore_ascii_case(&name))
                                            .map(|(_, v)| v.clone())
                                    })
                                    .flatten()
                            })
                        })
                        .unwrap_or(json!(""))
                }
                _ => return None,
            };
            Some((v, rest))
        }
        "util" => {
            let (Step::Call(m, args), rest) = steps.split_first()? else {
                return None;
            };
            let a = text(&arg(args, 0, scope, vars));
            let v = match m.as_str() {
                "escapeJavaScript" => json!(escape_js(&a)),
                "base64Encode" => json!(STANDARD.encode(a.as_bytes())),
                "base64Decode" => json!(String::from_utf8_lossy(
                    &STANDARD.decode(a.as_bytes()).unwrap_or_default()
                )),
                "urlEncode" => json!(url_encode(&a)),
                "urlDecode" => json!(url_decode(&a)),
                "parseJson" => serde_json::from_str(&a).unwrap_or(Value::Null),
                "toJson" => json!(arg(args, 0, scope, vars).to_string()),
                _ => return None,
            };
            Some((v, rest))
        }
        "context" => Some((scope.context.clone(), steps)),
        "stageVariables" => Some((scope.stage_variables.clone(), steps)),
        _ => None,
    }
}

fn step(v: &Value, st: &Step, scope: &Scope<'_>, vars: &HashMap<String, Value>) -> Option<Value> {
    match st {
        Step::Prop(p) => match v {
            Value::Object(o) => o.get(p).cloned().or(Some(Value::Null)),
            Value::Null => Some(Value::Null),
            _ => None,
        },
        Step::Call(m, args) => {
            let a0 = arg(args, 0, scope, vars);
            Some(match (m.as_str(), v) {
                ("get", Value::Object(o)) => o.get(&text(&a0)).cloned().unwrap_or(Value::Null),
                ("get", Value::Array(a)) => a0
                    .as_u64()
                    .and_then(|i| a.get(i as usize))
                    .cloned()
                    .unwrap_or(Value::Null),
                ("keySet", Value::Object(o)) => json!(o.keys().collect::<Vec<_>>()),
                ("entrySet", Value::Object(o)) => json!(
                    o.iter()
                        .map(|(k, v)| json!({ "key": k, "value": v }))
                        .collect::<Vec<_>>()
                ),
                ("containsKey", Value::Object(o)) => json!(o.contains_key(&text(&a0))),
                ("size", Value::Object(o)) => json!(o.len()),
                ("size", Value::Array(a)) => json!(a.len()),
                ("isEmpty", Value::Object(o)) => json!(o.is_empty()),
                ("isEmpty", Value::Array(a)) => json!(a.is_empty()),
                ("contains", Value::Array(a)) => json!(a.contains(&a0)),
                ("length", Value::String(s)) => json!(s.chars().count()),
                ("isEmpty", Value::String(s)) => json!(s.is_empty()),
                ("contains", Value::String(s)) => json!(s.contains(&text(&a0))),
                ("startsWith", Value::String(s)) => json!(s.starts_with(&text(&a0))),
                ("endsWith", Value::String(s)) => json!(s.ends_with(&text(&a0))),
                ("equals", x) => json!(text(x) == text(&a0)),
                ("toLowerCase", Value::String(s)) => json!(s.to_lowercase()),
                ("toUpperCase", Value::String(s)) => json!(s.to_uppercase()),
                ("trim", Value::String(s)) => json!(s.trim()),
                ("replace" | "replaceAll", Value::String(s)) => {
                    json!(s.replace(&text(&a0), &text(&arg(args, 1, scope, vars))))
                }
                ("split", Value::String(s)) => json!(s.split(&text(&a0)).collect::<Vec<_>>()),
                ("substring", Value::String(s)) => {
                    let chars: Vec<char> = s.chars().collect();
                    let start = a0.as_u64().unwrap_or(0) as usize;
                    let end = arg(args, 1, scope, vars)
                        .as_u64()
                        .map_or(chars.len(), |e| e as usize);
                    json!(
                        chars
                            .get(start.min(chars.len())..end.min(chars.len()))
                            .map(|c| c.iter().collect::<String>())
                            .unwrap_or_default()
                    )
                }
                ("toString", x) => json!(text(x)),
                _ => return None,
            })
        }
    }
}

fn escape_js(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

pub fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                (b as char).to_string()
            }
            b' ' => "+".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                if let Ok(x) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(x);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The `$input.params()` object of a request.
pub fn params(
    path: &Map<String, Value>,
    query: &[(String, String)],
    headers: &[(String, String)],
) -> Value {
    let q: Map<String, Value> = query.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    let h: Map<String, Value> = headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    json!({ "path": path, "querystring": q, "header": h })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(body: &str) -> Scope<'_> {
        Scope {
            input: Input {
                body,
                params: json!({ "path": { "id": "7" }, "querystring": { "q": "x" }, "header": { "X-Trace": "t1" } }),
            },
            context: json!({ "requestId": "r1", "stage": "prod", "authorizer": { "principalId": "me" } }),
            stage_variables: json!({ "env": "test" }),
        }
    }

    #[test]
    fn renders_templates() {
        let s = scope(r#"{"name":"Ann","items":[{"n":1},{"n":2}],"quote":"a\"b"}"#);
        assert_eq!(render("{\"statusCode\": 200}", &s), "{\"statusCode\": 200}");
        assert_eq!(
            render(
                "{\"id\": \"$input.params('id')\", \"q\": \"$input.params().querystring.get('q')\", \"t\": \"$input.params('x-trace')\"}",
                &s
            ),
            "{\"id\": \"7\", \"q\": \"x\", \"t\": \"t1\"}"
        );
        assert_eq!(render("$input.json('$.items[0]')", &s), "{\"n\":1}");
        assert_eq!(
            render(
                "$input.path('$.name') ${stageVariables.env} $context.stage $context.authorizer.principalId",
                &s
            ),
            "Ann test prod me"
        );
        assert_eq!(
            render(
                "#set($root = $input.path('$'))[#foreach($i in $root.items)$i.n#if($foreach.hasNext),#end#end]",
                &s
            ),
            "[1,2]"
        );
        assert_eq!(
            render("#if($input.path('$.name') == \"Ann\")yes#{else}no#end", &s),
            "yes"
        );
        assert_eq!(
            render(
                "#if($input.path('$.missing'))yes#elseif(1 < 2)mid#else\nno\n#end",
                &s
            ),
            "mid"
        );
        assert_eq!(
            render("$util.escapeJavaScript($input.path('$.quote'))", &s),
            "a\\\"b"
        );
        assert_eq!(
            render("$util.base64Encode('hi') $util.urlEncode('a b&c')", &s),
            "aGk= a+b%26c"
        );
        assert_eq!(render("$!nothing$nothing", &s), "$nothing");
        assert_eq!(
            render("[$input.path('$.missing')$context.missing.deeper]", &s),
            "[]"
        );
        assert_eq!(
            render(
                "Action=SendMessage&MessageBody=$util.urlEncode($input.body)",
                &scope("{}")
            ),
            "Action=SendMessage&MessageBody=%7B%7D"
        );
    }
}
