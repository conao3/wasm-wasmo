//! The JSONPath subset Amazon States Language accepts: dot and bracket
//! members, indexes (negative from the end), `*`, slices, `..` descent, and
//! `[?(@.path <op> literal)]` filters.

use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
enum Seg {
    Key(String),
    Index(i64),
    Wild,
    Slice(Option<i64>, Option<i64>, i64),
    Descend(String),
    Filter(Vec<Seg>, Option<(String, Value)>),
}

fn parse(path: &str) -> Result<Vec<Seg>, String> {
    let bad = || format!("Invalid JSONPath: '{path}'");
    let rest = path.strip_prefix('$').ok_or_else(bad)?;
    let mut segs = Vec::new();
    let b = rest.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'.' if b.get(i + 1) == Some(&b'.') => {
                let (name, next) = ident(rest, i + 2);
                if name.is_empty() {
                    return Err(bad());
                }
                segs.push(Seg::Descend(name));
                i = next;
            }
            b'.' => {
                let (name, next) = ident(rest, i + 1);
                match name.as_str() {
                    "" => return Err(bad()),
                    "*" => segs.push(Seg::Wild),
                    _ => segs.push(Seg::Key(name)),
                }
                i = next;
            }
            b'[' => {
                let close = matching(rest, i).ok_or_else(bad)?;
                segs.push(bracket(&rest[i + 1..close]).ok_or_else(bad)?);
                i = close + 1;
            }
            _ => return Err(bad()),
        }
    }
    Ok(segs)
}

fn ident(s: &str, start: usize) -> (String, usize) {
    let end = s[start..].find(['.', '[']).map_or(s.len(), |n| start + n);
    (s[start..end].to_string(), end)
}

fn matching(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut quote = None;
    for (i, c) in s[open..].char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '[') => depth += 1,
            (None, ']') => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

fn bracket(inner: &str) -> Option<Seg> {
    let inner = inner.trim();
    if inner == "*" {
        return Some(Seg::Wild);
    }
    if let Some(q) = inner
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| inner.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
    {
        return Some(Seg::Key(q.to_string()));
    }
    if let Some(f) = inner.strip_prefix("?(").and_then(|s| s.strip_suffix(')')) {
        return filter(f.trim());
    }
    if inner.contains(':') {
        let mut parts = inner.split(':');
        let num = |s: Option<&str>| -> Option<Option<i64>> {
            match s.map(str::trim) {
                None | Some("") => Some(None),
                Some(n) => n.parse().ok().map(Some),
            }
        };
        let start = num(parts.next())?;
        let end = num(parts.next())?;
        let step = num(parts.next())?.unwrap_or(1);
        return (step != 0).then_some(Seg::Slice(start, end, step));
    }
    inner.parse().ok().map(Seg::Index)
}

fn filter(expr: &str) -> Option<Seg> {
    let rest = expr.strip_prefix('@')?;
    for op in ["==", "!=", "<=", ">=", "<", ">"] {
        if let Some((lhs, rhs)) = rest.split_once(op) {
            let segs = parse(&format!("${}", lhs.trim())).ok()?;
            let rhs = rhs.trim();
            let lit = if let Some(s) = rhs.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
                Value::String(s.to_string())
            } else {
                serde_json::from_str(rhs).ok()?
            };
            return Some(Seg::Filter(segs, Some((op.to_string(), lit))));
        }
    }
    Some(Seg::Filter(parse(&format!("${}", rest.trim())).ok()?, None))
}

fn definite(segs: &[Seg]) -> bool {
    segs.iter()
        .all(|s| matches!(s, Seg::Key(_) | Seg::Index(_)))
}

fn index(len: usize, i: i64) -> Option<usize> {
    let i = if i < 0 { len as i64 + i } else { i };
    (0..len as i64).contains(&i).then_some(i as usize)
}

fn walk<'a>(v: &'a Value, segs: &[Seg], out: &mut Vec<&'a Value>) {
    let Some((seg, rest)) = segs.split_first() else {
        out.push(v);
        return;
    };
    match seg {
        Seg::Key(k) => {
            if let Some(x) = v.get(k) {
                walk(x, rest, out);
            }
        }
        Seg::Index(i) => {
            if let Some(a) = v.as_array()
                && let Some(i) = index(a.len(), *i)
            {
                walk(&a[i], rest, out);
            }
        }
        Seg::Wild => match v {
            Value::Array(a) => a.iter().for_each(|x| walk(x, rest, out)),
            Value::Object(o) => o.values().for_each(|x| walk(x, rest, out)),
            _ => {}
        },
        Seg::Slice(start, end, step) => {
            if let Some(a) = v.as_array() {
                let len = a.len() as i64;
                let clamp = |x: i64| if x < 0 { (len + x).max(0) } else { x.min(len) };
                if *step > 0 {
                    let mut i = start.map_or(0, clamp);
                    let end = end.map_or(len, clamp);
                    while i < end {
                        walk(&a[i as usize], rest, out);
                        i += step;
                    }
                } else {
                    let mut i = start.map_or(len - 1, |s| clamp(s).min(len - 1));
                    let end = end.map_or(-1, clamp);
                    while i > end {
                        walk(&a[i as usize], rest, out);
                        i += step;
                    }
                }
            }
        }
        Seg::Descend(k) => {
            if let Some(x) = v.get(k) {
                walk(x, rest, out);
            }
            match v {
                Value::Array(a) => a.iter().for_each(|x| walk(x, segs, out)),
                Value::Object(o) => o.values().for_each(|x| walk(x, segs, out)),
                _ => {}
            }
        }
        Seg::Filter(path, cond) => {
            let items: Vec<&Value> = match v {
                Value::Array(a) => a.iter().collect(),
                Value::Object(o) => o.values().collect(),
                _ => Vec::new(),
            };
            for item in items {
                let mut found = Vec::new();
                walk(item, path, &mut found);
                let keep = match (found.first(), cond) {
                    (None, _) => false,
                    (Some(_), None) => true,
                    (Some(x), Some((op, lit))) => compare(x, op, lit),
                };
                if keep {
                    walk(item, rest, out);
                }
            }
        }
    }
}

