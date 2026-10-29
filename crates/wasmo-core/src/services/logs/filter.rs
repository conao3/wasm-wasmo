//! CloudWatch Logs filter and pattern syntax: term patterns, `%regex%`,
//! JSON patterns (`{ $.a = "x" && $.b > 1 }`), and space-delimited patterns
//! (`[ip, user, ..., status = 4*, bytes]`).

use std::collections::BTreeMap;

use regex_lite::Regex;
use serde_json::Value;

#[derive(Debug, Clone)]
pub enum Pattern {
    All,
    Terms {
        all: Vec<String>,
        any: Vec<String>,
        none: Vec<String>,
    },
    Regex(Regex),
    Json(Expr),
    Fields(Vec<Field>),
}

#[derive(Debug, Clone)]
pub enum Expr {
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Cmp(String, String, Operand),
}

#[derive(Debug, Clone)]
pub enum Operand {
    Str(String),
    Num(f64),
    Regex(Regex),
    Keyword(String),
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: Option<String>,
    pub ellipsis: bool,
    pub cond: Option<Expr>,
}

/// What a match exposes to metric transformations: the parsed JSON document or
/// the named space-delimited fields.
#[derive(Debug, Default)]
pub struct Captures {
    pub json: Option<Value>,
    pub fields: BTreeMap<String, String>,
}

impl Captures {
    /// Resolves a metric value or dimension selector (`$.a.b`, `$name`).
    pub fn select(&self, selector: &str) -> Option<String> {
        if let Some(path) = selector.strip_prefix("$.") {
            return match json_path(self.json.as_ref()?, path)? {
                Value::String(s) => Some(s.clone()),
                v => Some(v.to_string()),
            };
        }
        self.fields.get(selector.strip_prefix('$')?).cloned()
    }
}

