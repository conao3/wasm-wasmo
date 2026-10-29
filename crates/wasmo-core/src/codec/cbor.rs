//! Smithy RPC v2 CBOR protocol: the CBOR data model (RFC 8949) and its
//! mapping to botocore parameters.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value};

use super::AwsError;
use super::time;
use crate::model::{Registry, Shape};

#[derive(Debug, Clone, PartialEq)]
pub enum Cbor {
    Unsigned(u64),
    Negative(u64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Cbor>),
    Map(Vec<(Cbor, Cbor)>),
    Tag(u64, Box<Cbor>),
    Bool(bool),
    Null,
    Undefined,
    Float(f64),
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

const BREAK: u8 = 0xff;

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, String> {
        let b = *self
            .buf
            .get(self.pos)
            .ok_or("unexpected end of CBOR data")?;
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.buf.len())
            .ok_or("unexpected end of CBOR data")?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn uint(&mut self, info: u8) -> Result<u64, String> {
        Ok(match info {
            0..=23 => info as u64,
            24 => self.byte()? as u64,
            25 => u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as u64,
            26 => u32::from_be_bytes(self.take(4)?.try_into().unwrap()) as u64,
            27 => u64::from_be_bytes(self.take(8)?.try_into().unwrap()),
            _ => return Err(format!("invalid CBOR additional information {info}")),
        })
    }

