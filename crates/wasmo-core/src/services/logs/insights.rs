//! A subset of the CloudWatch Logs Insights query language: `fields`,
//! `display`, `filter` (comparisons, `like`, `and` / `or` / `not`), `sort`,
//! `limit`, and `stats` with `count` / `sum` / `avg` / `min` / `max` and `by`.

use std::cmp::Ordering;

use indexmap::IndexMap;
use regex_lite::Regex;
use serde_json::Value;

pub type Row = IndexMap<String, String>;

#[derive(Debug, Clone)]
enum Cond {
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
    Not(Box<Cond>),
    Like(String, Regex),
    Cmp(String, String, Lit),
}

#[derive(Debug, Clone)]
enum Lit {
    Str(String),
    Num(f64),
}

#[derive(Debug, Clone)]
struct Agg {
    func: String,
    field: Option<String>,
    alias: String,
}

#[derive(Debug, Clone)]
enum Stage {
    Fields(Vec<String>),
    Filter(Cond),
    Sort(Vec<(String, bool)>),
    Limit(usize),
    Stats(Vec<Agg>, Vec<String>),
}

fn split_top(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let (mut depth, mut quote, mut cur) = (0, None, String::new());
    for c in s.chars() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'' | '/') if !(c == '/' && depth > 0) => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth -= 1,
            (None, _) if c == sep && depth == 0 => {
                out.push(std::mem::take(&mut cur).trim().to_string());
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn lit(t: &str) -> Lit {
    let t = t.trim();
    for q in ['"', '\''] {
        if let Some(s) = t.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
            return Lit::Str(s.to_string());
        }
    }
    t.parse()
        .map(Lit::Num)
        .unwrap_or_else(|_| Lit::Str(t.to_string()))
}

fn find_word(s: &str, word: &str) -> Option<usize> {
    let lower = s.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(word) {
        let at = from + i;
        let before = at == 0 || lower.as_bytes()[at - 1] == b' ';
        let after = lower
            .as_bytes()
            .get(at + word.len())
            .is_none_or(|b| *b == b' ');
        let inside =
            lower[..at].matches('"').count() % 2 == 1 || lower[..at].matches('/').count() % 2 == 1;
        if before && after && !inside {
            return Some(at);
        }
        from = at + word.len();
    }
    None
}

fn cond(s: &str) -> Result<Cond, String> {
    let s = s.trim();
    if let Some(at) = find_word(s, "or") {
        return Ok(Cond::Or(
            Box::new(cond(&s[..at])?),
            Box::new(cond(&s[at + 2..])?),
        ));
    }
    if let Some(at) = find_word(s, "and") {
        return Ok(Cond::And(
            Box::new(cond(&s[..at])?),
            Box::new(cond(&s[at + 3..])?),
        ));
    }
    if let Some(inner) = s.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
        return cond(inner);
    }
    if s.len() > 4 && s[..4].eq_ignore_ascii_case("not ") {
        return Ok(Cond::Not(Box::new(cond(&s[4..])?)));
    }
    if let Some(at) = find_word(s, "like") {
        let field = s[..at].trim();
        let (field, negate) = match field
            .strip_suffix(" not")
            .or_else(|| field.strip_suffix(" NOT"))
        {
            Some(f) => (f.trim(), true),
            None => (field, false),
        };
        let pat = s[at + 4..].trim();
        let re = if let Some(body) = pat.strip_prefix('/') {
            let (body, flags) = body.rsplit_once('/').ok_or("unterminated regex in like")?;
            let prefix = if flags.contains('i') { "(?i)" } else { "" };
            Regex::new(&format!("{prefix}{body}")).map_err(|e| e.to_string())?
        } else {
            let Lit::Str(text) = lit(pat) else {
                return Err(format!("like expects a string or regex, got {pat}"));
            };
            Regex::new(&regex_lite::escape(&text)).map_err(|e| e.to_string())?
        };
        let like = Cond::Like(field.to_string(), re);
        return Ok(if negate {
            Cond::Not(Box::new(like))
        } else {
            like
        });
    }
    for op in ["!=", "<=", ">=", "=", "<", ">"] {
        if let Some((l, r)) = s.split_once(op) {
            return Ok(Cond::Cmp(l.trim().to_string(), op.to_string(), lit(r)));
        }
    }
    Err(format!("unsupported filter expression: {s}"))
}

