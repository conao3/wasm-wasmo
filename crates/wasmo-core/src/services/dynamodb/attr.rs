use std::cmp::Ordering;
use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value};

use crate::codec::AwsError;

use super::number::{Num, normalize};
use super::validation;

pub type Item = BTreeMap<String, Attr>;

#[derive(Debug, Clone, PartialEq)]
pub enum Attr {
    S(String),
    N(String),
    B(Vec<u8>),
    Ss(Vec<String>),
    Ns(Vec<String>),
    Bs(Vec<Vec<u8>>),
    M(Item),
    L(Vec<Attr>),
    Null,
    Bool(bool),
}

fn invalid(message: impl std::fmt::Display) -> AwsError {
    validation(format!(
        "One or more parameter values were invalid: {message}"
    ))
}

fn decode_b64(s: &str) -> Result<Vec<u8>, AwsError> {
    STANDARD
        .decode(s)
        .map_err(|_| validation("Invalid Base64 encoding for binary attribute value"))
}

fn str_list(v: &Value) -> Vec<&str> {
    v.as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

impl Attr {
    pub fn from_value(v: &Value) -> Result<Self, AwsError> {
        let obj = v.as_object().ok_or_else(|| {
            validation("Supplied AttributeValue is empty, must contain exactly one of the supported datatypes")
        })?;
        let mut set = obj.iter().filter(|(_, x)| !x.is_null());
        let Some((tag, x)) = set.next() else {
            return Err(validation(
                "Supplied AttributeValue is empty, must contain exactly one of the supported datatypes",
            ));
        };
        if set.next().is_some() {
            return Err(validation(
                "Supplied AttributeValue has more than one datatypes set, must contain exactly one of the supported datatypes",
            ));
        }
        Ok(match tag.as_str() {
            "S" => Attr::S(x.as_str().unwrap_or_default().to_string()),
            "N" => Attr::N(normalize(x.as_str().unwrap_or_default())?),
            "B" => Attr::B(decode_b64(x.as_str().unwrap_or_default())?),
            "BOOL" => Attr::Bool(x.as_bool().unwrap_or_default()),
            "NULL" => {
                if x.as_bool() != Some(true) {
                    return Err(invalid(
                        "Null attribute value types must have the value of true",
                    ));
                }
                Attr::Null
            }
            "SS" => {
                let items: Vec<String> = str_list(x).into_iter().map(str::to_string).collect();
                check_set("string", &items)?;
                Attr::Ss(items)
            }
            "NS" => {
                let items = str_list(x)
                    .into_iter()
                    .map(normalize)
                    .collect::<Result<Vec<_>, _>>()?;
                check_set("number", &items)?;
                Attr::Ns(items)
            }
            "BS" => {
                let items = str_list(x)
                    .into_iter()
                    .map(decode_b64)
                    .collect::<Result<Vec<_>, _>>()?;
                check_set("binary", &items)?;
                Attr::Bs(items)
            }
            "M" => Attr::M(item_from_value(x)?),
            "L" => Attr::L(
                x.as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .map(Attr::from_value)
                    .collect::<Result<_, _>>()?,
            ),
            other => {
                return Err(validation(format!(
                    "Supplied AttributeValue has an unsupported datatype: {other}"
                )));
            }
        })
    }

    pub fn to_value(&self) -> Value {
        let (tag, v) = match self {
            Attr::S(s) => ("S", Value::String(s.clone())),
            Attr::N(n) => ("N", Value::String(n.clone())),
            Attr::B(b) => ("B", Value::String(STANDARD.encode(b))),
            Attr::Ss(v) => ("SS", v.iter().cloned().map(Value::String).collect()),
            Attr::Ns(v) => ("NS", v.iter().cloned().map(Value::String).collect()),
            Attr::Bs(v) => (
                "BS",
                v.iter()
                    .map(|b| Value::String(STANDARD.encode(b)))
                    .collect(),
            ),
            Attr::M(m) => ("M", item_to_value(m)),
            Attr::L(l) => ("L", l.iter().map(Attr::to_value).collect()),
            Attr::Null => ("NULL", Value::Bool(true)),
            Attr::Bool(b) => ("BOOL", Value::Bool(*b)),
        };
        let mut m = Map::new();
        m.insert(tag.to_string(), v);
        Value::Object(m)
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Attr::S(_) => "S",
            Attr::N(_) => "N",
            Attr::B(_) => "B",
            Attr::Ss(_) => "SS",
            Attr::Ns(_) => "NS",
            Attr::Bs(_) => "BS",
            Attr::M(_) => "M",
            Attr::L(_) => "L",
            Attr::Null => "NULL",
            Attr::Bool(_) => "BOOL",
        }
    }

    pub fn long_type_name(&self) -> &'static str {
        match self {
            Attr::S(_) => "STRING",
            Attr::N(_) => "NUMBER",
            Attr::B(_) => "BINARY",
            Attr::Ss(_) => "STRING_SET",
            Attr::Ns(_) => "NUMBER_SET",
            Attr::Bs(_) => "BINARY_SET",
            Attr::M(_) => "MAP",
            Attr::L(_) => "LIST",
            Attr::Null => "NULL",
            Attr::Bool(_) => "BOOLEAN",
        }
    }

    pub fn num(&self) -> Option<Num> {
        match self {
            Attr::N(n) => Num::parse(n).ok(),
            _ => None,
        }
    }

    pub fn compare(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (Attr::S(a), Attr::S(b)) => Some(a.as_bytes().cmp(b.as_bytes())),
            (Attr::N(_), Attr::N(_)) => Some(self.num()?.cmp(&other.num()?)),
            (Attr::B(a), Attr::B(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }

    pub fn equals(&self, other: &Self) -> bool {
        match (self, other) {
            (Attr::Ss(a), Attr::Ss(b)) | (Attr::Ns(a), Attr::Ns(b)) => same_members(a, b),
            (Attr::Bs(a), Attr::Bs(b)) => same_members(a, b),
            (Attr::M(a), Attr::M(b)) => {
                a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).is_some_and(|w| v.equals(w)))
            }
            (Attr::L(a), Attr::L(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.equals(y))
            }
            _ => self == other,
        }
    }

    pub fn size(&self) -> Option<usize> {
        Some(match self {
            Attr::S(s) => s.chars().count(),
            Attr::B(b) => b.len(),
            Attr::Ss(v) | Attr::Ns(v) => v.len(),
            Attr::Bs(v) => v.len(),
            Attr::L(v) => v.len(),
            Attr::M(m) => m.len(),
            Attr::N(_) | Attr::Null | Attr::Bool(_) => return None,
        })
    }

    pub fn contains(&self, needle: &Self) -> bool {
        match (self, needle) {
            (Attr::S(h), Attr::S(n)) => h.contains(n.as_str()),
            (Attr::Ss(h), Attr::S(n)) | (Attr::Ns(h), Attr::N(n)) => h.contains(n),
            (Attr::Bs(h), Attr::B(n)) => h.contains(n),
            (Attr::B(h), Attr::B(n)) => n.is_empty() || h.windows(n.len()).any(|w| w == n),
            (Attr::L(h), n) => h.iter().any(|x| x.equals(n)),
            _ => false,
        }
    }

    pub fn begins_with(&self, prefix: &Self) -> bool {
        match (self, prefix) {
            (Attr::S(v), Attr::S(p)) => v.starts_with(p.as_str()),
            (Attr::B(v), Attr::B(p)) => v.starts_with(p),
            _ => false,
        }
    }

    pub fn is_set(&self) -> bool {
        matches!(self, Attr::Ss(_) | Attr::Ns(_) | Attr::Bs(_))
    }

    pub fn union(&self, other: &Self) -> Option<Self> {
        fn merge<T: Clone + PartialEq>(a: &[T], b: &[T]) -> Vec<T> {
            let mut out = a.to_vec();
            for x in b {
                if !out.contains(x) {
                    out.push(x.clone());
                }
            }
            out
        }
        Some(match (self, other) {
            (Attr::Ss(a), Attr::Ss(b)) => Attr::Ss(merge(a, b)),
            (Attr::Ns(a), Attr::Ns(b)) => Attr::Ns(merge(a, b)),
            (Attr::Bs(a), Attr::Bs(b)) => Attr::Bs(merge(a, b)),
            _ => return None,
        })
    }

    pub fn difference(&self, other: &Self) -> Option<Option<Self>> {
        fn minus<T: Clone + PartialEq>(a: &[T], b: &[T]) -> Vec<T> {
            a.iter().filter(|x| !b.contains(x)).cloned().collect()
        }
        let out = match (self, other) {
            (Attr::Ss(a), Attr::Ss(b)) => Attr::Ss(minus(a, b)),
            (Attr::Ns(a), Attr::Ns(b)) => Attr::Ns(minus(a, b)),
            (Attr::Bs(a), Attr::Bs(b)) => Attr::Bs(minus(a, b)),
            _ => return None,
        };
        Some((out.size() != Some(0)).then_some(out))
    }

    pub fn byte_size(&self) -> usize {
        let number = |n: &str| {
            let digits = n.trim_start_matches(['-', '0', '.']);
            let digits = digits.trim_end_matches(['0', '.']);
            let count = digits.bytes().filter(u8::is_ascii_digit).count();
            count.div_ceil(2) + 1
        };
        match self {
            Attr::S(s) => s.len(),
            Attr::N(n) => number(n),
            Attr::B(b) => b.len(),
            Attr::Ss(v) => v.iter().map(String::len).sum(),
            Attr::Ns(v) => v.iter().map(|n| number(n)).sum(),
            Attr::Bs(v) => v.iter().map(Vec::len).sum(),
            Attr::M(m) => {
                3 + m
                    .iter()
                    .map(|(k, v)| k.len() + v.byte_size() + 1)
                    .sum::<usize>()
            }
            Attr::L(l) => 3 + l.iter().map(|v| v.byte_size() + 1).sum::<usize>(),
            Attr::Null | Attr::Bool(_) => 1,
        }
    }
}