fn tokens(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let b: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() || c == ',' {
            i += 1;
            continue;
        }
        let two: String = b[i..(i + 2).min(b.len())].iter().collect();
        if ["&&", "||", "!=", "<=", ">="].contains(&two.as_str()) {
            out.push(two);
            i += 2;
            continue;
        }
        if "()<>=".contains(c) {
            out.push(c.to_string());
            i += 1;
            continue;
        }
        let start = i;
        if c == '"' || c == '%' {
            i += 1;
            while i < b.len() && b[i] != c {
                if b[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
        } else {
            while i < b.len() && !b[i].is_whitespace() && !"()<>=!&|,".contains(b[i]) {
                i += 1;
            }
        }
        out.push(b[start..i.min(b.len())].iter().collect());
    }
    out
}

fn unquote(t: &str) -> String {
    t.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .map_or_else(|| t.to_string(), |s| s.replace("\\\"", "\""))
}

fn operand(t: &str) -> Result<Operand, String> {
    if let Some(re) = t.strip_prefix('%').and_then(|s| s.strip_suffix('%')) {
        return Regex::new(re)
            .map(Operand::Regex)
            .map_err(|e| e.to_string());
    }
    if t.starts_with('"') {
        return Ok(Operand::Str(unquote(t)));
    }
    if ["NULL", "TRUE", "FALSE", "EXISTS"].contains(&t) {
        return Ok(Operand::Keyword(t.to_string()));
    }
    Ok(t.parse()
        .map(Operand::Num)
        .unwrap_or_else(|_| Operand::Str(t.to_string())))
}

struct Parser {
    toks: Vec<String>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&str> {
        self.toks.get(self.pos).map(String::as_str)
    }

    fn next(&mut self) -> Result<String, String> {
        let t = self
            .toks
            .get(self.pos)
            .cloned()
            .ok_or("unexpected end of filter pattern")?;
        self.pos += 1;
        Ok(t)
    }

    fn or(&mut self, field: Option<&str>) -> Result<Expr, String> {
        let mut e = self.and(field)?;
        while self.peek() == Some("||") {
            self.pos += 1;
            e = Expr::Or(Box::new(e), Box::new(self.and(field)?));
        }
        Ok(e)
    }

    fn and(&mut self, field: Option<&str>) -> Result<Expr, String> {
        let mut e = self.primary(field)?;
        while self.peek() == Some("&&") {
            self.pos += 1;
            e = Expr::And(Box::new(e), Box::new(self.primary(field)?));
        }
        Ok(e)
    }

    fn primary(&mut self, field: Option<&str>) -> Result<Expr, String> {
        if self.peek() == Some("(") {
            self.pos += 1;
            let e = self.or(field)?;
            if self.next()? != ")" {
                return Err("expected ) in filter pattern".to_string());
            }
            return Ok(e);
        }
        let selector = self.next()?;
        if field.is_some_and(|f| f != selector) {
            return Err(format!("condition on {selector} inside field {field:?}"));
        }
        let op = self.next()?;
        let (op, value) = match op.as_str() {
            "IS" => ("IS".to_string(), self.next()?),
            "NOT" => ("NOT".to_string(), self.next()?),
            "=" | "!=" | "<" | "<=" | ">" | ">=" => (op, self.next()?),
            other => return Err(format!("unexpected {other} in filter pattern")),
        };
        Ok(Expr::Cmp(selector, op, operand(&value)?))
    }
}

pub fn parse(pattern: &str) -> Result<Pattern, String> {
    let p = pattern.trim();
    if p.is_empty() || p == "\"\"" {
        return Ok(Pattern::All);
    }
    if p.starts_with('{') {
        let inner = p
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .ok_or("unterminated JSON filter pattern")?;
        let mut parser = Parser {
            toks: tokens(inner),
            pos: 0,
        };
        let e = parser.or(None)?;
        if parser.pos != parser.toks.len() {
            return Err("trailing tokens in JSON filter pattern".to_string());
        }
        return Ok(Pattern::Json(e));
    }
    if p.starts_with('[') {
        let inner = p
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .ok_or("unterminated space-delimited filter pattern")?;
        return inner
            .split(',')
            .map(|f| {
                let f = f.trim();
                if f == "..." {
                    return Ok(Field {
                        name: None,
                        ellipsis: true,
                        cond: None,
                    });
                }
                let toks = tokens(f);
                let name = toks.first().cloned().ok_or("empty field in pattern")?;
                let cond = if toks.len() > 1 {
                    let mut parser = Parser { toks, pos: 0 };
                    Some(parser.or(Some(&name))?)
                } else {
                    None
                };
                Ok(Field {
                    name: Some(name),
                    ellipsis: false,
                    cond,
                })
            })
            .collect::<Result<_, String>>()
            .map(Pattern::Fields);
    }
    if let Some(re) = p.strip_prefix('%').and_then(|s| s.strip_suffix('%')) {
        return Regex::new(re)
            .map(Pattern::Regex)
            .map_err(|e| e.to_string());
    }
    let (mut all, mut any, mut none) = (Vec::new(), Vec::new(), Vec::new());
    for t in tokens(p) {
        if let Some(rest) = t.strip_prefix('?') {
            any.push(unquote(rest));
        } else if let Some(rest) = t.strip_prefix('-').filter(|r| !r.is_empty()) {
            none.push(unquote(rest));
        } else {
            all.push(unquote(&t));
        }
    }
    Ok(Pattern::Terms { all, any, none })
}

fn wildcard(pattern: &str, value: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == value;
    }
    let mut rest = value;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            let Some(r) = rest.strip_prefix(part) else {
                return false;
            };
            rest = r;
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else if let Some(at) = rest.find(part) {
            rest = &rest[at + part.len()..];
        } else {
            return false;
        }
    }
    true
}

fn json_path<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = doc;
    for seg in path.split('.') {
        let (name, rest) = seg.split_once('[').unwrap_or((seg, ""));
        if !name.is_empty() {
            cur = cur.get(name)?;
        }
        for idx in rest.split('[').filter(|s| !s.is_empty()) {
            cur = cur.get(idx.trim_end_matches(']').parse::<usize>().ok()?)?;
        }
    }
    Some(cur)
}

