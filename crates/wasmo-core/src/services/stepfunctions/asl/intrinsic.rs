//! Intrinsic functions (`States.Format(...)` and the rest) in payload
//! templates.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use md5::{Digest, Md5};
use serde_json::{Map, Value, json};
use sha1::Sha1;
use sha2::{Sha256, Sha384, Sha512};

use super::{Failure, Scope, to_text};

#[derive(Debug, Clone)]
enum Arg {
    /// A string literal keeps `\{` and `\}`, which only `States.Format` reads
    /// as escapes.
    Lit(Value),
    Path(String),
    Call(String, Vec<Arg>),
}

struct Parser<'a> {
    s: &'a str,
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.s[self.i..].chars().next()
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.i += 1;
        }
    }

    fn invalid(&self) -> String {
        format!("Invalid intrinsic function call: '{}'", self.s)
    }

    fn call(&mut self) -> Result<Arg, String> {
        let start = self.i;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '.')
        {
            self.i += 1;
        }
        let name = self.s[start..self.i].to_string();
        if !name.starts_with("States.") || self.peek() != Some('(') {
            return Err(self.invalid());
        }
        self.i += 1;
        let mut args = Vec::new();
        self.skip_ws();
        if self.peek() == Some(')') {
            self.i += 1;
            return Ok(Arg::Call(name, args));
        }
        loop {
            self.skip_ws();
            args.push(self.arg()?);
            self.skip_ws();
            match self.peek() {
                Some(',') => self.i += 1,
                Some(')') => {
                    self.i += 1;
                    return Ok(Arg::Call(name, args));
                }
                _ => return Err(self.invalid()),
            }
        }
    }

    fn string(&mut self) -> Result<Arg, String> {
        self.i += 1;
        let mut out = String::new();
        let mut chars = self.s[self.i..].char_indices();
        while let Some((n, c)) = chars.next() {
            match c {
                '\\' => {
                    if let Some((_, e)) = chars.next() {
                        if e == '{' || e == '}' {
                            out.push('\\');
                        }
                        out.push(e);
                    }
                }
                '\'' => {
                    self.i += n + 1;
                    return Ok(Arg::Lit(Value::String(out)));
                }
                c => out.push(c),
            }
        }
        Err(format!("Unterminated string in '{}'", self.s))
    }

    fn path(&mut self) -> Arg {
        let start = self.i;
        let mut depth = 0;
        let mut quote = false;
        while let Some(c) = self.peek() {
            match c {
                '\'' => quote = !quote,
                '[' if !quote => depth += 1,
                ']' if !quote => depth -= 1,
                ',' | ')' if !quote && depth == 0 => break,
                _ => {}
            }
            self.i += c.len_utf8();
        }
        Arg::Path(self.s[start..self.i].trim().to_string())
    }

    fn arg(&mut self) -> Result<Arg, String> {
        match self.peek() {
            Some('\'') => self.string(),
            Some('$') => Ok(self.path()),
            Some('S') => self.call(),
            _ => {
                let start = self.i;
                while self.peek().is_some_and(|c| c != ',' && c != ')') {
                    self.i += 1;
                }
                let raw = self.s[start..self.i].trim();
                serde_json::from_str(raw)
                    .map(Arg::Lit)
                    .map_err(|_| format!("Invalid intrinsic function argument: '{raw}'"))
            }
        }
    }
}

pub fn is_call(s: &str) -> bool {
    s.trim_start().starts_with("States.")
}

pub fn parses(expr: &str) -> bool {
    let mut p = Parser { s: expr, i: 0 };
    p.skip_ws();
    p.call().is_ok() && p.s[p.i..].trim().is_empty()
}

/// Evaluates an intrinsic function expression.
pub fn eval(expr: &str, scope: &mut Scope<'_, '_>) -> Result<Value, Failure> {
    let mut p = Parser { s: expr, i: 0 };
    p.skip_ws();
    let call = p.call().map_err(failure)?;
    if !p.s[p.i..].trim().is_empty() {
        return Err(failure(p.invalid()));
    }
    run(&call, scope)
}

