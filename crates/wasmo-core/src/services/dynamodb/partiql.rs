use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::codec::AwsError;
use crate::service::{InputExt, OpResult};

use super::attr::{Attr, Item, item_to_value};
use super::expr::eval::{apply, matches, project};
use super::expr::{Bindings, CmpOp, Cond, Kw, Parser, Path, Tok};
use super::number::normalize;
use super::{Region, err, validation};

fn syntax(message: impl std::fmt::Display) -> AwsError {
    validation(format!(
        "Statement wasn't well formed, can't be processed: {message}"
    ))
}

#[derive(Debug, Clone, PartialEq)]
enum PTok {
    Word(String),
    Quoted(String),
    Str(String),
    Num(String),
    Param,
    Sym(&'static str),
}

fn lex(s: &str) -> Result<Vec<PTok>, AwsError> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            let mut j = i + 1;
            let mut text = String::new();
            loop {
                match c.get(j) {
                    None => return Err(syntax("unterminated literal")),
                    Some(&q) if q == ch && c.get(j + 1) == Some(&ch) => {
                        text.push(ch);
                        j += 2;
                    }
                    Some(&q) if q == ch => break,
                    Some(&x) => {
                        text.push(x);
                        j += 1;
                    }
                }
            }
            out.push(if ch == '"' {
                PTok::Quoted(text)
            } else {
                PTok::Str(text)
            });
            i = j + 1;
            continue;
        }
        let negative_number = ch == '-'
            && c.get(i + 1).is_some_and(char::is_ascii_digit)
            && !matches!(
                out.last(),
                Some(
                    PTok::Word(_)
                        | PTok::Quoted(_)
                        | PTok::Num(_)
                        | PTok::Str(_)
                        | PTok::Param
                        | PTok::Sym(")" | "]")
                )
            );
        if ch.is_ascii_digit() || negative_number {
            let mut j = i + 1;
            while j < c.len()
                && (c[j].is_ascii_alphanumeric()
                    || c[j] == '.'
                    || ((c[j] == '-' || c[j] == '+') && matches!(c[j - 1], 'e' | 'E')))
            {
                j += 1;
            }
            out.push(PTok::Num(c[i..j].iter().collect()));
            i = j;
            continue;
        }
        if ch.is_ascii_alphabetic() || ch == '_' {
            let table_name = matches!(
                out.last(),
                Some(PTok::Word(w)) if ["FROM", "INTO", "UPDATE"].iter().any(|k| w.eq_ignore_ascii_case(k))
            );
            let mut j = i;
            while j < c.len()
                && (c[j].is_ascii_alphanumeric() || c[j] == '_' || (table_name && c[j] == '-'))
            {
                j += 1;
            }
            out.push(PTok::Word(c[i..j].iter().collect()));
            i = j;
            continue;
        }
        if ch == '?' {
            out.push(PTok::Param);
            i += 1;
            continue;
        }
        let two: String = c[i..(i + 2).min(c.len())].iter().collect();
        let sym = match two.as_str() {
            "<=" => "<=",
            ">=" => ">=",
            "<>" => "<>",
            "!=" => "<>",
            "<<" => "<<",
            ">>" => ">>",
            _ => match ch {
                '(' => "(",
                ')' => ")",
                '[' => "[",
                ']' => "]",
                '{' => "{",
                '}' => "}",
                ',' => ",",
                '.' => ".",
                '=' => "=",
                '<' => "<",
                '>' => ">",
                ':' => ":",
                '*' => "*",
                '+' => "+",
                '-' => "-",
                _ => return Err(syntax(format!("unexpected character '{ch}'"))),
            },
        };
        i += sym.len();
        out.push(PTok::Sym(sym));
    }
    Ok(out)
}

struct Stmt<'p> {
    toks: Vec<PTok>,
    pos: usize,
    params: &'p [Attr],
    next_param: usize,
    names: BTreeMap<String, String>,
    values: BTreeMap<String, Attr>,
}