fn agg(s: &str) -> Result<Agg, String> {
    let (expr, alias) = match find_word(s, "as") {
        Some(at) => (s[..at].trim(), Some(s[at + 2..].trim().to_string())),
        None => (s.trim(), None),
    };
    let (func, arg) = expr
        .strip_suffix(')')
        .and_then(|e| e.split_once('('))
        .ok_or_else(|| format!("unsupported stats expression: {s}"))?;
    let func = func.trim().to_ascii_lowercase();
    if !["count", "sum", "avg", "min", "max"].contains(&func.as_str()) {
        return Err(format!("unsupported stats function: {func}"));
    }
    let arg = arg.trim();
    Ok(Agg {
        alias: alias.unwrap_or_else(|| expr.replace(' ', "")),
        field: (!arg.is_empty() && arg != "*").then(|| arg.to_string()),
        func,
    })
}

fn parse(query: &str) -> Result<Vec<Stage>, String> {
    let mut stages = Vec::new();
    for part in split_top(query, '|') {
        let (cmd, rest) = part.split_once(char::is_whitespace).unwrap_or((&part, ""));
        let rest = rest.trim();
        stages.push(match cmd.to_ascii_lowercase().as_str() {
            "fields" | "display" => Stage::Fields(split_top(rest, ',')),
            "filter" => Stage::Filter(cond(rest)?),
            "sort" => Stage::Sort(
                split_top(rest, ',')
                    .iter()
                    .map(|k| {
                        let mut it = k.split_whitespace();
                        let field = it.next().unwrap_or_default().to_string();
                        (
                            field,
                            !it.next().is_some_and(|d| d.eq_ignore_ascii_case("asc")),
                        )
                    })
                    .collect(),
            ),
            "limit" => Stage::Limit(rest.parse().map_err(|_| format!("invalid limit: {rest}"))?),
            "stats" => {
                let (aggs, by) = match find_word(rest, "by") {
                    Some(at) => (&rest[..at], split_top(&rest[at + 2..], ',')),
                    None => (rest, Vec::new()),
                };
                Stage::Stats(
                    split_top(aggs, ',')
                        .iter()
                        .map(|a| agg(a))
                        .collect::<Result<_, _>>()?,
                    by,
                )
            }
            other => return Err(format!("unsupported query command: {other}")),
        });
    }
    Ok(stages)
}

fn num(s: &str) -> Option<f64> {
    s.parse().ok()
}

fn order(a: Option<&String>, b: Option<&String>) -> Ordering {
    match (a.and_then(|x| num(x)), b.and_then(|x| num(x))) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        _ => a.cmp(&b),
    }
}

fn eval(c: &Cond, row: &Row) -> bool {
    match c {
        Cond::And(a, b) => eval(a, row) && eval(b, row),
        Cond::Or(a, b) => eval(a, row) || eval(b, row),
        Cond::Not(a) => !eval(a, row),
        Cond::Like(f, re) => row.get(f).is_some_and(|v| re.is_match(v)),
        Cond::Cmp(f, op, lit) => {
            let Some(v) = row.get(f) else {
                return false;
            };
            let ord = match lit {
                Lit::Num(n) => match num(v) {
                    Some(x) => x.partial_cmp(n).unwrap_or(Ordering::Equal),
                    None => return op == "!=",
                },
                Lit::Str(s) => v.as_str().cmp(s.as_str()),
            };
            match op.as_str() {
                "=" => ord == Ordering::Equal,
                "!=" => ord != Ordering::Equal,
                "<" => ord == Ordering::Less,
                "<=" => ord != Ordering::Greater,
                ">" => ord == Ordering::Greater,
                _ => ord != Ordering::Less,
            }
        }
    }
}

fn format_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{n:.0}")
    } else {
        n.to_string()
    }
}

fn stats(rows: &[Row], aggs: &[Agg], by: &[String]) -> Vec<Row> {
    let mut groups: IndexMap<Vec<String>, Vec<&Row>> = IndexMap::new();
    for r in rows {
        let key = by
            .iter()
            .map(|f| r.get(f).cloned().unwrap_or_default())
            .collect();
        groups.entry(key).or_default().push(r);
    }
    if groups.is_empty() && by.is_empty() {
        groups.insert(Vec::new(), Vec::new());
    }
    groups
        .into_iter()
        .map(|(key, members)| {
            let mut out: Row = by.iter().cloned().zip(key).collect();
            for a in aggs {
                let vals: Vec<f64> = members
                    .iter()
                    .filter_map(|r| r.get(a.field.as_deref()?).and_then(|v| num(v)))
                    .collect();
                let v = match a.func.as_str() {
                    "count" => match &a.field {
                        Some(f) => members.iter().filter(|r| r.contains_key(f)).count() as f64,
                        None => members.len() as f64,
                    },
                    "sum" => vals.iter().sum(),
                    "avg" if vals.is_empty() => continue,
                    "avg" => vals.iter().sum::<f64>() / vals.len() as f64,
                    "min" => match vals.iter().copied().reduce(f64::min) {
                        Some(v) => v,
                        None => continue,
                    },
                    _ => match vals.iter().copied().reduce(f64::max) {
                        Some(v) => v,
                        None => continue,
                    },
                };
                out.insert(a.alias.clone(), format_num(v));
            }
            out
        })
        .collect()
}