fn failure(cause: impl Into<String>) -> Failure {
    Failure::new("States.IntrinsicFailure", cause)
}

fn value(arg: &Arg, scope: &mut Scope<'_, '_>) -> Result<Value, Failure> {
    match arg {
        Arg::Lit(Value::String(s)) => Ok(Value::String(s.replace("\\{", "{").replace("\\}", "}"))),
        Arg::Lit(v) => Ok(v.clone()),
        Arg::Path(p) => scope.path(p).map_err(|f| failure(f.cause)),
        Arg::Call(..) => run(arg, scope),
    }
}

fn int(v: &Value, name: &str) -> Result<i64, Failure> {
    v.as_i64()
        .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
        .ok_or_else(|| failure(format!("{name} requires integer arguments")))
}

fn text<'v>(v: &'v Value, name: &str) -> Result<&'v str, Failure> {
    v.as_str()
        .ok_or_else(|| failure(format!("{name} requires a string argument")))
}

fn array<'v>(v: &'v Value, name: &str) -> Result<&'v Vec<Value>, Failure> {
    v.as_array()
        .ok_or_else(|| failure(format!("{name} requires an array argument")))
}

fn arity(name: &str, args: &[Value], min: usize, max: usize) -> Result<(), Failure> {
    if (min..=max).contains(&args.len()) {
        Ok(())
    } else {
        Err(failure(format!(
            "The intrinsic function {name} received {} arguments",
            args.len()
        )))
    }
}

fn run(call: &Arg, scope: &mut Scope<'_, '_>) -> Result<Value, Failure> {
    let Arg::Call(name, raw) = call else {
        return value(call, scope);
    };
    let name = name.as_str();
    let mut args = Vec::with_capacity(raw.len());
    for (i, a) in raw.iter().enumerate() {
        match a {
            Arg::Lit(v) if i == 0 && name == "States.Format" => args.push(v.clone()),
            a => args.push(value(a, scope)?),
        }
    }
    match name {
        "States.Format" => format(name, &args),
        "States.StringToJson" => {
            arity(name, &args, 1, 1)?;
            serde_json::from_str(text(&args[0], name)?).map_err(|e| failure(format!("{name}: {e}")))
        }
        "States.JsonToString" => {
            arity(name, &args, 1, 1)?;
            Ok(Value::String(args[0].to_string()))
        }
        "States.Array" => Ok(Value::Array(args)),
        "States.UUID" => {
            arity(name, &args, 0, 0)?;
            Ok(Value::String(scope.uuid()))
        }
        "States.MathAdd" => {
            arity(name, &args, 2, 2)?;
            Ok(json!(int(&args[0], name)? + int(&args[1], name)?))
        }
        "States.MathRandom" => {
            arity(name, &args, 2, 3)?;
            let (lo, hi) = (int(&args[0], name)?, int(&args[1], name)?);
            if hi <= lo {
                return Err(failure(format!("{name} requires start < end")));
            }
            let draw = scope.random_u64();
            Ok(json!(lo + (draw % (hi - lo) as u64) as i64))
        }
        "States.Base64Encode" => {
            arity(name, &args, 1, 1)?;
            Ok(json!(STANDARD.encode(text(&args[0], name)?)))
        }
        "States.Base64Decode" => {
            arity(name, &args, 1, 1)?;
            let bytes = STANDARD
                .decode(text(&args[0], name)?)
                .map_err(|e| failure(format!("{name}: {e}")))?;
            Ok(json!(String::from_utf8_lossy(&bytes)))
        }
        "States.Hash" => hash(name, &args),
        "States.JsonMerge" => {
            arity(name, &args, 2, 3)?;
            let (Some(a), Some(b)) = (args[0].as_object(), args[1].as_object()) else {
                return Err(failure(format!("{name} requires two objects")));
            };
            let mut out: Map<String, Value> = a.clone();
            out.extend(b.clone());
            Ok(Value::Object(out))
        }
        "States.StringSplit" => {
            arity(name, &args, 2, 2)?;
            let delims: Vec<char> = text(&args[1], name)?.chars().collect();
            Ok(json!(
                text(&args[0], name)?
                    .split(delims.as_slice())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            ))
        }
        _ => arrays(name, &args),
    }
}