impl<'p> Stmt<'p> {
    fn peek(&self) -> Option<&PTok> {
        self.toks.get(self.pos)
    }

    fn word_is(&self, w: &str) -> bool {
        matches!(self.peek(), Some(PTok::Word(x)) if x.eq_ignore_ascii_case(w))
    }

    fn eat_word(&mut self, w: &str) -> bool {
        let ok = self.word_is(w);
        if ok {
            self.pos += 1;
        }
        ok
    }

    fn expect_word(&mut self, w: &str) -> Result<(), AwsError> {
        if self.eat_word(w) {
            Ok(())
        } else {
            Err(syntax(format!("expected {w}")))
        }
    }

    fn eat_sym(&mut self, s: &str) -> bool {
        let ok = matches!(self.peek(), Some(PTok::Sym(x)) if *x == s);
        if ok {
            self.pos += 1;
        }
        ok
    }

    fn expect_sym(&mut self, s: &str) -> Result<(), AwsError> {
        if self.eat_sym(s) {
            Ok(())
        } else {
            Err(syntax(format!("expected '{s}'")))
        }
    }

    fn ident(&mut self) -> Result<String, AwsError> {
        match self.toks.get(self.pos).cloned() {
            Some(PTok::Quoted(s) | PTok::Word(s)) => {
                self.pos += 1;
                Ok(s)
            }
            _ => Err(syntax("expected an identifier")),
        }
    }

    fn param(&mut self) -> Result<Attr, AwsError> {
        let v = self.params.get(self.next_param).cloned().ok_or_else(|| {
            validation("Number of parameters in request and statement don't match.")
        })?;
        self.next_param += 1;
        Ok(v)
    }

    fn literal(&mut self) -> Result<Attr, AwsError> {
        let t = self
            .toks
            .get(self.pos)
            .cloned()
            .ok_or_else(|| syntax("unexpected end of statement"))?;
        self.pos += 1;
        Ok(match t {
            PTok::Param => self.param()?,
            PTok::Str(s) => Attr::S(s),
            PTok::Num(n) => Attr::N(normalize(&n)?),
            PTok::Word(w) if w.eq_ignore_ascii_case("true") => Attr::Bool(true),
            PTok::Word(w) if w.eq_ignore_ascii_case("false") => Attr::Bool(false),
            PTok::Word(w) if w.eq_ignore_ascii_case("null") => Attr::Null,
            PTok::Sym("{") => {
                let mut m = Item::new();
                if !self.eat_sym("}") {
                    loop {
                        let k = match self.toks.get(self.pos).cloned() {
                            Some(PTok::Str(k) | PTok::Quoted(k)) => k,
                            _ => return Err(syntax("expected a string key in a tuple")),
                        };
                        self.pos += 1;
                        self.expect_sym(":")?;
                        let v = self.literal()?;
                        m.insert(k, v);
                        if self.eat_sym("}") {
                            break;
                        }
                        self.expect_sym(",")?;
                    }
                }
                Attr::M(m)
            }
            PTok::Sym("[") => {
                let mut l = Vec::new();
                if !self.eat_sym("]") {
                    loop {
                        l.push(self.literal()?);
                        if self.eat_sym("]") {
                            break;
                        }
                        self.expect_sym(",")?;
                    }
                }
                Attr::L(l)
            }
            PTok::Sym("<<") => {
                let mut items = Vec::new();
                loop {
                    items.push(self.literal()?);
                    if self.eat_sym(">>") {
                        break;
                    }
                    self.expect_sym(",")?;
                }
                let set = match items.first() {
                    Some(Attr::S(_)) => {
                        json!({ "SS": items.iter().map(|a| match a { Attr::S(s) => json!(s), _ => Value::Null }).collect::<Vec<_>>() })
                    }
                    Some(Attr::N(_)) => {
                        json!({ "NS": items.iter().map(|a| match a { Attr::N(s) => json!(s), _ => Value::Null }).collect::<Vec<_>>() })
                    }
                    Some(Attr::B(_)) => {
                        json!({ "BS": items.iter().map(|a| match a { Attr::B(b) => json!(STANDARD.encode(b)), _ => Value::Null }).collect::<Vec<_>>() })
                    }
                    _ => return Err(syntax("unsupported set element")),
                };
                Attr::from_value(&set)?
            }
            _ => return Err(syntax("expected a value")),
        })
    }