    fn peek_break(&mut self) -> bool {
        if self.buf.get(self.pos) == Some(&BREAK) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn chunks(&mut self, major: u8, info: u8) -> Result<Vec<u8>, String> {
        if info != 31 {
            let n = self.uint(info)? as usize;
            return Ok(self.take(n)?.to_vec());
        }
        let mut out = Vec::new();
        while !self.peek_break() {
            let b = self.byte()?;
            if b >> 5 != major || b & 0x1f == 31 {
                return Err("invalid chunk in indefinite-length string".to_string());
            }
            let n = self.uint(b & 0x1f)? as usize;
            out.extend_from_slice(self.take(n)?);
        }
        Ok(out)
    }

    fn value(&mut self) -> Result<Cbor, String> {
        let b = self.byte()?;
        let (major, info) = (b >> 5, b & 0x1f);
        Ok(match major {
            0 => Cbor::Unsigned(self.uint(info)?),
            1 => Cbor::Negative(self.uint(info)?),
            2 => Cbor::Bytes(self.chunks(2, info)?),
            3 => Cbor::Text(String::from_utf8(self.chunks(3, info)?).map_err(|e| e.to_string())?),
            4 => {
                let mut items = Vec::new();
                if info == 31 {
                    while !self.peek_break() {
                        items.push(self.value()?);
                    }
                } else {
                    for _ in 0..self.uint(info)? {
                        items.push(self.value()?);
                    }
                }
                Cbor::Array(items)
            }
            5 => {
                let mut entries = Vec::new();
                if info == 31 {
                    while !self.peek_break() {
                        entries.push((self.value()?, self.value()?));
                    }
                } else {
                    for _ in 0..self.uint(info)? {
                        entries.push((self.value()?, self.value()?));
                    }
                }
                Cbor::Map(entries)
            }
            6 => {
                let tag = self.uint(info)?;
                Cbor::Tag(tag, Box::new(self.value()?))
            }
            _ => match info {
                20 => Cbor::Bool(false),
                21 => Cbor::Bool(true),
                22 => Cbor::Null,
                23 => Cbor::Undefined,
                25 => Cbor::Float(half_to_f64(u16::from_be_bytes(
                    self.take(2)?.try_into().unwrap(),
                ))),
                26 => Cbor::Float(f32::from_be_bytes(self.take(4)?.try_into().unwrap()) as f64),
                27 => Cbor::Float(f64::from_be_bytes(self.take(8)?.try_into().unwrap())),
                0..=19 => Cbor::Unsigned(info as u64),
                24 => Cbor::Unsigned(self.byte()? as u64),
                _ => return Err(format!("unexpected CBOR simple value {info}")),
            },
        })
    }
}

fn half_to_f64(h: u16) -> f64 {
    let exp = (h >> 10) & 0x1f;
    let mant = (h & 0x3ff) as f64;
    let v = match exp {
        0 => mant * 2f64.powi(-24),
        31 if mant == 0.0 => f64::INFINITY,
        31 => f64::NAN,
        _ => (1.0 + mant / 1024.0) * 2f64.powi(exp as i32 - 15),
    };
    if h & 0x8000 != 0 { -v } else { v }
}

pub fn decode(buf: &[u8]) -> Result<Cbor, String> {
    let mut r = Reader { buf, pos: 0 };
    let v = r.value()?;
    if r.pos != buf.len() {
        return Err("trailing bytes after CBOR data item".to_string());
    }
    Ok(v)
}

fn head(out: &mut Vec<u8>, major: u8, n: u64) {
    let m = major << 5;
    match n {
        0..=23 => out.push(m | n as u8),
        24..=0xff => out.extend_from_slice(&[m | 24, n as u8]),
        0x100..=0xffff => {
            out.push(m | 25);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(m | 26);
            out.extend_from_slice(&(n as u32).to_be_bytes());
        }
        _ => {
            out.push(m | 27);
            out.extend_from_slice(&n.to_be_bytes());
        }
    }
}

pub fn encode(v: &Cbor, out: &mut Vec<u8>) {
    match v {
        Cbor::Unsigned(n) => head(out, 0, *n),
        Cbor::Negative(n) => head(out, 1, *n),
        Cbor::Bytes(b) => {
            head(out, 2, b.len() as u64);
            out.extend_from_slice(b);
        }
        Cbor::Text(s) => {
            head(out, 3, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
        Cbor::Array(items) => {
            head(out, 4, items.len() as u64);
            items.iter().for_each(|i| encode(i, out));
        }
        Cbor::Map(entries) => {
            head(out, 5, entries.len() as u64);
            for (k, v) in entries {
                encode(k, out);
                encode(v, out);
            }
        }
        Cbor::Tag(t, inner) => {
            head(out, 6, *t);
            encode(inner, out);
        }
        Cbor::Bool(b) => out.push(if *b { 0xf5 } else { 0xf4 }),
        Cbor::Null => out.push(0xf6),
        Cbor::Undefined => out.push(0xf7),
        Cbor::Float(f) => {
            if (*f as f32) as f64 == *f || f.is_nan() {
                out.push(0xfa);
                out.extend_from_slice(&(*f as f32).to_be_bytes());
            } else {
                out.push(0xfb);
                out.extend_from_slice(&f.to_be_bytes());
            }
        }
    }
}

fn number(c: &Cbor) -> Option<f64> {
    match c {
        Cbor::Unsigned(n) => Some(*n as f64),
        Cbor::Negative(n) => Some(-1.0 - *n as f64),
        Cbor::Float(f) => Some(*f),
        Cbor::Tag(_, inner) => number(inner),
        _ => None,
    }
}

fn float_value(f: f64) -> Value {
    if f.is_nan() {
        Value::String("NaN".to_string())
    } else if f.is_infinite() {
        Value::String(if f > 0.0 { "Infinity" } else { "-Infinity" }.to_string())
    } else {
        Value::from(f)
    }
}

fn text_key(c: &Cbor) -> Option<&str> {
    match c {
        Cbor::Text(s) => Some(s),
        _ => None,
    }
}

/// Converts a decoded CBOR item into botocore parameters for `shape`.
pub fn from_wire(reg: &Registry, c: &Cbor, shape: &Shape) -> Value {
    match (shape.kind.as_str(), c) {
        (_, Cbor::Null | Cbor::Undefined) => Value::Null,
        ("structure", Cbor::Map(entries)) if shape.document => Value::Object(untyped_map(entries)),
        ("structure", Cbor::Map(entries)) => {
            let mut out = Map::new();
            for (k, v) in entries {
                let Some(name) = text_key(k) else { continue };
                let Some(m) = shape.members.get(name) else {
                    continue;
                };
                if matches!(v, Cbor::Null | Cbor::Undefined) {
                    continue;
                }
                out.insert(name.to_string(), from_wire(reg, v, reg.member_shape(m)));
            }
            Value::Object(out)
        }
        ("list", Cbor::Array(items)) => {
            let item = reg.member_shape(shape.member.as_deref().expect("list member"));
            Value::Array(items.iter().map(|i| from_wire(reg, i, item)).collect())
        }
        ("map", Cbor::Map(entries)) => {
            let value = reg.member_shape(shape.value.as_deref().expect("map value"));
            Value::Object(
                entries
                    .iter()
                    .filter(|(_, v)| !matches!(v, Cbor::Null | Cbor::Undefined))
                    .filter_map(|(k, v)| Some((text_key(k)?.to_string(), from_wire(reg, v, value))))
                    .collect(),
            )
        }
        ("blob", Cbor::Bytes(b)) => Value::String(STANDARD.encode(b)),
        ("timestamp", _) => number(c).map_or(Value::Null, time::epoch_value),
        ("float" | "double", _) => number(c).map_or(Value::Null, float_value),
        (_, other) => untyped(other),
    }
}

fn untyped_map(entries: &[(Cbor, Cbor)]) -> Map<String, Value> {
    entries
        .iter()
        .filter_map(|(k, v)| Some((text_key(k)?.to_string(), untyped(v))))
        .collect()
}

fn untyped(c: &Cbor) -> Value {
    match c {
        Cbor::Unsigned(n) => Value::from(*n),
        Cbor::Negative(n) => Value::from(-1 - *n as i64),
        Cbor::Bytes(b) => Value::String(STANDARD.encode(b)),
        Cbor::Text(s) => Value::String(s.clone()),
        Cbor::Array(items) => Value::Array(items.iter().map(untyped).collect()),
        Cbor::Map(entries) => Value::Object(untyped_map(entries)),
        Cbor::Tag(_, inner) => untyped(inner),
        Cbor::Bool(b) => Value::Bool(*b),
        Cbor::Null | Cbor::Undefined => Value::Null,
        Cbor::Float(f) => float_value(*f),
    }
}

fn int(v: &Value) -> Cbor {
    let n = v
        .as_i64()
        .or_else(|| v.as_f64().map(|f| f as i64))
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_default();
    if n >= 0 {
        Cbor::Unsigned(n as u64)
    } else {
        Cbor::Negative((-1 - n) as u64)
    }
}

fn float(v: &Value) -> Cbor {
    Cbor::Float(match v {
        Value::String(s) => match s.as_str() {
            "NaN" => f64::NAN,
            "Infinity" => f64::INFINITY,
            "-Infinity" => f64::NEG_INFINITY,
            s => s.parse().unwrap_or_default(),
        },
        v => v.as_f64().unwrap_or_default(),
    })
}

fn untyped_to_wire(v: &Value) -> Cbor {
    match v {
        Value::Null => Cbor::Null,
        Value::Bool(b) => Cbor::Bool(*b),
        Value::Number(n) if n.is_f64() => Cbor::Float(n.as_f64().unwrap_or_default()),
        Value::Number(_) => int(v),
        Value::String(s) => Cbor::Text(s.clone()),
        Value::Array(a) => Cbor::Array(a.iter().map(untyped_to_wire).collect()),
        Value::Object(o) => Cbor::Map(
            o.iter()
                .map(|(k, v)| (Cbor::Text(k.clone()), untyped_to_wire(v)))
                .collect(),
        ),
    }
}

/// Converts botocore parameters for `shape` into a CBOR item.
pub fn to_wire(reg: &Registry, v: &Value, shape: &Shape) -> Cbor {
    if v.is_null() {
        return Cbor::Null;
    }
    match shape.kind.as_str() {
        "structure" if shape.document => untyped_to_wire(v),
        "structure" => Cbor::Map(
            v.as_object()
                .into_iter()
                .flatten()
                .filter(|(_, x)| !x.is_null())
                .filter_map(|(k, x)| {
                    let m = shape.members.get(k)?;
                    Some((Cbor::Text(k.clone()), to_wire(reg, x, reg.member_shape(m))))
                })
                .collect(),
        ),
        "list" => {
            let item = reg.member_shape(shape.member.as_deref().expect("list member"));
            Cbor::Array(
                v.as_array()
                    .into_iter()
                    .flatten()
                    .map(|x| to_wire(reg, x, item))
                    .collect(),
            )
        }
        "map" => {
            let value = reg.member_shape(shape.value.as_deref().expect("map value"));
            Cbor::Map(
                v.as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(_, x)| !x.is_null())
                    .map(|(k, x)| (Cbor::Text(k.clone()), to_wire(reg, x, value)))
                    .collect(),
            )
        }
        "blob" => Cbor::Bytes(
            v.as_str()
                .and_then(|s| STANDARD.decode(s).ok())
                .unwrap_or_default(),
        ),
        "timestamp" => Cbor::Tag(
            1,
            Box::new(Cbor::Float(time::value_epoch(v).unwrap_or_default())),
        ),
        "boolean" => Cbor::Bool(v.as_bool().unwrap_or(v.as_str() == Some("true"))),
        "integer" | "long" | "short" | "byte" | "bigInteger" => int(v),
        "float" | "double" | "bigDecimal" => float(v),
        _ => match v {
            Value::String(s) => Cbor::Text(s.clone()),
            other => untyped_to_wire(other),
        },
    }
}

pub fn parse_input(reg: &Registry, body: &[u8], shape: Option<&Shape>) -> Result<Value, AwsError> {
    let Some(shape) = shape else {
        return Ok(Value::Object(Map::new()));
    };
    if body.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let c = decode(body).map_err(|e| AwsError::new(400, "SerializationException", e))?;
    Ok(match from_wire(reg, &c, shape) {
        Value::Null => Value::Object(Map::new()),
        v => v,
    })
}

pub fn serialize(reg: &Registry, value: &Value, shape: Option<&Shape>) -> Vec<u8> {
    let c = shape.map_or(Cbor::Map(Vec::new()), |s| to_wire(reg, value, s));
    let mut out = Vec::new();
    encode(&c, &mut out);
    out
}

/// Body of an RPC v2 CBOR error: `__type`, `message`, and the modeled members.
pub fn error_body(reg: &Registry, error: &AwsError, shape: Option<&Shape>) -> Vec<u8> {
    let mut entries = vec![(
        Cbor::Text("__type".to_string()),
        Cbor::Text(error.code.clone()),
    )];
    if !error.message.is_empty() {
        let key = shape
            .and_then(|s| s.members.keys().find(|n| n.eq_ignore_ascii_case("message")))
            .map_or("message", String::as_str);
        entries.push((
            Cbor::Text(key.to_string()),
            Cbor::Text(error.message.clone()),
        ));
    }
    if let Some(Cbor::Map(members)) =
        shape.map(|s| to_wire(reg, &Value::Object((*error.data).clone()), s))
    {
        entries.extend(
            members
                .into_iter()
                .filter(|(k, _)| !text_key(k).is_some_and(|k| k.eq_ignore_ascii_case("message"))),
        );
    }
    let mut out = Vec::new();
    encode(&Cbor::Map(entries), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(c: Cbor) {
        let mut out = Vec::new();
        encode(&c, &mut out);
        assert_eq!(decode(&out).unwrap(), c);
    }

    #[test]
    fn encodes_and_decodes_items() {
        roundtrip(Cbor::Unsigned(0));
        roundtrip(Cbor::Unsigned(500));
        roundtrip(Cbor::Unsigned(u64::MAX));
        roundtrip(Cbor::Negative(9));
        roundtrip(Cbor::Text("héllo".to_string()));
        roundtrip(Cbor::Bytes(vec![0, 1, 2]));
        roundtrip(Cbor::Float(1.5));
        roundtrip(Cbor::Float(0.1));
        roundtrip(Cbor::Tag(1, Box::new(Cbor::Float(1.7e9))));
        roundtrip(Cbor::Map(vec![(
            Cbor::Text("a".to_string()),
            Cbor::Array(vec![Cbor::Bool(true), Cbor::Null]),
        )]));
    }

    #[test]
    fn decodes_indefinite_lengths_and_half_floats() {
        let bytes = [
            0xbf, 0x61, 0x61, 0x9f, 0x01, 0xff, 0x61, 0x62, 0x7f, 0x61, 0x78, 0x61, 0x79, 0xff,
            0x61, 0x63, 0xf9, 0x3e, 0x00, 0xff,
        ];
        assert_eq!(
            decode(&bytes).unwrap(),
            Cbor::Map(vec![
                (
                    Cbor::Text("a".to_string()),
                    Cbor::Array(vec![Cbor::Unsigned(1)])
                ),
                (Cbor::Text("b".to_string()), Cbor::Text("xy".to_string())),
                (Cbor::Text("c".to_string()), Cbor::Float(1.5)),
            ])
        );
    }

    #[test]
    fn rejects_truncated_input() {
        assert!(decode(&[0x62, 0x61]).is_err());
        assert!(decode(&[0x01, 0x02]).is_err());
    }
}