fn same_members<T: PartialEq>(a: &[T], b: &[T]) -> bool {
    a.len() == b.len() && a.iter().all(|x| b.contains(x))
}

fn check_set<T: PartialEq>(kind: &str, items: &[T]) -> Result<(), AwsError> {
    if items.is_empty() {
        return Err(invalid(format!("An {kind} set  may not be empty")));
    }
    for (i, a) in items.iter().enumerate() {
        if items[i + 1..].contains(a) {
            return Err(invalid("Input collection contains duplicates."));
        }
    }
    Ok(())
}

pub fn item_from_value(v: &Value) -> Result<Item, AwsError> {
    v.as_object()
        .into_iter()
        .flatten()
        .map(|(k, x)| Ok((k.clone(), Attr::from_value(x)?)))
        .collect()
}

pub fn item_to_value(item: &Item) -> Value {
    Value::Object(
        item.iter()
            .map(|(k, v)| (k.clone(), v.to_value()))
            .collect(),
    )
}

pub fn item_size(item: &Item) -> usize {
    item.iter().map(|(k, v)| k.len() + v.byte_size()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn a(v: Value) -> Attr {
        Attr::from_value(&v).unwrap()
    }

    #[test]
    fn round_trips_every_type() {
        let v = json!({"M": {"s": {"S": "x"}, "n": {"N": "1.50"}, "b": {"B": "AAH/"}, "ss": {"SS": ["a", "b"]},
            "ns": {"NS": ["1", "2"]}, "bs": {"BS": ["AA=="]}, "l": {"L": [{"NULL": true}, {"BOOL": false}]}}});
        let attr = a(v);
        let back = attr.to_value();
        assert_eq!(back["M"]["n"], json!({"N": "1.5"}));
        assert_eq!(back["M"]["b"], json!({"B": "AAH/"}));
        assert_eq!(Attr::from_value(&back).unwrap(), attr);
    }

    #[test]
    fn rejects_malformed_values() {
        for v in [
            json!({}),
            json!({"S": "a", "N": "1"}),
            json!({"SS": []}),
            json!({"SS": ["a", "a"]}),
            json!({"NS": ["1", "1.0"]}),
            json!({"N": "x"}),
            json!({"NULL": false}),
        ] {
            assert!(Attr::from_value(&v).is_err(), "{v}");
        }
    }

    #[test]
    fn compares_and_tests_membership() {
        assert_eq!(
            a(json!({"N": "10"})).compare(&a(json!({"N": "9"}))),
            Some(Ordering::Greater)
        );
        assert_eq!(a(json!({"S": "a"})).compare(&a(json!({"N": "1"}))), None);
        assert!(a(json!({"NS": ["1", "2"]})).equals(&a(json!({"NS": ["2", "1"]}))));
        assert!(a(json!({"L": [{"S": "x"}]})).contains(&a(json!({"S": "x"}))));
        assert!(a(json!({"S": "hello"})).begins_with(&a(json!({"S": "he"}))));
        assert_eq!(
            a(json!({"SS": ["a", "b"]})).difference(&a(json!({"SS": ["a", "b"]}))),
            Some(None)
        );
    }
}