    fn bind_name(&mut self, name: String) -> Tok {
        let key = format!("#p{}", self.names.len());
        self.names.insert(key.clone(), name);
        Tok::Name(key)
    }

    fn bind_value(&mut self, v: Attr) -> Tok {
        let key = format!(":p{}", self.values.len());
        self.values.insert(key.clone(), v);
        Tok::Value(key)
    }

    fn path_tokens(&mut self, out: &mut Vec<Tok>) -> Result<(), AwsError> {
        let root = self.ident()?;
        let t = self.bind_name(root);
        out.push(t);
        loop {
            if self.eat_sym(".") {
                let f = self.ident()?;
                out.push(Tok::Dot);
                let t = self.bind_name(f);
                out.push(t);
            } else if self.peek() == Some(&PTok::Sym("[")) {
                self.pos += 1;
                let Some(PTok::Num(n)) = self.peek().cloned() else {
                    return Err(syntax("expected a list index"));
                };
                self.pos += 1;
                self.expect_sym("]")?;
                out.extend([Tok::LBracket, Tok::Int(n), Tok::RBracket]);
            } else {
                return Ok(());
            }
        }
    }

    fn is_value_start(&self) -> bool {
        match self.peek() {
            Some(PTok::Str(_) | PTok::Num(_) | PTok::Param) => true,
            Some(PTok::Sym(s)) => matches!(*s, "{" | "[" | "<<"),
            Some(PTok::Word(w)) => ["true", "false", "null"]
                .iter()
                .any(|k| w.eq_ignore_ascii_case(k)),
            _ => false,
        }
    }

    fn translate(&mut self, stop: &[&str]) -> Result<Vec<Tok>, AwsError> {
        let mut out = Vec::new();
        let mut depth = 0usize;
        while let Some(t) = self.peek().cloned() {
            if depth == 0 && stop.iter().any(|w| self.word_is(w)) {
                break;
            }
            if depth == 0 && t == PTok::Sym(",") && stop.contains(&",") {
                break;
            }
            if self.is_value_start() {
                let v = self.literal()?;
                let t = self.bind_value(v);
                out.push(t);
                continue;
            }
            match t {
                PTok::Word(w)
                    if self.toks.get(self.pos + 1) == Some(&PTok::Sym("(")) && !is_keyword(&w) =>
                {
                    self.pos += 1;
                    out.push(Tok::Ident(w));
                }
                PTok::Word(w) if w.eq_ignore_ascii_case("IS") => {
                    self.pos += 1;
                    let negate = self.eat_word("NOT");
                    let start = path_start(&out).ok_or_else(|| syntax("IS needs an attribute"))?;
                    let path: Vec<Tok> = out.drain(start..).collect();
                    let func = if self.eat_word("MISSING") {
                        if negate {
                            "attribute_exists"
                        } else {
                            "attribute_not_exists"
                        }
                    } else if self.eat_word("NULL") {
                        if negate {
                            out.push(Tok::Kw(Kw::Not));
                        }
                        "attribute_type"
                    } else {
                        return Err(syntax("expected MISSING or NULL after IS"));
                    };
                    out.push(Tok::Ident(func.into()));
                    out.push(Tok::LParen);
                    out.extend(path);
                    if func == "attribute_type" {
                        out.push(Tok::Comma);
                        let t = self.bind_value(Attr::S("NULL".into()));
                        out.push(t);
                    }
                    out.push(Tok::RParen);
                }
                PTok::Word(w) if is_keyword(&w) => {
                    self.pos += 1;
                    let kw = match w.to_ascii_uppercase().as_str() {
                        "AND" => Kw::And,
                        "OR" => Kw::Or,
                        "NOT" => Kw::Not,
                        "BETWEEN" => Kw::Between,
                        "IN" => Kw::In,
                        _ => return Err(syntax(format!("unexpected keyword {w}"))),
                    };
                    out.push(Tok::Kw(kw));
                    if kw == Kw::In && self.eat_sym("[") {
                        out.push(Tok::LParen);
                        loop {
                            let v = self.literal()?;
                            let t = self.bind_value(v);
                            out.push(t);
                            if self.eat_sym("]") {
                                break;
                            }
                            self.expect_sym(",")?;
                            out.push(Tok::Comma);
                        }
                        out.push(Tok::RParen);
                    }
                }
                PTok::Word(_) | PTok::Quoted(_) => self.path_tokens(&mut out)?,
                PTok::Sym(s) => {
                    self.pos += 1;
                    out.push(match s {
                        "(" => {
                            depth += 1;
                            Tok::LParen
                        }
                        ")" => {
                            depth = depth.saturating_sub(1);
                            Tok::RParen
                        }
                        "," => Tok::Comma,
                        "=" => Tok::Cmp(CmpOp::Eq),
                        "<>" => Tok::Cmp(CmpOp::Ne),
                        "<" => Tok::Cmp(CmpOp::Lt),
                        "<=" => Tok::Cmp(CmpOp::Le),
                        ">" => Tok::Cmp(CmpOp::Gt),
                        ">=" => Tok::Cmp(CmpOp::Ge),
                        "+" => Tok::Plus,
                        "-" => Tok::Minus,
                        other => return Err(syntax(format!("unexpected '{other}'"))),
                    });
                }
                _ => return Err(syntax("unexpected token")),
            }
        }
        out.push(Tok::Eof);
        Ok(out)
    }

