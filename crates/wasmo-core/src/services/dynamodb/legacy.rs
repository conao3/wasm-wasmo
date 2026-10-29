use serde_json::{Map, Value};

use crate::codec::AwsError;

use super::attr::{Attr, Item};
use super::expr::eval::{get, remove_path, set_path};
use super::expr::{CmpOp, Cond, Operand, Path, RangeCond};
use super::validation;

fn invalid(message: impl std::fmt::Display) -> AwsError {
    validation(format!(
        "One or more parameter values were invalid: {message}"
    ))
}

fn values(entry: &Value) -> Result<Vec<Attr>, AwsError> {
    entry
        .get("AttributeValueList")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(Attr::from_value)
        .collect()
}

fn comparison(attr: &str, op: &str, mut list: Vec<Attr>) -> Result<Cond, AwsError> {
    let arity = |n: usize, list: &[Attr]| {
        if list.len() == n {
            Ok(())
        } else {
            Err(invalid(format!(
                "Invalid number of argument(s) for the {op} ComparisonOperator"
            )))
        }
    };
    let path = || Path::top(attr);
    let cmp = |o: CmpOp, list: &mut Vec<Attr>| {
        Cond::Cmp(o, Operand::Path(path()), Operand::Value(list.remove(0)))
    };
    Ok(match op {
        "EQ" | "NE" | "LE" | "LT" | "GE" | "GT" | "CONTAINS" | "NOT_CONTAINS" | "BEGINS_WITH" => {
            arity(1, &list)?;
            match op {
                "EQ" => cmp(CmpOp::Eq, &mut list),
                "NE" => Cond::Not(Box::new(cmp(CmpOp::Eq, &mut list))),
                "LE" => cmp(CmpOp::Le, &mut list),
                "LT" => cmp(CmpOp::Lt, &mut list),
                "GE" => cmp(CmpOp::Ge, &mut list),
                "GT" => cmp(CmpOp::Gt, &mut list),
                "CONTAINS" => Cond::Contains(path(), Operand::Value(list.remove(0))),
                "NOT_CONTAINS" => Cond::And(
                    Box::new(Cond::Exists(path())),
                    Box::new(Cond::Not(Box::new(Cond::Contains(
                        path(),
                        Operand::Value(list.remove(0)),
                    )))),
                ),
                _ => Cond::BeginsWith(path(), Operand::Value(list.remove(0))),
            }
        }
        "NULL" => {
            arity(0, &list)?;
            Cond::NotExists(path())
        }
        "NOT_NULL" => {
            arity(0, &list)?;
            Cond::Exists(path())
        }
        "BETWEEN" => {
            arity(2, &list)?;
            let hi = list.pop().expect("two values");
            let lo = list.pop().expect("two values");
            Cond::Between(
                Operand::Path(path()),
                Operand::Value(lo),
                Operand::Value(hi),
            )
        }
        "IN" => {
            if list.is_empty() {
                arity(1, &list)?;
            }
            Cond::In(
                Operand::Path(path()),
                list.into_iter().map(Operand::Value).collect(),
            )
        }
        other => {
            return Err(validation(format!(
                "1 validation error detected: Value '{other}' at 'comparisonOperator' failed to satisfy constraint: Member must satisfy enum value set: [IN, NULL, BETWEEN, LT, NOT_CONTAINS, EQ, GT, NOT_NULL, NE, LE, BEGINS_WITH, GE, CONTAINS]"
            )));
        }
    })
}

fn combine(conds: Vec<Cond>, operator: Option<&str>) -> Option<Cond> {
    let or = operator == Some("OR");
    conds.into_iter().reduce(|a, b| {
        if or {
            Cond::Or(Box::new(a), Box::new(b))
        } else {
            Cond::And(Box::new(a), Box::new(b))
        }
    })
}

fn operator(input: &Value) -> Option<&str> {
    input.get("ConditionalOperator").and_then(Value::as_str)
}

pub fn expected(input: &Value) -> Result<Option<Cond>, AwsError> {
    let Some(map) = input.get("Expected").and_then(Value::as_object) else {
        return Ok(None);
    };
    let mut conds = Vec::new();
    for (attr, entry) in map {
        let value = entry.get("Value").map(Attr::from_value).transpose()?;
        let exists = entry.get("Exists").and_then(Value::as_bool);
        conds.push(if let Some(op) = entry.get("ComparisonOperator").and_then(Value::as_str) {
            if value.is_some() || exists.is_some() {
                return Err(invalid(format!(
                    "Value or Exists cannot be used with ComparisonOperator for Attribute: {attr}"
                )));
            }
            comparison(attr, op, values(entry)?)?
        } else {
            match (exists, value) {
                (Some(false), None) => Cond::NotExists(Path::top(attr)),
                (Some(false), Some(_)) => {
                    return Err(invalid(format!(
                        "Value cannot be used when Exists is false for Attribute: {attr}"
                    )));
                }
                (_, Some(v)) => Cond::Cmp(
                    CmpOp::Eq,
                    Operand::Path(Path::top(attr)),
                    Operand::Value(v),
                ),
                (_, None) => {
                    return Err(invalid(format!(
                        "Value must be provided when Exists is true for Attribute: {attr}"
                    )));
                }
            }
        });
    }
    Ok(combine(conds, operator(input)))
}