fn compare(actual: Option<&Value>, op: &str, operand: &Operand) -> bool {
    let text = actual.map(|v| match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    });
    match (op, operand) {
        ("IS", Operand::Keyword(k)) => match k.as_str() {
            "NULL" => actual.is_some_and(Value::is_null),
            "TRUE" => actual == Some(&Value::Bool(true)),
            "FALSE" => actual == Some(&Value::Bool(false)),
            _ => false,
        },
        ("NOT", Operand::Keyword(k)) if k == "EXISTS" => actual.is_none(),
        (_, Operand::Regex(re)) => {
            let m = text.as_deref().is_some_and(|t| re.is_match(t));
            if op == "!=" { !m } else { m && op == "=" }
        }
        (_, Operand::Str(s)) => {
            let m = text.as_deref().is_some_and(|t| wildcard(s, t));
            match op {
                "=" => m,
                "!=" => text.is_some() && !m,
                _ => false,
            }
        }
        (_, Operand::Num(n)) => {
            let Some(a) = actual
                .and_then(Value::as_f64)
                .or_else(|| text.as_deref().and_then(|t| t.parse().ok()))
            else {
                return false;
            };
            match op {
                "=" => a == *n,
                "!=" => a != *n,
                "<" => a < *n,
                "<=" => a <= *n,
                ">" => a > *n,
                ">=" => a >= *n,
                _ => false,
            }
        }
        _ => false,
    }
}

fn eval(e: &Expr, lookup: &dyn Fn(&str) -> Option<Value>) -> bool {
    match e {
        Expr::And(a, b) => eval(a, lookup) && eval(b, lookup),
        Expr::Or(a, b) => eval(a, lookup) || eval(b, lookup),
        Expr::Cmp(sel, op, v) => compare(lookup(sel).as_ref(), op, v),
    }
}

/// Splits a log line into space-delimited fields; text in `""` or `[]` is one field.
fn split_fields(message: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = message.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let close = match c {
            '"' => Some('"'),
            '[' => Some(']'),
            _ => None,
        };
        let mut field = String::new();
        if let Some(close) = close {
            chars.next();
            for c in chars.by_ref() {
                if c == close {
                    break;
                }
                field.push(c);
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                field.push(c);
                chars.next();
            }
        }
        out.push(field);
    }
    out
}

fn match_fields(pattern: &[Field], values: &[String], out: &mut BTreeMap<String, String>) -> bool {
    let Some((first, rest)) = pattern.split_first() else {
        return values.is_empty();
    };
    if first.ellipsis {
        return (0..=values.len()).any(|skip| {
            let mut trial = out.clone();
            let ok = match_fields(rest, &values[skip..], &mut trial);
            if ok {
                *out = trial;
            }
            ok
        });
    }
    let Some((value, more)) = values.split_first() else {
        return false;
    };
    if let Some(cond) = &first.cond {
        let v = Value::String(value.clone());
        if !eval(cond, &|_| Some(v.clone())) {
            return false;
        }
    }
    if let Some(name) = &first.name {
        out.insert(name.clone(), value.clone());
    }
    match_fields(rest, more, out)
}