/// Flattens a JSON log message into Insights fields (`a.b`, `list.0`).
pub fn discover(prefix: &str, v: &Value, row: &mut Row) {
    match v {
        Value::Object(o) => {
            for (k, x) in o {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                discover(&key, x, row);
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                discover(&format!("{prefix}.{i}"), x, row);
            }
        }
        Value::String(s) => {
            row.insert(prefix.to_string(), s.clone());
        }
        Value::Null => {}
        other => {
            row.insert(prefix.to_string(), other.to_string());
        }
    }
}

/// Runs `query` over `rows` and returns the result rows.
pub fn run(query: &str, mut rows: Vec<Row>) -> Result<Vec<Row>, String> {
    let mut fields: Option<Vec<String>> = None;
    for stage in parse(query)? {
        match stage {
            Stage::Fields(f) => fields = Some(f),
            Stage::Filter(c) => rows.retain(|r| eval(&c, r)),
            Stage::Sort(keys) => rows.sort_by(|a, b| {
                keys.iter()
                    .map(|(f, desc)| {
                        let o = order(a.get(f), b.get(f));
                        if *desc { o.reverse() } else { o }
                    })
                    .find(|o| *o != Ordering::Equal)
                    .unwrap_or(Ordering::Equal)
            }),
            Stage::Limit(n) => rows.truncate(n),
            Stage::Stats(aggs, by) => {
                rows = stats(&rows, &aggs, &by);
                fields = None;
            }
        }
    }
    Ok(match fields {
        Some(f) => rows
            .into_iter()
            .map(|r| {
                f.iter()
                    .filter_map(|k| Some((k.clone(), r.get(k)?.clone())))
                    .collect()
            })
            .collect(),
        None => rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<Row> {
        [
            (1, "GET /a 200", "200", "12"),
            (2, "GET /b 500", "500", "40"),
            (3, "POST /a 200", "200", "8"),
            (4, "GET /a 404", "404", "3"),
        ]
        .iter()
        .map(|(t, m, s, l)| {
            Row::from_iter([
                ("@timestamp".to_string(), t.to_string()),
                ("@message".to_string(), m.to_string()),
                ("status".to_string(), s.to_string()),
                ("latency".to_string(), l.to_string()),
            ])
        })
        .collect()
    }

    fn col(rows: &[Row], f: &str) -> Vec<String> {
        rows.iter().filter_map(|r| r.get(f).cloned()).collect()
    }

    #[test]
    fn fields_filter_sort_limit() {
        let out = run(
            "fields @timestamp, @message | filter @message like /GET/ | sort latency desc | limit 2",
            rows(),
        )
        .unwrap();
        assert_eq!(col(&out, "@timestamp"), ["2", "1"]);
        assert!(out[0].get("latency").is_none());
        let out = run("filter status = 200 and latency > 10", rows()).unwrap();
        assert_eq!(col(&out, "@timestamp"), ["1"]);
        let out = run("filter status != 200 or @message like 'POST'", rows()).unwrap();
        assert_eq!(col(&out, "@timestamp"), ["2", "3", "4"]);
        let out = run("filter @message not like /\\/a/", rows()).unwrap();
        assert_eq!(col(&out, "@timestamp"), ["2"]);
    }

    #[test]
    fn stats_aggregate_by_group() {
        let out = run(
            "stats count(*) as n, avg(latency), max(latency) by status | sort n desc",
            rows(),
        )
        .unwrap();
        assert_eq!(col(&out, "status"), ["200", "500", "404"]);
        assert_eq!(col(&out, "n"), ["2", "1", "1"]);
        assert_eq!(col(&out, "avg(latency)"), ["10", "40", "3"]);
        assert_eq!(col(&out, "max(latency)"), ["12", "40", "3"]);
        let total = run("stats sum(latency) as total", rows()).unwrap();
        assert_eq!(col(&total, "total"), ["63"]);
    }

    #[test]
    fn json_messages_become_fields() {
        let mut row = Row::new();
        discover(
            "",
            &serde_json::json!({"user": {"id": 7}, "tags": ["x"], "ok": true}),
            &mut row,
        );
        assert_eq!(row.get("user.id").map(String::as_str), Some("7"));
        assert_eq!(row.get("tags.0").map(String::as_str), Some("x"));
        assert_eq!(row.get("ok").map(String::as_str), Some("true"));
    }

    #[test]
    fn rejects_unknown_commands() {
        assert!(run("frobnicate everything", rows()).is_err());
    }
}