pub fn filter(input: &Value, key: &str) -> Result<Option<Cond>, AwsError> {
    let Some(map) = input.get(key).and_then(Value::as_object) else {
        return Ok(None);
    };
    let mut conds = Vec::new();
    for (attr, entry) in map {
        let op = entry
            .get("ComparisonOperator")
            .and_then(Value::as_str)
            .unwrap_or_default();
        conds.push(comparison(attr, op, values(entry)?)?);
    }
    Ok(combine(conds, operator(input)))
}

pub fn key_conditions(map: &Map<String, Value>) -> Result<Vec<(String, RangeCond)>, AwsError> {
    let mut out = Vec::new();
    for (attr, entry) in map {
        let op = entry
            .get("ComparisonOperator")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut list = values(entry)?;
        let need = if op == "BETWEEN" { 2 } else { 1 };
        if list.len() != need {
            return Err(invalid(format!(
                "Invalid number of argument(s) for the {op} ComparisonOperator"
            )));
        }
        let first = list.remove(0);
        let cond = match op {
            "EQ" => RangeCond::Cmp(CmpOp::Eq, first),
            "LE" => RangeCond::Cmp(CmpOp::Le, first),
            "LT" => RangeCond::Cmp(CmpOp::Lt, first),
            "GE" => RangeCond::Cmp(CmpOp::Ge, first),
            "GT" => RangeCond::Cmp(CmpOp::Gt, first),
            "BEGINS_WITH" => RangeCond::BeginsWith(first),
            "BETWEEN" => RangeCond::Between(first, list.remove(0)),
            other => {
                return Err(validation(format!(
                    "Unsupported operator on KeyCondition: {other}"
                )));
            }
        };
        out.push((attr.clone(), cond));
    }
    Ok(out)
}

pub fn attributes_to_get(input: &Value) -> Option<Vec<Path>> {
    let list = input.get("AttributesToGet")?.as_array()?;
    Some(
        list.iter()
            .filter_map(Value::as_str)
            .map(Path::top)
            .collect(),
    )
}

pub fn attribute_updates(
    map: &Map<String, Value>,
    item: &Item,
) -> Result<(Item, Vec<Path>), AwsError> {
    let mut state = item.clone();
    let mut paths = Vec::new();
    for (attr, update) in map {
        let path = Path::top(attr);
        let value = update.get("Value").map(Attr::from_value).transpose()?;
        let action = update
            .get("Action")
            .and_then(Value::as_str)
            .unwrap_or("PUT");
        let current = get(item, &path);
        match (action, value) {
            ("PUT", Some(v)) => set_path(&mut state, &path, v)?,
            ("PUT", None) => {
                return Err(invalid(format!(
                    "Only DELETE action is allowed when no attribute value is specified: {attr}"
                )));
            }
            ("DELETE", None) => remove_path(&mut state, &path),
            ("DELETE", Some(v)) => {
                if !v.is_set() {
                    return Err(invalid(format!(
                        "DELETE action with value is not supported for the type {}",
                        v.long_type_name()
                    )));
                }
                if let Some(cur) = current {
                    match cur.difference(&v) {
                        Some(Some(rest)) => set_path(&mut state, &path, rest)?,
                        Some(None) => remove_path(&mut state, &path),
                        None => return Err(invalid("Type mismatch for attribute to update")),
                    }
                }
            }
            ("ADD", Some(v)) => {
                let next = match (current, &v) {
                    (None, _) if matches!(v, Attr::N(_) | Attr::L(_)) || v.is_set() => v.clone(),
                    (Some(Attr::N(a)), Attr::N(b)) => {
                        let a = Attr::N(a.clone()).num().expect("stored number");
                        let b = Attr::N(b.clone()).num().expect("validated number");
                        Attr::N(a.add(&b)?.to_string())
                    }
                    (Some(Attr::L(a)), Attr::L(b)) => {
                        let mut l = a.clone();
                        l.extend(b.iter().cloned());
                        Attr::L(l)
                    }
                    (Some(cur), v) if v.is_set() => cur
                        .union(v)
                        .ok_or_else(|| invalid("Type mismatch for attribute to update"))?,
                    _ => {
                        return Err(invalid(format!(
                            "ADD action is not supported for the type {}",
                            v.long_type_name()
                        )));
                    }
                };
                set_path(&mut state, &path, next)?;
            }
            ("ADD", None) => {
                return Err(invalid(format!(
                    "Only DELETE action is allowed when no attribute value is specified: {attr}"
                )));
            }
            (other, _) => {
                return Err(validation(format!(
                    "1 validation error detected: Value '{other}' at 'attributeUpdates.{attr}.member.action' failed to satisfy constraint: Member must satisfy enum value set: [ADD, PUT, DELETE]"
                )));
            }
        }
        paths.push(path);
    }
    Ok((state, paths))
}