    fn bindings(&self) -> Bindings {
        let mut b = Bindings::with(self.names.clone(), self.values.clone());
        b.allow_reserved = true;
        b
    }

    fn condition(&mut self) -> Result<Option<Cond>, AwsError> {
        if !self.eat_word("WHERE") {
            return Ok(None);
        }
        let toks = self.translate(&[])?;
        let mut b = self.bindings();
        let mut p = Parser::from_tokens("WHERE clause", toks, &mut b);
        let c = p.condition()?;
        p.expect_end()?;
        Ok(Some(c))
    }
}

fn is_keyword(w: &str) -> bool {
    ["AND", "OR", "NOT", "BETWEEN", "IN", "IS"]
        .iter()
        .any(|k| w.eq_ignore_ascii_case(k))
}

fn path_start(toks: &[Tok]) -> Option<usize> {
    let mut i = toks.len();
    loop {
        match toks.get(i.checked_sub(1)?)? {
            Tok::RBracket => i = i.checked_sub(3)?,
            Tok::Name(_) => {
                i -= 1;
                if i > 0 && toks[i - 1] == Tok::Dot {
                    i -= 1;
                } else {
                    return Some(i);
                }
            }
            _ => return None,
        }
    }
}

fn parameters(v: &Value) -> Result<Vec<Attr>, AwsError> {
    v.array_field("Parameters")
        .iter()
        .map(Attr::from_value)
        .collect()
}

