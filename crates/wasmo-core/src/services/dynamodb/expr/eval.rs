use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::codec::AwsError;

use super::super::attr::{Attr, Item};
use super::super::validation;
use super::{CmpOp, Cond, Operand, Path, SetOperand, SetValue, Step, Update};

pub fn get<'a>(item: &'a Item, path: &Path) -> Option<&'a Attr> {
    let mut cur = item.get(&path.root)?;
    for step in &path.steps {
        cur = match (step, cur) {
            (Step::Field(f), Attr::M(m)) => m.get(f)?,
            (Step::Index(i), Attr::L(l)) => l.get(*i)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn operand(op: &Operand, item: &Item) -> Option<Attr> {
    match op {
        Operand::Value(v) => Some(v.clone()),
        Operand::Path(p) => get(item, p).cloned(),
        Operand::Size(p) => Some(Attr::N(get(item, p)?.size()?.to_string())),
    }
}

fn compare_ok(ord: Option<Ordering>, op: CmpOp) -> bool {
    let Some(o) = ord else {
        return false;
    };
    match op {
        CmpOp::Eq => o == Ordering::Equal,
        CmpOp::Ne => o != Ordering::Equal,
        CmpOp::Lt => o == Ordering::Less,
        CmpOp::Le => o != Ordering::Greater,
        CmpOp::Gt => o == Ordering::Greater,
        CmpOp::Ge => o != Ordering::Less,
    }
}

pub fn matches(cond: &Cond, item: &Item) -> bool {
    match cond {
        Cond::And(a, b) => matches(a, item) && matches(b, item),
        Cond::Or(a, b) => matches(a, item) || matches(b, item),
        Cond::Not(c) => !matches(c, item),
        Cond::Cmp(op, l, r) => {
            let (Some(l), Some(r)) = (operand(l, item), operand(r, item)) else {
                return false;
            };
            match op {
                CmpOp::Eq => l.equals(&r),
                CmpOp::Ne => !l.equals(&r),
                _ => compare_ok(l.compare(&r), *op),
            }
        }
        Cond::Between(t, lo, hi) => {
            let (Some(t), Some(lo), Some(hi)) =
                (operand(t, item), operand(lo, item), operand(hi, item))
            else {
                return false;
            };
            compare_ok(t.compare(&lo), CmpOp::Ge) && compare_ok(t.compare(&hi), CmpOp::Le)
        }
        Cond::In(t, list) => operand(t, item).is_some_and(|t| {
            list.iter()
                .any(|c| operand(c, item).is_some_and(|c| t.equals(&c)))
        }),
        Cond::Exists(p) => get(item, p).is_some(),
        Cond::NotExists(p) => get(item, p).is_none(),
        Cond::Type(p, t) => get(item, p).is_some_and(|v| v.type_name() == t),
        Cond::BeginsWith(p, prefix) => match (get(item, p), operand(prefix, item)) {
            (Some(v), Some(prefix)) => v.begins_with(&prefix),
            _ => false,
        },
        Cond::Contains(p, needle) => match (get(item, p), operand(needle, item)) {
            (Some(v), Some(needle)) => v.contains(&needle),
            _ => false,
        },
    }
}

fn invalid_path() -> AwsError {
    validation("The document path provided in the update expression is invalid for update")
}

fn missing_attribute() -> AwsError {
    validation("The provided expression refers to an attribute that does not exist in the item")
}

fn wrong_operand(op: &str, v: &Attr) -> AwsError {
    validation(format!(
        "Invalid UpdateExpression: Incorrect operand type for operator or function; operator: {op}, operand type: {}",
        v.long_type_name()
    ))
}

fn set_operand(op: &SetOperand, item: &Item) -> Result<Attr, AwsError> {
    match op {
        SetOperand::Value(v) => Ok(v.clone()),
        SetOperand::Path(p) => get(item, p).cloned().ok_or_else(missing_attribute),
        SetOperand::IfNotExists(p, default) => match get(item, p) {
            Some(v) => Ok(v.clone()),
            None => set_operand(default, item),
        },
        SetOperand::ListAppend(a, b) => {
            let a = set_operand(a, item)?;
            let b = set_operand(b, item)?;
            match (a, b) {
                (Attr::L(mut a), Attr::L(b)) => {
                    a.extend(b);
                    Ok(Attr::L(a))
                }
                (Attr::L(_), other) | (other, _) => Err(wrong_operand("list_append", &other)),
            }
        }
    }
}

fn arithmetic(l: Attr, r: Attr, plus: bool) -> Result<Attr, AwsError> {
    let op = if plus { "+" } else { "-" };
    let (Some(a), Some(b)) = (l.num(), r.num()) else {
        let bad = if l.num().is_none() { &l } else { &r };
        return Err(wrong_operand(op, bad));
    };
    let n = if plus { a.add(&b)? } else { a.sub(&b)? };
    Ok(Attr::N(n.to_string()))
}

fn set_value(v: &SetValue, item: &Item) -> Result<Attr, AwsError> {
    match v {
        SetValue::Operand(o) => set_operand(o, item),
        SetValue::Plus(a, b) => arithmetic(set_operand(a, item)?, set_operand(b, item)?, true),
        SetValue::Minus(a, b) => arithmetic(set_operand(a, item)?, set_operand(b, item)?, false),
    }
}

pub fn set_path(item: &mut Item, path: &Path, value: Attr) -> Result<(), AwsError> {
    let Some((last, parents)) = path.steps.split_last() else {
        item.insert(path.root.clone(), value);
        return Ok(());
    };
    let mut cur = item.get_mut(&path.root).ok_or_else(invalid_path)?;
    for step in parents {
        cur = match (step, cur) {
            (Step::Field(f), Attr::M(m)) => m.get_mut(f).ok_or_else(invalid_path)?,
            (Step::Index(i), Attr::L(l)) => l.get_mut(*i).ok_or_else(invalid_path)?,
            _ => return Err(invalid_path()),
        };
    }
    match (last, cur) {
        (Step::Field(f), Attr::M(m)) => {
            m.insert(f.clone(), value);
        }
        (Step::Index(i), Attr::L(l)) => {
            if *i < l.len() {
                l[*i] = value;
            } else {
                l.push(value);
            }
        }
        _ => return Err(invalid_path()),
    }
    Ok(())
}

pub fn remove_path(item: &mut Item, path: &Path) {
    let Some((last, parents)) = path.steps.split_last() else {
        item.remove(&path.root);
        return;
    };
    let Some(mut cur) = item.get_mut(&path.root) else {
        return;
    };
    for step in parents {
        let next = match (step, cur) {
            (Step::Field(f), Attr::M(m)) => m.get_mut(f),
            (Step::Index(i), Attr::L(l)) => l.get_mut(*i),
            _ => None,
        };
        let Some(next) = next else {
            return;
        };
        cur = next;
    }
    match (last, cur) {
        (Step::Field(f), Attr::M(m)) => {
            m.remove(f);
        }
        (Step::Index(i), Attr::L(l)) if *i < l.len() => {
            l.remove(*i);
        }
        _ => {}
    }
}

/// Applies an update expression. Every action reads the item as it was
/// before the update.
pub fn apply(update: &Update, item: &Item) -> Result<Item, AwsError> {
    let mut state = item.clone();
    for (path, value) in &update.set {
        let v = set_value(value, item)?;
        set_path(&mut state, path, v)?;
    }
    let mut removals: Vec<&Path> = update.remove.iter().collect();
    removals.sort_by(|a, b| b.cmp(a));
    for path in removals {
        remove_path(&mut state, path);
    }
    for (path, value) in &update.add {
        let next = match (get(item, path), value) {
            (None, Attr::N(_)) => value.clone(),
            (None, v) if v.is_set() => v.clone(),
            (Some(cur @ Attr::N(_)), Attr::N(_)) => arithmetic(cur.clone(), value.clone(), true)?,
            (Some(cur), v) if v.is_set() => cur.union(v).ok_or_else(|| {
                validation("An operand in the update expression has an incorrect data type")
            })?,
            (Some(_), Attr::N(_)) => {
                return Err(validation(
                    "An operand in the update expression has an incorrect data type",
                ));
            }
            (_, v) => return Err(wrong_operand("ADD", v)),
        };
        set_path(&mut state, path, next)?;
    }
    for (path, value) in &update.delete {
        if !value.is_set() {
            return Err(wrong_operand("DELETE", value));
        }
        let Some(cur) = get(item, path) else {
            continue;
        };
        match cur.difference(value) {
            Some(Some(rest)) => set_path(&mut state, path, rest)?,
            Some(None) => remove_path(&mut state, path),
            None => {
                return Err(validation(
                    "An operand in the update expression has an incorrect data type",
                ));
            }
        }
    }
    Ok(state)
}

#[derive(Default)]
struct Node {
    leaf: bool,
    fields: BTreeMap<String, Node>,
    indexes: BTreeMap<usize, Node>,
}

fn project_attr(v: &Attr, node: &Node) -> Option<Attr> {
    if node.leaf {
        return Some(v.clone());
    }
    match v {
        Attr::M(m) if !node.fields.is_empty() => {
            let out: Item = node
                .fields
                .iter()
                .filter_map(|(k, n)| Some((k.clone(), project_attr(m.get(k)?, n)?)))
                .collect();
            (!out.is_empty()).then_some(Attr::M(out))
        }
        Attr::L(l) if !node.indexes.is_empty() => {
            let out: Vec<Attr> = node
                .indexes
                .iter()
                .filter_map(|(i, n)| project_attr(l.get(*i)?, n))
                .collect();
            (!out.is_empty()).then_some(Attr::L(out))
        }
        _ => None,
    }
}

pub fn project(item: &Item, paths: &[Path]) -> Item {
    let mut roots: BTreeMap<String, Node> = BTreeMap::new();
    for p in paths {
        let mut node = roots.entry(p.root.clone()).or_default();
        for step in &p.steps {
            node = match step {
                Step::Field(f) => node.fields.entry(f.clone()).or_default(),
                Step::Index(i) => node.indexes.entry(*i).or_default(),
            };
        }
        node.leaf = true;
    }
    roots
        .iter()
        .filter_map(|(k, n)| Some((k.clone(), project_attr(item.get(k)?, n)?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::{Bindings, parse_condition, parse_projection, parse_update};
    use super::*;
    use crate::services::dynamodb::attr::{item_from_value, item_to_value};
    use serde_json::{Value, json};

    fn item(v: Value) -> Item {
        item_from_value(&v).unwrap()
    }

    fn bindings(values: Value) -> Bindings {
        Bindings::from_input(&json!({ "ExpressionAttributeValues": values })).unwrap()
    }

    fn check(expr: &str, values: Value, it: &Item) -> bool {
        let mut b = bindings(values);
        matches(
            &parse_condition("ConditionExpression", expr, &mut b).unwrap(),
            it,
        )
    }

    fn update(expr: &str, values: Value, it: &Item) -> Result<Value, AwsError> {
        let mut b = bindings(values);
        let u = parse_update(expr, &mut b)?;
        Ok(item_to_value(&apply(&u, it)?))
    }

    #[test]
    fn evaluates_conditions() {
        let it = item(
            json!({"n": {"N": "5"}, "s": {"S": "hello"}, "tags": {"SS": ["a", "b"]},
            "m": {"M": {"l": {"L": [{"N": "1"}, {"S": "x"}]}}}}),
        );
        let v = json!({":three": {"N": "3"}, ":ten": {"N": "10"}, ":he": {"S": "he"}, ":a": {"S": "a"}, ":x": {"S": "x"}, ":ty": {"S": "L"}});
        assert!(check("n BETWEEN :three AND :ten", v.clone(), &it));
        assert!(check(
            "n IN (:ten, :three) OR begins_with(s, :he)",
            v.clone(),
            &it
        ));
        assert!(check(
            "contains(tags, :a) AND contains(m.l, :x)",
            v.clone(),
            &it
        ));
        assert!(check(
            "size(s) > :three AND attribute_type(m.l, :ty)",
            v.clone(),
            &it
        ));
        assert!(check(
            "attribute_not_exists(absent) AND NOT (n < :three)",
            v.clone(),
            &it
        ));
        assert!(!check("absent <> :three", v.clone(), &it));
        assert!(check("m.l[1] = :x", v.clone(), &it));
        assert!(!check("s < :three", v, &it));
    }

    #[test]
    fn applies_update_actions_against_the_original_item() {
        let it = item(
            json!({"a": {"N": "1"}, "b": {"N": "2"}, "l": {"L": [{"N": "0"}, {"N": "1"}, {"N": "2"}, {"N": "3"}]},
            "ns": {"NS": ["1", "2"]}}),
        );
        let out = update(
            "SET a = b, b = a + :inc, c = if_not_exists(c, :z), l = list_append(l, :more) REMOVE l[0], l[2] ADD ns :ns, cnt :inc DELETE ns :rm",
            json!({":inc": {"N": "0.5"}, ":z": {"S": "zero"}, ":more": {"L": [{"N": "9"}]}, ":ns": {"NS": ["3"]}, ":rm": {"NS": ["1"]}}),
            &it,
        );
        assert!(out.is_err(), "l is both SET and REMOVEd: {out:?}");
        let out = update(
            "SET a = b, b = a + :inc, c = if_not_exists(c, :z) REMOVE l[0], l[2] ADD cnt :inc DELETE ns :rm",
            json!({":inc": {"N": "0.5"}, ":z": {"S": "zero"}, ":rm": {"NS": ["1"]}}),
            &it,
        )
        .unwrap();
        assert_eq!(out["a"], json!({"N": "2"}));
        assert_eq!(out["b"], json!({"N": "1.5"}));
        assert_eq!(out["c"], json!({"S": "zero"}));
        assert_eq!(out["l"], json!({"L": [{"N": "1"}, {"N": "3"}]}));
        assert_eq!(out["cnt"], json!({"N": "0.5"}));
        assert_eq!(out["ns"], json!({"NS": ["2"]}));
    }

    #[test]
    fn update_errors_match_dynamodb() {
        let it = item(json!({"s": {"S": "x"}, "m": {"M": {}}, "ss": {"SS": ["a"]}}));
        let e = update("SET n = absent + :one", json!({":one": {"N": "1"}}), &it).unwrap_err();
        assert!(e.message.contains("does not exist"), "{}", e.message);
        let e = update("SET n = s + :one", json!({":one": {"N": "1"}}), &it).unwrap_err();
        assert!(e.message.contains("operator: +"), "{}", e.message);
        let e = update("SET x.y = :one", json!({":one": {"N": "1"}}), &it).unwrap_err();
        assert!(e.message.contains("document path"), "{}", e.message);
        let e = update(
            "SET l = list_append(s, :one)",
            json!({":one": {"L": []}}),
            &it,
        )
        .unwrap_err();
        assert!(e.message.contains("list_append"), "{}", e.message);
        let out = update(
            "SET m.k = :one, l2 = :l, s2 = :one DELETE ss :ss",
            json!({":one": {"N": "1"}, ":l": {"L": []}, ":ss": {"SS": ["a"]}}),
            &it,
        )
        .unwrap();
        assert_eq!(out["m"], json!({"M": {"k": {"N": "1"}}}));
        assert!(out.get("ss").is_none());
        let appended = update(
            "SET l2[5] = :one",
            json!({":one": {"N": "1"}}),
            &item(json!({"l2": {"L": []}})),
        )
        .unwrap();
        assert_eq!(appended["l2"], json!({"L": [{"N": "1"}]}));
    }

    #[test]
    fn projects_paths_and_compacts_lists() {
        let it = item(
            json!({"a": {"L": [{"N": "0"}, {"N": "1"}, {"N": "2"}, {"N": "3"}]},
            "m": {"M": {"x": {"S": "x"}, "y": {"S": "y"}}}, "z": {"S": "z"}}),
        );
        let mut b = Bindings::default();
        let paths = parse_projection("a[2], a[0], m.y, nope.deep", &mut b).unwrap();
        let out = item_to_value(&project(&it, &paths));
        assert_eq!(
            out,
            json!({"a": {"L": [{"N": "0"}, {"N": "2"}]}, "m": {"M": {"y": {"S": "y"}}}})
        );
    }
}