fn compare(x: &Value, op: &str, lit: &Value) -> bool {
    let ord = match (x, lit) {
        (Value::Number(a), Value::Number(b)) => a.as_f64().partial_cmp(&b.as_f64()),
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        _ => None,
    };
    match op {
        "==" => x == lit,
        "!=" => x != lit,
        "<" => ord.is_some_and(|o| o.is_lt()),
        "<=" => ord.is_some_and(|o| o.is_le()),
        ">" => ord.is_some_and(|o| o.is_gt()),
        ">=" => ord.is_some_and(|o| o.is_ge()),
        _ => false,
    }
}

/// Evaluates `path` against `data`. A definite path yields its value or None
/// when it does not resolve; any other path yields the array of matches.
pub fn get(data: &Value, path: &str) -> Result<Option<Value>, String> {
    let segs = parse(path)?;
    let mut out = Vec::new();
    walk(data, &segs, &mut out);
    Ok(if definite(&segs) {
        out.first().map(|v| (*v).clone())
    } else {
        Some(Value::Array(out.into_iter().cloned().collect()))
    })
}

/// Whether `p` is a path over the input (`$...`) or the context object
/// (`$$...`).
pub fn valid(p: &str) -> bool {
    let p = p
        .strip_prefix('$')
        .filter(|r| r.starts_with('$'))
        .unwrap_or(p);
    parse(p).is_ok()
}

/// Whether `path` is a reference path (members and indexes only).
pub fn is_reference(path: &str) -> bool {
    parse(path).is_ok_and(|s| definite(&s))
}

/// Writes `value` at the reference path `path` inside `data`, creating the
/// objects on the way.
pub fn set(data: Value, path: &str, value: Value) -> Result<Value, String> {
    let segs = parse(path)?;
    if !definite(&segs) {
        return Err(format!("Invalid reference path: '{path}'"));
    }
    let mut root = data;
    put(&mut root, &segs, value, path)?;
    Ok(root)
}

fn put(slot: &mut Value, segs: &[Seg], value: Value, path: &str) -> Result<(), String> {
    let Some((seg, rest)) = segs.split_first() else {
        *slot = value;
        return Ok(());
    };
    match seg {
        Seg::Key(k) => {
            if slot.is_null() {
                *slot = Value::Object(Map::new());
            }
            let Some(obj) = slot.as_object_mut() else {
                return Err(format!(
                    "Unable to apply ResultPath '{path}': the input is not an object"
                ));
            };
            put(
                obj.entry(k.clone()).or_insert(Value::Null),
                rest,
                value,
                path,
            )
        }
        Seg::Index(i) => {
            let len = slot.as_array().map_or(0, Vec::len);
            let Some(i) = slot.as_array().and(index(len, *i)) else {
                return Err(format!(
                    "Unable to apply ResultPath '{path}': index out of range"
                ));
            };
            put(
                &mut slot.as_array_mut().expect("checked")[i],
                rest,
                value,
                path,
            )
        }
        _ => Err(format!("Invalid reference path: '{path}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_paths() {
        let v = json!({"a": {"b": [1, 2, 3]}, "c d": 4, "items": [{"p": 5, "n": "x"}, {"p": 15, "n": "y"}]});
        assert_eq!(get(&v, "$.a.b[1]").unwrap(), Some(json!(2)));
        assert_eq!(get(&v, "$.a.b[-1]").unwrap(), Some(json!(3)));
        assert_eq!(get(&v, "$['c d']").unwrap(), Some(json!(4)));
        assert_eq!(get(&v, "$.missing").unwrap(), None);
        assert_eq!(get(&v, "$.a.b[0:2]").unwrap(), Some(json!([1, 2])));
        assert_eq!(get(&v, "$.items[*].n").unwrap(), Some(json!(["x", "y"])));
        assert_eq!(get(&v, "$..p").unwrap(), Some(json!([5, 15])));
        assert_eq!(
            get(&v, "$.items[?(@.p > 10)].n").unwrap(),
            Some(json!(["y"]))
        );
        assert!(get(&v, "a.b").is_err());
    }

    #[test]
    fn writes_paths() {
        let v = set(json!({"a": 1}), "$.r.s", json!(2)).unwrap();
        assert_eq!(v, json!({"a": 1, "r": {"s": 2}}));
        assert_eq!(set(json!({"a": 1}), "$", json!(3)).unwrap(), json!(3));
        assert!(set(json!([1]), "$.x", json!(1)).is_err());
    }
}