fn run(
    region: &mut Region,
    statement: &str,
    params: &[Attr],
    input: &Value,
) -> Result<Value, AwsError> {
    let mut s = Stmt {
        toks: lex(statement)?,
        pos: 0,
        params,
        next_param: 0,
        names: BTreeMap::new(),
        values: BTreeMap::new(),
    };
    let out = if s.eat_word("SELECT") {
        select(region, &mut s, input)?
    } else if s.eat_word("INSERT") {
        s.expect_word("INTO")?;
        let name = s.ident()?;
        s.expect_word("VALUE")?;
        let Attr::M(item) = s.literal()? else {
            return Err(syntax("INSERT expects a tuple value"));
        };
        let t = region.table_mut(&name)?;
        let pk = t.pk_of_item(&item)?;
        if t.items.contains_key(&pk) {
            return Err(err(
                "DuplicateItemException",
                "Duplicate primary key exists in table",
            ));
        }
        t.items.insert(pk, item);
        json!({ "Items": [] })
    } else if s.eat_word("UPDATE") {
        let name = s.ident()?;
        let mut toks = Vec::new();
        loop {
            if s.eat_word("SET") {
                toks.push(Tok::Kw(Kw::Set));
                loop {
                    s.path_tokens(&mut toks)?;
                    s.expect_sym("=")?;
                    toks.push(Tok::Cmp(CmpOp::Eq));
                    let mut v = s.translate(&["SET", "REMOVE", "WHERE", ","])?;
                    v.pop();
                    toks.extend(v);
                    if !s.eat_sym(",") {
                        break;
                    }
                    toks.push(Tok::Comma);
                }
            } else if s.eat_word("REMOVE") {
                toks.push(Tok::Kw(Kw::Remove));
                loop {
                    s.path_tokens(&mut toks)?;
                    if !s.eat_sym(",") {
                        break;
                    }
                    toks.push(Tok::Comma);
                }
            } else {
                break;
            }
        }
        toks.push(Tok::Eof);
        let mut b = s.bindings();
        let update = Parser::from_tokens("UpdateExpression", toks, &mut b).update()?;
        let cond = s
            .condition()?
            .ok_or_else(|| syntax("UPDATE requires a WHERE clause"))?;
        let t = region.table_mut(&name)?;
        let keys: Vec<_> = t
            .items
            .iter()
            .filter(|(_, it)| matches(&cond, it))
            .map(|(k, _)| k.clone())
            .collect();
        if keys.is_empty() {
            return Err(err(
                "ConditionalCheckFailedException",
                "The conditional request failed",
            ));
        }
        for k in keys {
            let new = apply(&update, &t.items[&k])?;
            t.pk_of_item(&new)?;
            t.items.insert(k, new);
        }
        json!({ "Items": [] })
    } else if s.eat_word("DELETE") {
        s.expect_word("FROM")?;
        let name = s.ident()?;
        let cond = s
            .condition()?
            .ok_or_else(|| syntax("DELETE requires a WHERE clause"))?;
        let t = region.table_mut(&name)?;
        t.items.retain(|_, it| !matches(&cond, it));
        json!({ "Items": [] })
    } else {
        return Err(syntax("unsupported statement"));
    };
    if s.pos != s.toks.len() {
        return Err(syntax("unexpected trailing tokens"));
    }
    if s.next_param != params.len() {
        return Err(validation(
            "Number of parameters in request and statement don't match.",
        ));
    }
    Ok(out)
}