impl Pattern {
    pub fn matches(&self, message: &str) -> Option<Captures> {
        let mut caps = Captures::default();
        let ok = match self {
            Pattern::All => true,
            Pattern::Terms { all, any, none } => {
                all.iter().all(|t| message.contains(t.as_str()))
                    && (any.is_empty() || any.iter().any(|t| message.contains(t.as_str())))
                    && !none.iter().any(|t| message.contains(t.as_str()))
            }
            Pattern::Regex(re) => re.is_match(message),
            Pattern::Json(e) => {
                let Ok(doc @ Value::Object(_)) = serde_json::from_str::<Value>(message) else {
                    return None;
                };
                let ok = eval(e, &|sel| {
                    let path = sel.strip_prefix("$.").or_else(|| sel.strip_prefix('$'))?;
                    if path.is_empty() {
                        return Some(doc.clone());
                    }
                    json_path(&doc, path).cloned()
                });
                caps.json = Some(doc);
                ok
            }
            Pattern::Fields(fields) => {
                match_fields(fields, &split_fields(message), &mut caps.fields)
            }
        };
        if !ok {
            return None;
        }
        if caps.json.is_none() {
            caps.json = serde_json::from_str(message).ok();
        }
        Some(caps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, message: &str) -> bool {
        parse(pattern).unwrap().matches(message).is_some()
    }

    #[test]
    fn term_patterns() {
        assert!(m("", "anything"));
        assert!(m("ERROR", "an ERROR happened"));
        assert!(!m("ERROR", "an error happened"));
        assert!(m("ERROR timeout", "ERROR: db timeout"));
        assert!(!m("ERROR timeout", "ERROR: db refused"));
        assert!(m("?ERROR ?WARN", "WARN disk"));
        assert!(!m("?ERROR ?WARN", "INFO disk"));
        assert!(m("ERROR -retry", "ERROR fatal"));
        assert!(!m("ERROR -retry", "ERROR will retry"));
        assert!(m("\"user not found\"", "lookup: user not found"));
        assert!(m("%5\\d\\d%", "status 503"));
        assert!(!m("%5\\d\\d%", "status 404"));
    }

    #[test]
    fn json_patterns() {
        let msg = r#"{"eventType":"UpdateTrail","latency":120,"user":{"id":7,"name":"ada"},"tags":["a","b"],"ok":true,"gone":null}"#;
        assert!(m(r#"{ $.eventType = "UpdateTrail" }"#, msg));
        assert!(m(r#"{ $.eventType = "Update*" }"#, msg));
        assert!(!m(r#"{ $.eventType != "UpdateTrail" }"#, msg));
        assert!(m("{ $.latency > 100 && $.user.id = 7 }", msg));
        assert!(!m("{ $.latency > 200 }", msg));
        assert!(m(r#"{ ($.latency > 200) || ($.user.name = "ada") }"#, msg));
        assert!(m(r#"{ $.tags[1] = "b" }"#, msg));
        assert!(m("{ $.ok IS TRUE }", msg));
        assert!(m("{ $.gone IS NULL }", msg));
        assert!(m("{ $.missing NOT EXISTS }", msg));
        assert!(!m("{ $.latency NOT EXISTS }", msg));
        assert!(m("{ $.eventType = %Upd.*% }", msg));
        assert!(!m(r#"{ $.eventType = "x" }"#, "not json"));
    }

    #[test]
    fn space_delimited_patterns() {
        let line =
            r#"127.0.0.1 - frank [10/Oct/2000:13:25:15 -0700] "GET /index.html HTTP/1.0" 404 1534"#;
        assert!(m(
            "[ip, user, username, timestamp, request, status_code = 404, bytes]",
            line
        ));
        assert!(m(
            "[ip, user, username, timestamp, request, status_code = 4*, bytes]",
            line
        ));
        assert!(!m(
            "[ip, user, username, timestamp, request, status_code = 5*, bytes]",
            line
        ));
        assert!(m("[..., status_code = 404, bytes > 1000]", line));
        assert!(m("[ip = 127.0.0.1, ...]", line));
        assert!(m("[..., code = 404 || code = 500, bytes]", line));
        let caps = parse("[ip, user, username, timestamp, request, status_code, bytes]")
            .unwrap()
            .matches(line)
            .unwrap();
        assert_eq!(caps.select("$bytes").as_deref(), Some("1534"));
        assert_eq!(
            caps.select("$request").as_deref(),
            Some("GET /index.html HTTP/1.0")
        );
    }

    #[test]
    fn json_selectors_feed_metric_values() {
        let caps = parse("{ $.latency > 0 }")
            .unwrap()
            .matches(r#"{"latency":42,"route":"/a"}"#)
            .unwrap();
        assert_eq!(caps.select("$.latency").as_deref(), Some("42"));
        assert_eq!(caps.select("$.route").as_deref(), Some("/a"));
        assert_eq!(caps.select("$.none"), None);
    }

    #[test]
    fn rejects_malformed_patterns() {
        assert!(parse("{ $.a = }").is_err());
        assert!(parse("{ $.a = 1").is_err());
        assert!(parse("%(%").is_err());
    }
}