fn hash(name: &str, args: &[Value]) -> Result<Value, Failure> {
    arity(name, args, 2, 2)?;
    let data = to_text(&args[0]);
    let data = data.as_bytes();
    let digest: Vec<u8> = match text(&args[1], name)? {
        "MD5" => Md5::digest(data).to_vec(),
        "SHA-1" => Sha1::digest(data).to_vec(),
        "SHA-256" => Sha256::digest(data).to_vec(),
        "SHA-384" => Sha384::digest(data).to_vec(),
        "SHA-512" => Sha512::digest(data).to_vec(),
        other => {
            return Err(failure(format!("{name}: unsupported algorithm '{other}'")));
        }
    };
    Ok(json!(
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ))
}

fn format(name: &str, args: &[Value]) -> Result<Value, Failure> {
    let template = text(
        args.first()
            .ok_or_else(|| failure(format!("{name} requires a template")))?,
        name,
    )?;
    let mut rest = args[1..].iter();
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && matches!(chars.peek(), Some('{' | '}')) {
            out.extend(chars.next());
        } else if c == '{' && chars.peek() == Some(&'}') {
            chars.next();
            let v = rest.next().ok_or_else(|| {
                failure(format!(
                    "{name}: the template has more placeholders than arguments"
                ))
            })?;
            out.push_str(&to_text(v));
        } else {
            out.push(c);
        }
    }
    if rest.next().is_some() {
        return Err(failure(format!(
            "{name}: the template has fewer placeholders than arguments"
        )));
    }
    Ok(Value::String(out))
}

fn arrays(name: &str, args: &[Value]) -> Result<Value, Failure> {
    match name {
        "States.ArrayPartition" => {
            arity(name, args, 2, 2)?;
            let size = int(&args[1], name)?;
            if size <= 0 {
                return Err(failure(format!("{name} requires a positive chunk size")));
            }
            Ok(json!(
                array(&args[0], name)?
                    .chunks(size as usize)
                    .map(<[Value]>::to_vec)
                    .collect::<Vec<_>>()
            ))
        }
        "States.ArrayContains" => {
            arity(name, args, 2, 2)?;
            Ok(json!(array(&args[0], name)?.contains(&args[1])))
        }
        "States.ArrayRange" => {
            arity(name, args, 3, 3)?;
            let (start, end, step) = (
                int(&args[0], name)?,
                int(&args[1], name)?,
                int(&args[2], name)?,
            );
            if step == 0 {
                return Err(failure(format!("{name} requires a non-zero step")));
            }
            let mut out = Vec::new();
            let mut i = start;
            while (step > 0 && i <= end) || (step < 0 && i >= end) {
                out.push(json!(i));
                if out.len() > 1000 {
                    return Err(failure(format!("{name} produces more than 1000 items")));
                }
                i += step;
            }
            Ok(Value::Array(out))
        }
        "States.ArrayGetItem" => {
            arity(name, args, 2, 2)?;
            let a = array(&args[0], name)?;
            let i = int(&args[1], name)?;
            usize::try_from(i)
                .ok()
                .and_then(|i| a.get(i))
                .cloned()
                .ok_or_else(|| failure(format!("{name}: index {i} is out of range")))
        }
        "States.ArrayLength" => {
            arity(name, args, 1, 1)?;
            Ok(json!(array(&args[0], name)?.len()))
        }
        "States.ArrayUnique" => {
            arity(name, args, 1, 1)?;
            let mut out: Vec<Value> = Vec::new();
            for v in array(&args[0], name)? {
                if !out.contains(v) {
                    out.push(v.clone());
                }
            }
            Ok(Value::Array(out))
        }
        _ => Err(failure(format!("Unknown intrinsic function: {name}"))),
    }
}