fn select(region: &mut Region, s: &mut Stmt<'_>, input: &Value) -> Result<Value, AwsError> {
    let mut paths: Option<Vec<Path>> = None;
    if !s.eat_sym("*") {
        let mut toks = Vec::new();
        loop {
            s.path_tokens(&mut toks)?;
            if !s.eat_sym(",") {
                break;
            }
            toks.push(Tok::Comma);
        }
        toks.push(Tok::Eof);
        let mut b = s.bindings();
        paths = Some(Parser::from_tokens("ProjectionExpression", toks, &mut b).projection()?);
    }
    s.expect_word("FROM")?;
    let name = s.ident()?;
    let index = if s.eat_sym(".") {
        Some(s.ident()?)
    } else {
        None
    };
    let cond = s.condition()?;
    let t = region.table(&name)?;
    let index = index.map(|i| t.index(&i)).transpose()?;
    let offset = input
        .str_field("NextToken")
        .and_then(|t| STANDARD.decode(t).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    let limit = input.i64_field("Limit").map(|l| l.max(1) as usize);
    let mut matched = t
        .items
        .values()
        .filter(|it| index.is_none_or(|i| t.in_index(i, it)))
        .map(|it| index.map_or_else(|| it.clone(), |i| t.index_view(i, it)))
        .filter(|it| cond.as_ref().is_none_or(|c| matches(c, it)))
        .skip(offset);
    let mut items = Vec::new();
    for it in matched.by_ref() {
        if limit.is_some_and(|l| items.len() >= l) {
            break;
        }
        items.push(item_to_value(&match &paths {
            Some(p) => project(&it, p),
            None => it,
        }));
    }
    let mut out = json!({});
    if limit.is_some_and(|l| items.len() >= l) && matched.next().is_some() {
        out["NextToken"] = json!(STANDARD.encode((offset + items.len()).to_string()));
    }
    out["Items"] = Value::Array(items);
    Ok(out)
}

fn table_of(statement: &str) -> Option<String> {
    let toks = lex(statement).ok()?;
    let pos = toks.iter().position(|t| {
        matches!(t, PTok::Word(w) if ["FROM", "INTO", "UPDATE"].iter().any(|k| w.eq_ignore_ascii_case(k)))
    })?;
    match toks.get(pos + 1)? {
        PTok::Quoted(s) | PTok::Word(s) => Some(s.clone()),
        _ => None,
    }
}

pub fn execute(region: &mut Region, input: &Value) -> OpResult {
    let statement = input
        .str_field("Statement")
        .ok_or_else(|| validation("1 validation error detected: Value null at 'statement' failed to satisfy constraint: Member must not be null"))?;
    run(region, statement, &parameters(input)?, input)
}

pub fn batch_execute(region: &mut Region, input: &Value) -> OpResult {
    let statements = input.array_field("Statements");
    if statements.is_empty() || statements.len() > 25 {
        return Err(validation(
            "1 validation error detected: Value at 'statements' failed to satisfy constraint: Member must have length less than or equal to 25",
        ));
    }
    let mut responses = Vec::new();
    for st in statements {
        let text = st.str_field("Statement").unwrap_or_default();
        let table = table_of(text);
        let result = parameters(st).and_then(|p| run(region, text, &p, &json!({})));
        let mut r = json!({});
        if let Some(t) = &table {
            r["TableName"] = json!(t);
        }
        match result {
            Ok(out) => {
                if let Some(first) = out["Items"].as_array().and_then(|a| a.first()) {
                    r["Item"] = first.clone();
                }
            }
            Err(e) => {
                let code = e.code.trim_end_matches("Exception");
                r["Error"] = json!({ "Code": code, "Message": e.message });
            }
        }
        responses.push(r);
    }
    Ok(json!({ "Responses": responses }))
}

pub fn execute_transaction(region: &mut Region, input: &Value) -> OpResult {
    let statements = input.array_field("TransactStatements");
    if statements.is_empty() || statements.len() > 100 {
        return Err(validation(
            "1 validation error detected: Value at 'transactStatements' failed to satisfy constraint: Member must have length less than or equal to 100",
        ));
    }
    let snapshot = region.tables.clone();
    let mut responses = Vec::new();
    for (i, st) in statements.iter().enumerate() {
        let text = st.str_field("Statement").unwrap_or_default();
        let result = parameters(st).and_then(|p| run(region, text, &p, &json!({})));
        match result {
            Ok(out) => responses.push(match out["Items"].as_array().and_then(|a| a.first()) {
                Some(item) => json!({ "Item": item }),
                None => json!({}),
            }),
            Err(e) if e.code == "ValidationException" || e.code == "ResourceNotFoundException" => {
                region.tables = snapshot;
                return Err(e);
            }
            Err(e) => {
                region.tables = snapshot;
                let reasons: Vec<Value> = (0..statements.len())
                    .map(|j| {
                        if j == i {
                            json!({ "Code": e.code.trim_end_matches("Exception"), "Message": e.message })
                        } else {
                            json!({ "Code": "None" })
                        }
                    })
                    .collect();
                let codes: Vec<String> = reasons
                    .iter()
                    .map(|r| r["Code"].as_str().unwrap_or_default().to_string())
                    .collect();
                let mut c = err(
                    "TransactionCanceledException",
                    format!(
                        "Transaction cancelled, please refer cancellation reasons for specific reasons [{}]",
                        codes.join(", ")
                    ),
                );
                c.data
                    .insert("CancellationReasons".into(), Value::Array(reasons));
                return Err(c);
            }
        }
    }
    Ok(json!({ "Responses": responses }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::dynamodb::attr::item_from_value;

    fn region() -> Region {
        let mut r = Region::default();
        let mut env = crate::env::FixedEnv {
            now_ms: 0.0,
            seed: 7,
        };
        let mut ctx =
            crate::service::Ctx::new("000000000000", "us-east-1", "http://wasmo.local", &mut env);
        crate::services::dynamodb::table::create(
            &mut r,
            &mut ctx,
            &json!({
                "TableName": "users",
                "AttributeDefinitions": [{"AttributeName": "id", "AttributeType": "S"}],
                "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}],
                "BillingMode": "PAY_PER_REQUEST",
            }),
        )
        .unwrap();
        r
    }

    fn exec(r: &mut Region, s: &str, params: Value) -> OpResult {
        execute(r, &json!({ "Statement": s, "Parameters": params }))
    }

    #[test]
    fn insert_select_update_delete() {
        let mut r = region();
        exec(&mut r, "INSERT INTO users VALUE {'id': 'a', 'n': 1, 'tags': <<'x', 'y'>>, 'm': {'k': [true, null]}}", json!([])).unwrap();
        exec(
            &mut r,
            "INSERT INTO \"users\" VALUE {'id': ?, 'n': -2.50}",
            json!([{"S": "b"}]),
        )
        .unwrap();
        let dup = exec(&mut r, "INSERT INTO users VALUE {'id': 'a'}", json!([])).unwrap_err();
        assert_eq!(dup.code, "DuplicateItemException");
        let out = exec(
            &mut r,
            "SELECT id, n FROM users WHERE n > ? AND tags IS NOT MISSING",
            json!([{"N": "0"}]),
        )
        .unwrap();
        assert_eq!(out["Items"], json!([{"id": {"S": "a"}, "n": {"N": "1"}}]));
        exec(
            &mut r,
            "UPDATE users SET n = n + 1, extra = 'x' REMOVE tags WHERE id = 'a'",
            json!([]),
        )
        .unwrap();
        let t = r.table("users").unwrap();
        let a = t
            .items
            .values()
            .find(|it| it["id"] == Attr::S("a".into()))
            .unwrap();
        assert_eq!(item_to_value(a)["n"], json!({"N": "2"}));
        assert!(a.get("tags").is_none());
        let out = exec(
            &mut r,
            "SELECT * FROM users WHERE id IN ['b', 'zzz']",
            json!([]),
        )
        .unwrap();
        assert_eq!(out["Items"][0]["n"], json!({"N": "-2.5"}));
        exec(&mut r, "DELETE FROM users WHERE id = 'b'", json!([])).unwrap();
        assert_eq!(r.table("users").unwrap().items.len(), 1);
        let missing = exec(
            &mut r,
            "UPDATE users SET n = 1 WHERE id = 'nope'",
            json!([]),
        )
        .unwrap_err();
        assert_eq!(missing.code, "ConditionalCheckFailedException");
    }

    #[test]
    fn transaction_rolls_back() {
        let mut r = region();
        let e = execute_transaction(
            &mut r,
            &json!({ "TransactStatements": [
                {"Statement": "INSERT INTO users VALUE {'id': 'x'}"},
                {"Statement": "UPDATE users SET n = 1 WHERE id = 'nope'"},
            ]}),
        )
        .unwrap_err();
        assert_eq!(e.code, "TransactionCanceledException");
        assert!(r.table("users").unwrap().items.is_empty());
        let item = item_from_value(&json!({"id": {"S": "x"}})).unwrap();
        assert!(r.table("users").unwrap().pk_of_item(&item).is_ok());
    }
}
